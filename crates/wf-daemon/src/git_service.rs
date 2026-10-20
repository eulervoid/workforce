//! Commits between devices over `wf/git/1`: this daemon serves `git upload-pack` on the bare
//! repository of a workforce, and fetches the commits it lacks from other devices through a
//! bridge, a small program that `git fetch` runs and that pipes to a socket of this daemon.

use std::path::Path;
use std::sync::{Arc, Weak};

use n0_future::boxed::BoxFuture;
use tokio::net::UnixListener;
use wf_core::id::WorkforceId;
use wf_core::identity::DeviceKey;
use wf_core::session::CommitId;
use wf_net::{GitProcess, GitSource, NetError, SourceError};
use wf_vcs::{GitCli, VcsError};

use crate::Daemon;

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("this daemon has no `wf` binary to bridge git with")]
    NoBridge,
    #[error(transparent)]
    Net(#[from] NetError),
    #[error("{0}")]
    Git(#[from] VcsError),
    #[error("could not set up the bridge: {0}")]
    Io(#[from] std::io::Error),
    #[error("a background task did not finish: {0}")]
    Task(#[from] tokio::task::JoinError),
}

#[derive(Debug, thiserror::Error)]
enum ServeError {
    #[error("the daemon is shutting down")]
    Gone,
    #[error("workforce {0} has no repository here")]
    NoRepository(WorkforceId),
    #[error("`git upload-pack` failed: {0}")]
    Exit(std::process::ExitStatus),
}

/// Answers `wf/git/1` with `git upload-pack` on the bare repositories of this machine.
pub(crate) struct UploadPack(Weak<Daemon>);

impl UploadPack {
    pub(crate) fn new(daemon: &Arc<Daemon>) -> Arc<Self> {
        Arc::new(Self(Arc::downgrade(daemon)))
    }
}

impl GitSource for UploadPack {
    fn upload_pack(&self, workforce: WorkforceId) -> BoxFuture<Result<GitProcess, SourceError>> {
        let daemon = self.0.clone();
        Box::pin(async move {
            let daemon = daemon.upgrade().ok_or(ServeError::Gone)?;
            let repository = daemon.bare_repository(workforce);
            if !repository.join("HEAD").is_file() {
                return Err(ServeError::NoRepository(workforce).into());
            }
            let mut child =
                tokio::task::spawn_blocking(move || GitCli::open(&repository)?.spawn_upload_pack())
                    .await??;
            let input = tokio::process::ChildStdin::from_std(child.stdin.take().expect("piped"))?;
            let output =
                tokio::process::ChildStdout::from_std(child.stdout.take().expect("piped"))?;
            let finished = async move {
                let status = tokio::task::spawn_blocking(move || child.wait()).await??;
                if status.success() {
                    Ok(())
                } else {
                    Err(ServeError::Exit(status).into())
                }
            };
            Ok(GitProcess {
                input: Box::new(input),
                output: Box::new(output),
                finished: Box::pin(finished),
            })
        })
    }
}

impl Daemon {
    /// Builds the bare repository of a workforce just joined from the device it was joined
    /// through: its target and item branches. A failure is only logged: the commits the log names
    /// are fetched from the devices as they are met, whatever happens here.
    pub(crate) async fn build_repository_from(
        self: &Arc<Self>,
        workforce: WorkforceId,
        device: DeviceKey,
        storage: &crate::StorageHandle,
    ) {
        self.ensure_bare_repository(workforce, storage).await;
        if let Err(error) = self.fetch_branches_from(workforce, device).await {
            tracing::warn!(
                "could not build the repository of workforce {workforce} from {}: {}",
                device.fmt_short(),
                wf_core::error_chain(&error)
            );
        }
    }

    /// Fetches `commit` of `workforce` from `device` into the bare repository.
    pub(crate) async fn fetch_commit_from(
        self: &Arc<Self>,
        workforce: WorkforceId,
        device: DeviceKey,
        commit: CommitId,
    ) -> Result<(), FetchError> {
        self.fetch_through_bridge(workforce, device, move |git, bridge| {
            git.fetch_commit(&bridge, commit)
        })
        .await
    }

    /// Fetches the branches of `device` into the bare repository of `workforce`: what a device
    /// that has just joined through it starts with.
    pub(crate) async fn fetch_branches_from(
        self: &Arc<Self>,
        workforce: WorkforceId,
        device: DeviceKey,
    ) -> Result<(), FetchError> {
        self.fetch_through_bridge(workforce, device, |git, bridge| {
            git.fetch_branches_through(&bridge)
        })
        .await
    }

    /// Runs `fetch`, a `git fetch` in the bare repository of `workforce` through the bridge it is
    /// given, while this side pipes the bridge over iroh to `device`. `git fetch` runs the `wf`
    /// binary of this daemon as its `ext::` bridge, which pipes to a socket in a private
    /// directory.
    async fn fetch_through_bridge(
        self: &Arc<Self>,
        workforce: WorkforceId,
        device: DeviceKey,
        fetch: impl FnOnce(GitCli, Vec<String>) -> Result<(), VcsError> + Send + 'static,
    ) -> Result<(), FetchError> {
        let binary = self
            .bin_dir()
            .map(|directory| directory.join("wf"))
            .ok_or(FetchError::NoBridge)?;
        let node = self.node().await?;
        // A short path, as socket paths have little room; the directory is private to this daemon.
        let directory = tempfile::Builder::new().prefix("wf-git-").tempdir()?;
        let socket = directory.path().join("bridge");
        let listener = UnixListener::bind(&socket)?;
        let bridge = vec![
            binary.display().to_string(),
            "git-bridge".to_string(),
            socket.display().to_string(),
        ];
        let repository = self.bare_repository(workforce);
        let mut fetch =
            tokio::task::spawn_blocking(move || fetch(GitCli::open(&repository)?, bridge));
        let relay = async {
            let (stream, _) = listener.accept().await?;
            node.git_connect(wf_net::endpoint_id(device), workforce, stream)
                .await
                .map_err(FetchError::from)
        };
        tokio::select! {
            fetched = &mut fetch => Ok(fetched??),
            relayed = relay => {
                // The relay ending first is a refusal or a broken connection, which says more
                // than git's hanging up; ending well, git is about to finish.
                relayed?;
                Ok(fetch.await??)
            }
        }
    }
}

/// The bridge `git fetch` runs: pipes its own input and output to the unix socket at `socket`,
/// and passes on the end of either. Blocking, for a process that does nothing else.
pub fn run_git_bridge(socket: &Path) -> std::io::Result<()> {
    use std::net::Shutdown;
    use std::os::unix::net::UnixStream;

    let stream = UnixStream::connect(socket)?;
    let mut writer = stream.try_clone()?;
    let upstream = std::thread::spawn(move || {
        let copied = std::io::copy(&mut std::io::stdin().lock(), &mut writer);
        let _ = writer.shutdown(Shutdown::Write);
        copied
    });
    let mut reader = stream;
    std::io::copy(&mut reader, &mut std::io::stdout().lock())?;
    // Git closes its end of our input when it is done, which ends the thread.
    let _ = upstream.join();
    Ok(())
}

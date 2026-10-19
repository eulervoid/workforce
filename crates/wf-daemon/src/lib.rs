//! The daemon: one per machine, one device key and one iroh endpoint, serving every workforce
//! stored on it. It owns the replicas, answers sync and join from other devices, keeps in touch
//! with them, and serves the replica to the CLI over a unix socket.

pub mod caches;
pub mod caller;
pub mod client;
pub mod command;
pub mod data_dir;
pub mod decide;
mod environment;
mod faults;
mod inherit;
mod judges;
pub mod machine_config;
mod notify;
mod panics;
mod peers;
mod permissions;
pub mod proto;
mod runner;
mod sandbox;
mod server;
mod storage;

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio::net::UnixListener;
use wf_core::event::{DeviceProfile, Event, EventBody};
use wf_core::id::WorkforceId;
use wf_core::identity::DeviceSigner;
use wf_core::invite::InviteCode;
use wf_net::{NetError, Network, Node};
use wf_store::{ForkError, HistorySource, RedbStore, Replica, StoreError};

pub use data_dir::DataDir;
pub use faults::{HarnessAtFault, MachineFaults, OutOfSpace, RETRY_HARNESS};
pub use panics::{Panics, watch_panics};
pub use peers::{PeerOutcome, PeerSync, Presence, peers_of, sync_all};
pub use runner::{SessionError, ensure_unclaimed, session_model};
pub use storage::{StorageClosed, StorageHandle};

/// The file in the directory of a workforce that says where its repository is on this machine.
pub const REPOSITORY_FILE: &str = "repository";

/// Records where the repository of the workforce stored in `workforce_directory` is. The file is
/// replaced in one step, so a reader sees the old path or the new one and never half of one.
pub fn remember_repository(
    workforce_directory: &std::path::Path,
    repository: &std::path::Path,
) -> std::io::Result<()> {
    let path = workforce_directory.join(REPOSITORY_FILE);
    let text = repository.to_string_lossy();
    if std::fs::read_to_string(&path).is_ok_and(|current| current.trim() == text) {
        return Ok(());
    }
    data_dir::write_private(&path, text.as_bytes())
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("a daemon is already listening on {0}")]
    AlreadyRunning(PathBuf),
    #[error("socket {path}: {source}")]
    Socket {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Net(#[from] NetError),
    #[error("workforce {0} is not stored on this machine")]
    UnknownWorkforce(WorkforceId),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Fork(#[from] ForkError),
    #[error(transparent)]
    StorageClosed(#[from] StorageClosed),
    #[error("could not reach a device to join through: {0}")]
    JoinUnreachable(String),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub struct Config {
    /// Holds one directory per workforce, named by its id.
    pub data_dir: DataDir,
    pub socket: PathBuf,
    pub network: Network,
    /// Commands for harnesses, in place of the ones wf knows by name. Tests start fake agents
    /// this way.
    pub harnesses: BTreeMap<wf_core::name::HarnessName, wf_harness::AcpAgentConfig>,
    /// The machine config, read for each permission request. `None` uses the defaults.
    pub machine_config: Option<PathBuf>,
    /// The `wf` binary agents run. The daemon copies it to `<data>/bin/wf` when it starts and
    /// puts that directory first on the agents' `PATH`, so rebuilding or upgrading `wf` does not
    /// change what running agents call. `None` leaves `PATH` as it is.
    pub wf_binary: Option<PathBuf>,
}

pub(crate) struct Loaded {
    pub(crate) storage: StorageHandle,
    pub(crate) merges: runner::MergeQueue,
}

/// Serves the workforce to its other devices and keeps in sync with them, from when it first has
/// another device or an open invite.
async fn go_online_when_needed(
    daemon: std::sync::Weak<Daemon>,
    id: WorkforceId,
    storage: StorageHandle,
) {
    let Some((own, mut halted)) = daemon
        .upgrade()
        .map(|daemon| (daemon.device(), daemon.halted.subscribe()))
    else {
        return;
    };
    let mut heads = storage.watch_heads();
    loop {
        let needed = storage
            .with(move |replica| peers::needs_network(replica.state(), own, now_ms()))
            .await;
        match needed {
            Ok(true) => break,
            Ok(false) => {}
            Err(StorageClosed) => return,
        }
        tokio::select! {
            changed = heads.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            _ = halted.wait_for(|halted| *halted) => return,
        }
    }
    let Some(daemon) = daemon.upgrade() else {
        return;
    };
    let node = match daemon.node().await {
        Ok(node) => node,
        Err(error) => {
            tracing::warn!(
                "workforce {id} stays offline: {}",
                wf_core::error_chain(&error)
            );
            return;
        }
    };
    node.serve(id, storage.clone());
    let presence = daemon.presence.clone();
    drop(daemon);
    peers::run_peer_loop(node, id, storage, presence).await;
}

/// Copies `binary` to `<directory>/wf`. Through a rename, so an agent that runs the old copy at
/// that moment keeps it. Not a hard link: in the tests, macOS killed runs of the binary while
/// daemons linked it.
fn install_wf(binary: &Path, directory: &Path) -> Result<(), DaemonError> {
    let io = |path: &Path| {
        let path = path.to_path_buf();
        move |source| DaemonError::Io { path, source }
    };
    std::fs::create_dir_all(directory).map_err(io(directory))?;
    let staged = directory.join(".wf.new");
    std::fs::copy(binary, &staged).map_err(io(binary))?;
    let installed = directory.join("wf");
    std::fs::rename(&staged, &installed).map_err(io(&installed))
}

/// Workforces created or joined while the daemon runs are picked up this often.
const RESCAN_EVERY: std::time::Duration = std::time::Duration::from_secs(10);

async fn rescan(daemon: std::sync::Weak<Daemon>) {
    loop {
        tokio::time::sleep(RESCAN_EVERY).await;
        let Some(daemon) = daemon.upgrade() else {
            return;
        };
        if *daemon.halted.borrow() {
            return;
        }
        daemon.load_stored_workforces();
    }
}

/// Looks for the history of a fork among the workforces this daemon has loaded.
struct ParentHistory(std::sync::Weak<Daemon>);

impl HistorySource for ParentHistory {
    fn history_behind(
        &self,
        parent: WorkforceId,
        wanted: &std::collections::BTreeSet<wf_core::id::EventId>,
    ) -> Vec<Event> {
        let Some(loaded) = self
            .0
            .upgrade()
            .and_then(|daemon| daemon.loaded_workforce(&parent))
        else {
            return Vec::new();
        };
        let wanted = wanted.clone();
        loaded
            .storage
            .with_blocking(move |replica| HistorySource::history_behind(&*replica, parent, &wanted))
            .unwrap_or_default()
    }
}

/// Which writes of events a test has fail.
type WriteMatcher = fn(&EventBody) -> bool;

pub struct Daemon {
    config: Config,
    device: DeviceSigner,
    /// Started when a workforce first needs the network, so that a workforce on one device never
    /// goes online.
    node: tokio::sync::OnceCell<Arc<Node<StorageHandle>>>,
    presence: Presence,
    runners: runner::Runners,
    starting: runner::ItemLocks,
    /// Taken while a session ends, so that two ends of one session, such as the agent's submit
    /// and a timeout, do not both act.
    ending: tokio::sync::Mutex<()>,
    held: Arc<permissions::Held>,
    /// Whom each connection to the socket acts for.
    callers: caller::Callers,
    judges: judges::Judges,
    loaded: Mutex<BTreeMap<WorkforceId, Arc<Loaded>>>,
    /// Stores that failed to open, so each is reported once.
    failed: Mutex<std::collections::BTreeSet<WorkforceId>>,
    /// Turns true when the daemon halts; its background tasks then return.
    halted: tokio::sync::watch::Sender<bool>,
    /// What is wrong with this machine, as what failed said.
    faults: faults::Faults,
    /// For the tests: the next write of an event this matches fails once.
    failing_write: Mutex<Option<WriteMatcher>>,
}

impl Daemon {
    pub async fn start(config: Config, device: DeviceSigner) -> Result<Arc<Self>, DaemonError> {
        if let Some(binary) = &config.wf_binary {
            install_wf(binary, &config.data_dir.bin())?;
        }
        let daemon = Arc::new(Self {
            config,
            device,
            node: tokio::sync::OnceCell::new(),
            presence: Presence::default(),
            runners: runner::Runners::default(),
            starting: runner::ItemLocks::default(),
            ending: tokio::sync::Mutex::new(()),
            held: Arc::default(),
            callers: caller::Callers::default(),
            judges: judges::Judges::default(),
            loaded: Mutex::new(BTreeMap::new()),
            failed: Mutex::new(Default::default()),
            halted: tokio::sync::watch::Sender::new(false),
            faults: faults::Faults::default(),
            failing_write: Mutex::new(None),
        });
        daemon.load_stored_workforces();
        tokio::spawn(rescan(Arc::downgrade(&daemon)));
        Ok(daemon)
    }

    pub fn device(&self) -> wf_core::identity::DeviceKey {
        self.device.key()
    }

    /// Stops the harness of every session and judge without ending the sessions, and the daemon's
    /// background tasks, so that the next daemon resumes the sessions. Waits until the harnesses
    /// are closed.
    pub async fn halt(&self) {
        self.halted.send_replace(true);
        self.runners.halt().await;
        self.judges.close().await;
    }

    /// Takes `error` as the error of a write of the daemon that failed: whether it says the disk
    /// is full. For the tests, which cannot fill a disk.
    #[doc(hidden)]
    pub fn write_failed(&self, what: &str, error: &std::io::Error) -> bool {
        self.faults.note_space(what, error)
    }

    /// Has the next write of an event that `matching` accepts fail once, as a write fails on a
    /// full disk. For the tests, which cannot fill a disk.
    #[doc(hidden)]
    pub fn fail_next_write(&self, matching: WriteMatcher) {
        *self.failing_write.lock().unwrap() = Some(matching);
    }

    /// Whether a write of `body` is the one to fail, which is the case only once.
    pub(crate) fn write_fails(&self, body: &EventBody) -> bool {
        let mut failing = self.failing_write.lock().unwrap();
        if failing.is_some_and(|matching| matching(body)) {
            *failing = None;
            return true;
        }
        false
    }

    /// Completes once the daemon halts.
    pub(crate) async fn until_halted(&self) {
        let mut halted = self.halted.subscribe();
        let _ = halted.wait_for(|halted| *halted).await;
    }

    /// The directory with the daemon's copy of `wf`, if it made one.
    pub(crate) fn bin_dir(&self) -> Option<PathBuf> {
        self.config
            .wf_binary
            .as_ref()
            .map(|_| self.config.data_dir.bin())
    }

    /// The network node of this machine, started on first use.
    pub async fn node(&self) -> Result<Arc<Node<StorageHandle>>, NetError> {
        self.node
            .get_or_try_init(|| async {
                let node = Node::start(&self.device, self.config.network.clone()).await?;
                tracing::info!("the network is on");
                Ok(Arc::new(node))
            })
            .await
            .cloned()
    }

    fn load_stored_workforces(self: &Arc<Self>) {
        let Ok(entries) = std::fs::read_dir(self.config.data_dir.root()) else {
            return;
        };
        for entry in entries.flatten() {
            let Some(id) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<WorkforceId>().ok())
            else {
                continue;
            };
            if !self.config.data_dir.store(&id).is_file() {
                continue;
            }
            if self.loaded_workforce(&id).is_some() {
                continue;
            }
            if let Err(error) = self.workforce(id) {
                let first_time = self
                    .failed
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(id);
                if first_time {
                    let store = self.config.data_dir.store(&id);
                    let hint = if let DaemonError::Store(StoreError::Corrupt(_)) = &error {
                        format!(
                            "; move {} away, then `wf import` an export of the workforce, or join it again from another device",
                            store.display()
                        )
                    } else {
                        String::new()
                    };
                    tracing::warn!(
                        "not serving workforce {id}: {}{hint}",
                        wf_core::error_chain(&error)
                    );
                }
            }
        }
    }

    /// The loaded workforce, opened from this machine's data directory on first use.
    pub(crate) fn workforce(self: &Arc<Self>, id: WorkforceId) -> Result<Arc<Loaded>, DaemonError> {
        let mut loaded = self
            .loaded
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match loaded.get(&id) {
            Some(existing) if !existing.storage.is_closed() => return Ok(Arc::clone(existing)),
            Some(_) => {
                loaded.remove(&id);
            }
            None => {}
        }
        let path = self.config.data_dir.store(&id);
        if !path.is_file() {
            return Err(DaemonError::UnknownWorkforce(id));
        }
        // Every write to the log goes through the storage task, so it is where a full disk is
        // noticed, whoever wrote: a session, the principal, an agent, the merge task or a peer.
        let history = {
            let daemon = Arc::downgrade(self);
            move || Some(Box::new(ParentHistory(daemon.clone())) as Box<dyn HistorySource>)
        };
        let note_space = {
            let daemon = Arc::downgrade(self);
            move |error: &(dyn std::error::Error + 'static)| {
                if let Some(daemon) = daemon.upgrade() {
                    daemon.faults.note_space("the event log", error);
                }
            }
        };
        let storage = StorageHandle::open(path, id, history, note_space)?;
        // With the network on, serve at once: a device may sync a fork right after it was made.
        if let Some(node) = self.node.get() {
            node.serve(id, storage.clone());
        }
        tokio::spawn(go_online_when_needed(
            Arc::downgrade(self),
            id,
            storage.clone(),
        ));
        let entry = Arc::new(Loaded {
            storage,
            merges: runner::MergeQueue::default(),
        });
        loaded.insert(id, Arc::clone(&entry));
        tokio::spawn(runner::run_merger(
            Arc::downgrade(self),
            id,
            Arc::clone(&entry),
        ));
        let daemon = Arc::clone(self);
        let started = Arc::clone(&entry);
        tokio::spawn(async move {
            daemon.resume_sessions(id, &started).await;
            daemon.remove_stray_worktrees(id, &started).await;
            daemon.keep_caches_in_budget(id, &started).await;
            let weak = Arc::downgrade(&daemon);
            drop(daemon);
            runner::run_dispatcher(weak, id, started).await;
        });
        Ok(entry)
    }

    /// The loaded workforce, if it is one and its storage is running: nothing is opened.
    pub(crate) fn loaded_workforce(&self, id: &WorkforceId) -> Option<Arc<Loaded>> {
        self.loaded
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(id)
            .filter(|loaded| !loaded.storage.is_closed())
            .cloned()
    }

    /// Like [`Daemon::workforce`], and creates the workforce empty when this machine does not
    /// have it yet, so it can be taken in from other devices.
    pub(crate) fn follow(self: &Arc<Self>, id: WorkforceId) -> Result<Arc<Loaded>, DaemonError> {
        let directory = self.config.data_dir.workforce(&id);
        std::fs::create_dir_all(&directory).map_err(|source| DaemonError::Io {
            path: directory.clone(),
            source,
        })?;
        drop(RedbStore::open(self.config.data_dir.store(&id))?);
        self.workforce(id)
    }

    /// Founds the fork that `genesis` starts from `parent`, which is a workforce of this daemon,
    /// and starts serving it. The history is taken from the parent, so the fork is complete as
    /// soon as it exists. If it cannot be founded nothing is left behind.
    pub(crate) async fn create_fork(
        self: &Arc<Self>,
        parent: &Loaded,
        genesis: Event,
    ) -> Result<(), DaemonError> {
        let child = WorkforceId::from(genesis.id());
        let directory = self.config.data_dir.workforce(&child);
        std::fs::create_dir_all(&directory).map_err(|source| DaemonError::Io {
            path: directory.clone(),
            source,
        })?;
        let path = self.config.data_dir.store(&child);
        let created = {
            let path = path.clone();
            parent
                .storage
                .with(move |replica| {
                    let store = RedbStore::open(path)?;
                    Replica::create_fork(store, genesis, &*replica).map(drop)
                })
                .await?
        };
        if let Err(error) = created {
            let _ = std::fs::remove_file(&path);
            return Err(error.into());
        }
        self.workforce(child)?;
        Ok(())
    }

    /// Joins the workforce of `code` with this daemon's device key, through the first of the
    /// devices it names that can be reached. A refusal ends it at once.
    pub(crate) async fn join(
        self: &Arc<Self>,
        loaded: &Loaded,
        code: &InviteCode,
        profile: DeviceProfile,
    ) -> Result<(), DaemonError> {
        let mut failures = Vec::new();
        for bootstrap in &code.bootstrap {
            let storage = loaded.storage.clone();
            let signer = self.device.clone();
            let (invite, profile) = (code.invite, profile.clone());
            let proof = code
                .prove(signer.key())
                .map_err(|error| NetError::JoinAuthoring(error.into()))?;
            let author = {
                let storage = storage.clone();
                move || async move {
                    storage
                        .with(move |replica| {
                            replica.bind_device(&signer, invite, proof, profile, now_ms())
                        })
                        .await?
                        .map_err(Into::into)
                }
            };
            match self
                .node()
                .await?
                .join(code, wf_net::endpoint_id(*bootstrap), &storage, author)
                .await
            {
                Ok(()) => return Ok(()),
                Err(error @ (NetError::JoinRefused { .. } | NetError::JoinAuthoring(_))) => {
                    return Err(error.into());
                }
                Err(error) => failures.push(wf_core::error_chain(&error)),
            }
        }
        Err(DaemonError::JoinUnreachable(failures.join("; ")))
    }

    /// Serves the CLI on the socket until `shutdown` completes.
    pub async fn serve(
        self: Arc<Self>,
        shutdown: impl Future<Output = ()>,
    ) -> Result<(), DaemonError> {
        let listener = self.bind_socket().await?;
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                _ = &mut shutdown => break,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { continue };
                    tokio::spawn(server::handle(Arc::clone(&self), stream));
                }
            }
        }
        let _ = std::fs::remove_file(&self.config.socket);
        Ok(())
    }

    async fn bind_socket(&self) -> Result<UnixListener, DaemonError> {
        use std::os::unix::fs::PermissionsExt;

        let socket_error = |source| DaemonError::Socket {
            path: self.config.socket.clone(),
            source,
        };
        if let Some(parent) = self.config.socket.parent() {
            data_dir::create_private_directory(parent).map_err(socket_error)?;
        }
        if self.config.socket.exists() {
            if tokio::net::UnixStream::connect(&self.config.socket)
                .await
                .is_ok()
            {
                return Err(DaemonError::AlreadyRunning(self.config.socket.clone()));
            }
            let _ = std::fs::remove_file(&self.config.socket);
        }
        // After a running daemon would have answered, so its token stays; before the socket
        // takes connections, so a client finds this daemon's token.
        self.callers
            .publish(&self.config.data_dir)
            .map_err(|source| DaemonError::Io {
                path: self.config.data_dir.principal_token(),
                source,
            })?;
        let listener = UnixListener::bind(&self.config.socket).map_err(socket_error)?;
        std::fs::set_permissions(&self.config.socket, std::fs::Permissions::from_mode(0o600))
            .map_err(socket_error)?;
        Ok(listener)
    }

    pub(crate) fn presence(&self) -> &Presence {
        &self.presence
    }

    pub(crate) fn signer(&self) -> &DeviceSigner {
        &self.device
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_installed_wf_stays_when_the_original_changes() {
        let root = tempfile::tempdir().unwrap();
        let original = root.path().join("wf");
        let bin = DataDir::new(root.path().join("data")).bin();
        std::fs::write(&original, "first build").unwrap();
        install_wf(&original, &bin).unwrap();
        std::fs::write(&original, "second build").unwrap();
        assert_eq!(
            std::fs::read_to_string(bin.join("wf")).unwrap(),
            "first build"
        );

        install_wf(&original, &bin).unwrap();
        assert_eq!(
            std::fs::read_to_string(bin.join("wf")).unwrap(),
            "second build"
        );
    }
}

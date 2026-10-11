//! The daemon: one per machine, one device key and one iroh endpoint, serving every workforce
//! stored on it. It owns the replicas, answers sync and join from other devices, keeps in touch
//! with them, and serves the replica to the CLI over a unix socket.

pub mod client;
pub mod decide;
mod judges;
pub mod machine_config;
mod peers;
mod permissions;
pub mod proto;
mod runner;
mod server;
mod storage;

use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::net::UnixListener;
use wf_core::event::{DeviceProfile, Event};
use wf_core::id::WorkforceId;
use wf_core::identity::DeviceSigner;
use wf_core::invite::InviteCode;
use wf_net::replica::bind_device;
use wf_net::{NetError, Network, Node};
use wf_store::{ForkError, HistorySource, RedbStore, Replica, StoreError};

pub use peers::{PeerOutcome, PeerSync, Presence, peers_of, sync_all};
pub use runner::SessionError;
pub use storage::{StorageClosed, StorageHandle};

pub const STORE_FILE: &str = "store.redb";

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
    let temporary = workforce_directory.join(format!("{REPOSITORY_FILE}.{}", std::process::id()));
    std::fs::write(&temporary, text.as_bytes())?;
    std::fs::rename(temporary, path)
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
    pub data_dir: PathBuf,
    pub socket: PathBuf,
    pub network: Network,
    /// Commands for harnesses, in place of the ones wf knows by name. Tests start fake agents
    /// this way.
    pub harnesses: BTreeMap<wf_core::name::HarnessName, wf_harness::AcpAgentConfig>,
    /// The machine config, read for each permission request. `None` uses the defaults.
    pub machine_config: Option<PathBuf>,
}

pub(crate) struct Loaded {
    pub(crate) storage: StorageHandle,
}

/// Workforces created or joined while the daemon runs are picked up this often.
const RESCAN_EVERY: std::time::Duration = std::time::Duration::from_secs(10);

async fn rescan(daemon: std::sync::Weak<Daemon>) {
    loop {
        tokio::time::sleep(RESCAN_EVERY).await;
        let Some(daemon) = daemon.upgrade() else {
            return;
        };
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

pub struct Daemon {
    config: Config,
    device: DeviceSigner,
    node: Arc<Node<StorageHandle>>,
    presence: Presence,
    runners: runner::Runners,
    merging: runner::Merging,
    held: Arc<permissions::Held>,
    judges: judges::Judges,
    loaded: Mutex<BTreeMap<WorkforceId, Arc<Loaded>>>,
    /// Stores that failed to open, so each is reported once.
    failed: Mutex<std::collections::BTreeSet<WorkforceId>>,
}

impl Daemon {
    pub async fn start(config: Config, device: DeviceSigner) -> Result<Arc<Self>, DaemonError> {
        let node = Arc::new(Node::start(&device, config.network.clone()).await?);
        let daemon = Arc::new(Self {
            config,
            device,
            node,
            presence: Presence::default(),
            runners: runner::Runners::default(),
            merging: runner::Merging::default(),
            held: Arc::default(),
            judges: judges::Judges::default(),
            loaded: Mutex::new(BTreeMap::new()),
            failed: Mutex::new(Default::default()),
        });
        daemon.load_stored_workforces();
        tokio::spawn(rescan(Arc::downgrade(&daemon)));
        Ok(daemon)
    }

    pub fn device(&self) -> wf_core::identity::DeviceKey {
        self.device.key()
    }

    pub fn node(&self) -> &Arc<Node<StorageHandle>> {
        &self.node
    }

    fn load_stored_workforces(self: &Arc<Self>) {
        let Ok(entries) = std::fs::read_dir(&self.config.data_dir) else {
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
            if !entry.path().join(STORE_FILE).is_file() {
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
                    eprintln!(
                        "not serving workforce {id}: {}; if it is from an older build, remove {}",
                        wf_core::report::report(&error),
                        self.config.data_dir.join(id.to_string()).display()
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
        let path = self.config.data_dir.join(id.to_string()).join(STORE_FILE);
        if !path.is_file() {
            return Err(DaemonError::UnknownWorkforce(id));
        }
        let open = {
            let daemon = Arc::downgrade(self);
            move || {
                let mut replica = Replica::open(RedbStore::open(path.clone())?, id)?;
                replica.set_history_source(Box::new(ParentHistory(daemon.clone())));
                Ok(replica)
            }
        };
        let storage = StorageHandle::spawn(open()?, open);
        self.node.serve(id, storage.clone());
        tokio::spawn(peers::run_peer_loop(
            Arc::clone(&self.node),
            id,
            storage.clone(),
            self.presence.clone(),
        ));
        let entry = Arc::new(Loaded { storage });
        loaded.insert(id, Arc::clone(&entry));
        let daemon = Arc::clone(self);
        let started = Arc::clone(&entry);
        tokio::spawn(async move {
            daemon.end_stale_sessions(id, &started).await;
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
        let directory = self.config.data_dir.join(id.to_string());
        std::fs::create_dir_all(&directory).map_err(|source| DaemonError::Io {
            path: directory.clone(),
            source,
        })?;
        drop(RedbStore::open(directory.join(STORE_FILE))?);
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
        let directory = self.config.data_dir.join(child.to_string());
        std::fs::create_dir_all(&directory).map_err(|source| DaemonError::Io {
            path: directory.clone(),
            source,
        })?;
        let path = directory.join(STORE_FILE);
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
                            bind_device(replica, &signer, invite, proof, profile, now_ms())
                        })
                        .await?
                        .map_err(Into::into)
                }
            };
            match self
                .node
                .join(code, wf_net::endpoint_id(*bootstrap), &storage, author)
                .await
            {
                Ok(()) => return Ok(()),
                Err(error @ (NetError::JoinRefused { .. } | NetError::JoinAuthoring(_))) => {
                    return Err(error.into());
                }
                Err(error) => failures.push(wf_core::report::report(&error)),
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
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

        let socket_error = |source| DaemonError::Socket {
            path: self.config.socket.clone(),
            source,
        };
        if let Some(parent) = self.config.socket.parent() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)
                .map_err(socket_error)?;
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

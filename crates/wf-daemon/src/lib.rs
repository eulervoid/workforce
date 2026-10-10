//! The daemon: one per machine, one device key and one iroh endpoint, serving every workforce
//! stored on it. It owns the replicas, answers sync and join from other devices, keeps in touch
//! with them, and serves the replica to the CLI over a unix socket.

pub mod client;
mod peers;
pub mod proto;
mod server;
mod storage;

use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::net::UnixListener;
use wf_core::id::WorkforceId;
use wf_core::identity::DeviceSigner;
use wf_net::{NetError, Network, Node};
use wf_store::{RedbStore, Replica, StoreError};

pub use peers::{PeerOutcome, PeerSync, Presence, peers_of, sync_all};
pub use storage::{StorageClosed, StorageHandle};

pub const STORE_FILE: &str = "store.redb";

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
}

pub struct Config {
    /// Holds one directory per workforce, named by its id.
    pub data_dir: PathBuf,
    pub socket: PathBuf,
    pub network: Network,
}

pub(crate) struct Loaded {
    pub(crate) storage: StorageHandle,
}

pub struct Daemon {
    config: Config,
    device: DeviceSigner,
    node: Arc<Node<StorageHandle>>,
    presence: Presence,
    loaded: Mutex<BTreeMap<WorkforceId, Arc<Loaded>>>,
}

impl Daemon {
    pub async fn start(config: Config, device: DeviceSigner) -> Result<Arc<Self>, DaemonError> {
        let node = Arc::new(Node::start(&device, config.network.clone()).await?);
        let daemon = Arc::new(Self {
            config,
            device,
            node,
            presence: Presence::default(),
            loaded: Mutex::new(BTreeMap::new()),
        });
        daemon.load_stored_workforces();
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
            if let Err(error) = self.workforce(id) {
                eprintln!("not serving workforce {id}: {error}");
            }
        }
    }

    /// The loaded workforce, opened from this machine's data directory on first use.
    pub(crate) fn workforce(self: &Arc<Self>, id: WorkforceId) -> Result<Arc<Loaded>, DaemonError> {
        let mut loaded = self
            .loaded
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(existing) = loaded.get(&id) {
            return Ok(Arc::clone(existing));
        }
        let path = self.config.data_dir.join(id.to_string()).join(STORE_FILE);
        if !path.is_file() {
            return Err(DaemonError::UnknownWorkforce(id));
        }
        let replica = Replica::open(RedbStore::open(path)?)?;
        if replica.state().workforce_id() != Some(id) {
            return Err(DaemonError::UnknownWorkforce(id));
        }
        let storage = StorageHandle::spawn(replica);
        self.node.serve(id, storage.clone());
        tokio::spawn(peers::run_peer_loop(
            Arc::clone(&self.node),
            id,
            storage.clone(),
            self.presence.clone(),
        ));
        let entry = Arc::new(Loaded { storage });
        loaded.insert(id, Arc::clone(&entry));
        Ok(entry)
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

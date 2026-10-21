//! The daemon: one per machine, one device key and one iroh endpoint, serving every workforce
//! stored on it. It owns the replicas, answers sync and join from other devices, keeps in touch
//! with them, and serves the replica to the CLI over a unix socket.

pub mod caches;
pub mod caller;
pub mod client;
pub mod command;
mod commits;
pub mod data_dir;
pub mod decide;
mod environment;
mod faults;
mod git_service;
mod inherit;
mod judges;
mod keyed_locks;
mod live_diff;
pub mod machine_config;
mod notify;
mod open_files;
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
use tracing::instrument::WithSubscriber;
use wf_core::claims::Presence;
use wf_core::event::{DeviceProfile, Event, EventBody};
use wf_core::id::{SessionId, WorkforceId};
use wf_core::identity::DeviceSigner;
use wf_core::invite::InviteCode;
use wf_net::{NetError, Network, Node};
use wf_store::{ForkError, HistorySource, RedbStore, Replica, StoreError};

pub use data_dir::DataDir;
pub use faults::{HarnessAtFault, MachineFaults, OutOfSpace, RETRY_HARNESS, Retry};
pub use git_service::{FetchError, run_git_bridge};
pub use open_files::raise_open_file_limit;
pub use panics::{Panics, watch_panics};
pub use peers::{PeerOutcome, PeerSync, peers_of, sync_all};
pub use runner::{SessionError, session_model};
pub use storage::{StorageClosed, StorageHandle};

/// The file in the directory of a workforce that said where its repository was on this machine,
/// before wf had a bare repository of its own. A daemon that finds it makes the bare repository
/// from that repository once, and removes the file.
const LEGACY_REPOSITORY_FILE: &str = "repository";

/// Spawns `task` logging where its caller logs, so that a daemon run with a log of its own, as
/// each daemon of the tests is, keeps everything its tasks log in it.
pub(crate) fn spawn<F>(task: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    tokio::spawn(task.with_current_subscriber())
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

/// What a daemon is started with: where it keeps its data and listens, and what it runs for agents.
pub struct Launch {
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
    pub(crate) commits: commits::Commits,
    /// Held by whatever writes the refs of items in the bare repository (`wf/<item>` and the work
    /// set apart): the refs pass, imports and starts. The refs pass reads the log only once it
    /// holds it, so it never acts on a state older than a write before it. Only the refs pass
    /// removes them.
    pub(crate) item_refs: tokio::sync::Mutex<()>,
    pub(crate) target: runner::TargetBranch,
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
            .with(move |replica| peers::needs_network(replica.state(), own, wf_store::now_ms()))
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
    daemon.serve_on(&node, id, &storage);
    let presence = Arc::clone(&daemon.presence);
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
/// How long the accept loop waits after the socket fails to accept.
const ACCEPT_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(250);

async fn rescan(daemon: std::sync::Weak<Daemon>) {
    loop {
        tokio::time::sleep(RESCAN_EVERY).await;
        let Some(daemon) = daemon.upgrade() else {
            return;
        };
        if *daemon.halted.borrow() {
            return;
        }
        daemon.load_stored_workforces().await;
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
    launch: Launch,
    device: DeviceSigner,
    /// Started when a workforce first needs the network, so that a workforce on one device never
    /// goes online.
    node: tokio::sync::OnceCell<Arc<Node<StorageHandle>>>,
    /// What this daemon heard of the other devices since it started, to tell which are gone.
    presence: Arc<Mutex<Presence>>,
    runners: runner::Runners,
    /// The checks the daemon runs as sessions submit.
    submissions: runner::SubmissionChecks,
    /// Taken while checks run in the check worktree of a workforce, which they share.
    checking: Arc<keyed_locks::KeyedLocks<WorkforceId>>,
    /// Taken while a session starts, from reading the state until `SessionStarted` is written.
    starting: runner::StartLocks,
    /// Taken while a session ends, so that two ends of one session, such as the agent's submit
    /// and a timeout, do not both act; ends of other sessions do not wait.
    ending: Arc<keyed_locks::KeyedLocks<SessionId>>,
    /// Taken while a workforce is opened, because its store can be open only once.
    opening: Arc<keyed_locks::KeyedLocks<WorkforceId>>,
    /// Held while a workforce's bare repository is made, so two callers do not both make it.
    repositories: Arc<keyed_locks::KeyedLocks<WorkforceId>>,
    held: Arc<permissions::Held>,
    /// Whom each connection to the socket acts for.
    callers: caller::Callers,
    judges: judges::Judges,
    loaded: Mutex<BTreeMap<WorkforceId, Arc<Loaded>>>,
    /// Stores that failed to open, so each is reported once.
    failed: Mutex<std::collections::BTreeSet<WorkforceId>>,
    /// When the caches of each workforce were last measured against the budget.
    caches_measured: Mutex<BTreeMap<WorkforceId, std::time::Instant>>,
    /// Turns true when the daemon halts; its background tasks then return.
    halted: tokio::sync::watch::Sender<bool>,
    /// What is wrong with this machine, as what failed said.
    faults: faults::Faults,
    /// What the caches of each workforce took when they were last checked against the budget.
    /// A workforce without a budget has none.
    cache_usage: Mutex<BTreeMap<WorkforceId, caches::CacheUsage>>,
    /// For the tests: the next write of an event this matches fails once.
    failing_write: Mutex<Option<WriteMatcher>>,
    /// For the tests: runs once, after the next merge found the target at the tip and before it
    /// reads the target to merge onto.
    #[cfg(test)]
    before_merge_tree: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// How many principal reviews the daemon found to merge into the target when it first checked
    /// them; the tests wait on it to know a check ran.
    reviews_found_merging: std::sync::atomic::AtomicUsize,
}

impl Daemon {
    pub async fn start(launch: Launch, device: DeviceSigner) -> Result<Arc<Self>, DaemonError> {
        if let Some(binary) = &launch.wf_binary {
            install_wf(binary, &launch.data_dir.bin())?;
        }
        let daemon = Arc::new(Self {
            launch,
            device,
            node: tokio::sync::OnceCell::new(),
            presence: Arc::new(Mutex::new(Presence::new(wf_store::now_ms(), []))),
            runners: runner::Runners::default(),
            submissions: runner::SubmissionChecks::default(),
            checking: Default::default(),
            starting: runner::StartLocks::default(),
            ending: Default::default(),
            opening: Default::default(),
            repositories: Default::default(),
            held: Arc::default(),
            callers: caller::Callers::default(),
            judges: judges::Judges::default(),
            loaded: Mutex::new(BTreeMap::new()),
            failed: Mutex::new(Default::default()),
            caches_measured: Mutex::new(BTreeMap::new()),
            halted: tokio::sync::watch::Sender::new(false),
            faults: faults::Faults::default(),
            cache_usage: Mutex::new(BTreeMap::new()),
            failing_write: Mutex::new(None),
            #[cfg(test)]
            before_merge_tree: Mutex::new(None),
            reviews_found_merging: Default::default(),
        });
        daemon.load_stored_workforces().await;
        crate::spawn(rescan(Arc::downgrade(&daemon)));
        Ok(daemon)
    }

    /// For the tests: how many principal reviews the daemon has found to merge when first checking
    /// them.
    pub fn reviews_found_merging(&self) -> usize {
        self.reviews_found_merging
            .load(std::sync::atomic::Ordering::SeqCst)
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

    /// Has this daemon start, and hear what it heard, `by` earlier. For the tests, which cannot
    /// wait out a lease.
    #[doc(hidden)]
    pub fn backdate_presence(&self, by: std::time::Duration) {
        let by_ms = u64::try_from(by.as_millis()).unwrap_or(u64::MAX);
        let mut presence = self
            .presence
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *presence = Presence::new(
            presence.started_ms().saturating_sub(by_ms),
            presence
                .heard_ms()
                .map(|(device, at)| (device, at.saturating_sub(by_ms))),
        );
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
        self.launch
            .wf_binary
            .as_ref()
            .map(|_| self.launch.data_dir.bin())
    }

    /// Whether the network node has been started.
    #[doc(hidden)]
    pub fn network_is_on(&self) -> bool {
        self.node.initialized()
    }

    /// The network node of this machine, started on first use.
    pub async fn node(&self) -> Result<Arc<Node<StorageHandle>>, NetError> {
        self.node
            .get_or_try_init(|| async {
                let node = Node::start(&self.device, self.launch.network.clone()).await?;
                tracing::info!("the network is on");
                Ok(Arc::new(node))
            })
            .await
            .cloned()
    }

    /// Has the node answer the other devices about `id`: sync from its log, and the work of its
    /// items from this machine's worktrees.
    fn serve_on(
        self: &Arc<Self>,
        node: &Node<StorageHandle>,
        id: WorkforceId,
        storage: &StorageHandle,
    ) {
        node.serve(id, storage.clone());
        node.serve_query(live_diff::WorktreeQuery::new(self));
        node.serve_git(git_service::UploadPack::new(self));
    }

    async fn load_stored_workforces(self: &Arc<Self>) {
        let Ok(entries) = std::fs::read_dir(self.launch.data_dir.root()) else {
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
            if !self.launch.data_dir.store(&id).is_file() {
                continue;
            }
            if self.loaded_workforce(&id).is_some() {
                continue;
            }
            if let Err(error) = self.workforce(id).await {
                let first_time = self
                    .failed
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(id);
                if first_time {
                    let store = self.launch.data_dir.store(&id);
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
    pub(crate) async fn workforce(
        self: &Arc<Self>,
        id: WorkforceId,
    ) -> Result<Arc<Loaded>, DaemonError> {
        if let Some(existing) = self.loaded_workforce(&id) {
            return Ok(existing);
        }
        // The open runs as a task of its own, so a caller that gives up halfway cannot leave a
        // finished open behind that nobody inserts, with the lock released.
        let daemon = Arc::clone(self);
        crate::spawn(async move { daemon.open_workforce(id).await })
            .await
            .map_err(|error| DaemonError::Io {
                path: self.launch.data_dir.store(&id),
                source: std::io::Error::other(error),
            })?
    }

    async fn open_workforce(self: &Arc<Self>, id: WorkforceId) -> Result<Arc<Loaded>, DaemonError> {
        // Opening folds the whole log, so it runs outside the `loaded` lock, which every lookup
        // takes. A store can be open only once, so opens of one workforce wait for each other.
        let _opening = self.opening.lock(id).await;
        if let Some(existing) = self.loaded_workforce(&id) {
            return Ok(existing);
        }
        let path = self.launch.data_dir.store(&id);
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
        let storage =
            tokio::task::spawn_blocking(move || StorageHandle::open(path, id, history, note_space))
                .await
                .map_err(|error| DaemonError::Io {
                    path: self.launch.data_dir.store(&id),
                    source: std::io::Error::other(error),
                })??;
        // With the network on, serve at once: a device may sync a fork right after it was made.
        if let Some(node) = self.node.get() {
            self.serve_on(node, id, &storage);
        }
        crate::spawn(go_online_when_needed(
            Arc::downgrade(self),
            id,
            storage.clone(),
        ));
        self.ensure_bare_repository(id, &storage).await;
        let entry = Arc::new(Loaded {
            storage,
            merges: runner::MergeQueue::default(),
            commits: commits::Commits::default(),
            item_refs: tokio::sync::Mutex::new(()),
            target: runner::TargetBranch::default(),
        });
        self.loaded
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id, Arc::clone(&entry));
        crate::spawn(runner::run_merger(
            Arc::downgrade(self),
            id,
            Arc::clone(&entry),
        ));
        crate::spawn(runner::run_target(
            Arc::downgrade(self),
            id,
            Arc::clone(&entry),
        ));
        crate::spawn(commits::run_commits(
            Arc::downgrade(self),
            id,
            Arc::clone(&entry),
            commits::DeviceFetcher::new(self, id),
        ));
        let daemon = Arc::clone(self);
        let started = Arc::clone(&entry);
        crate::spawn(async move {
            daemon.resume_sessions(id, &started).await;
            daemon.remove_stray_repositories(id, &started).await;
            daemon.keep_caches_in_budget(id, &started).await;
            let weak = Arc::downgrade(&daemon);
            drop(daemon);
            runner::run_dispatcher(weak, id, started).await;
        });
        Ok(entry)
    }

    /// Makes the bare repository of a workforce that has none yet: from the repository an earlier
    /// version remembered (its target and item branches, which also gets the remote `wf`), or
    /// empty when the workforce never had one or its checkout is gone. It waits for the policy,
    /// which a joined or forked workforce gets by sync after it loads, so it is called again
    /// wherever the repository is first needed. A failure is only logged, and tried again.
    pub(crate) async fn ensure_bare_repository(&self, id: WorkforceId, storage: &StorageHandle) {
        let _making = self.repositories.lock(id).await;
        let directory = self.launch.data_dir.workforce(&id);
        let bare = wf_vcs::bare_repository(&directory);
        if bare.join("HEAD").is_file() {
            return;
        }
        let target = storage
            .with(|replica| {
                replica
                    .state()
                    .policy()
                    .map(|config| config.policy.merge.target.clone())
            })
            .await;
        let Ok(Some(target)) = target else {
            return;
        };
        let remembered = directory.join(LEGACY_REPOSITORY_FILE);
        let checkout = std::fs::read_to_string(&remembered)
            .ok()
            .map(|text| PathBuf::from(text.trim()))
            .filter(|path| !path.as_os_str().is_empty());
        let made = tokio::task::spawn_blocking(move || {
            wf_vcs::GitCli::create_bare_repository(&bare, &target, checkout.as_deref()).map(|_| ())
        })
        .await;
        match made {
            Ok(Ok(())) => {
                let _ = std::fs::remove_file(&remembered);
            }
            Ok(Err(error)) => tracing::warn!(
                "could not make the repository of workforce {id}: {}",
                wf_core::error_chain(&error)
            ),
            Err(error) => tracing::warn!(
                "could not make the repository of workforce {id}: {}",
                wf_core::error_chain(&error)
            ),
        }
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
    pub(crate) async fn follow(
        self: &Arc<Self>,
        id: WorkforceId,
    ) -> Result<Arc<Loaded>, DaemonError> {
        let directory = self.launch.data_dir.workforce(&id);
        let store = self.launch.data_dir.store(&id);
        // A task of its own, like the open in `workforce`, so it finishes whatever the caller does.
        let daemon = Arc::clone(self);
        crate::spawn(async move {
            // Redb allows one open, so the pre-create waits for a concurrent `workforce` open.
            let _opening = daemon.opening.lock(id).await;
            if daemon.loaded_workforce(&id).is_none() {
                tokio::task::spawn_blocking(move || {
                    std::fs::create_dir_all(&directory).map_err(|source| DaemonError::Io {
                        path: directory.clone(),
                        source,
                    })?;
                    drop(RedbStore::open(store)?);
                    Ok::<_, DaemonError>(())
                })
                .await
                .map_err(|error| DaemonError::Io {
                    path: daemon.launch.data_dir.store(&id),
                    source: std::io::Error::other(error),
                })??;
            }
            Ok::<_, DaemonError>(())
        })
        .await
        .map_err(|error| DaemonError::Io {
            path: self.launch.data_dir.store(&id),
            source: std::io::Error::other(error),
        })??;
        self.workforce(id).await
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
        let directory = self.launch.data_dir.workforce(&child);
        std::fs::create_dir_all(&directory).map_err(|source| DaemonError::Io {
            path: directory.clone(),
            source,
        })?;
        let path = self.launch.data_dir.store(&child);
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
        self.workforce(child).await?;
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
                            replica.bind_device(&signer, invite, proof, profile, wf_store::now_ms())
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
                Ok(()) => {
                    self.build_repository_from(code.workforce, *bootstrap, &storage)
                        .await;
                    return Ok(());
                }
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
                    let Ok((stream, _)) = accepted else {
                        // Out of file descriptors, say: retrying at once would spin.
                        tokio::time::sleep(ACCEPT_RETRY_DELAY).await;
                        continue;
                    };
                    crate::spawn(server::handle(Arc::clone(&self), stream));
                }
            }
        }
        let _ = std::fs::remove_file(&self.launch.socket);
        Ok(())
    }

    async fn bind_socket(&self) -> Result<UnixListener, DaemonError> {
        use std::os::unix::fs::PermissionsExt;

        let socket_error = |source| DaemonError::Socket {
            path: self.launch.socket.clone(),
            source,
        };
        if let Some(parent) = self.launch.socket.parent() {
            data_dir::create_private_directory(parent).map_err(socket_error)?;
        }
        if self.launch.socket.exists() {
            if tokio::net::UnixStream::connect(&self.launch.socket)
                .await
                .is_ok()
            {
                return Err(DaemonError::AlreadyRunning(self.launch.socket.clone()));
            }
            let _ = std::fs::remove_file(&self.launch.socket);
        }
        // After a running daemon would have answered, so its token stays; before the socket
        // takes connections, so a client finds this daemon's token.
        self.callers
            .publish(&self.launch.data_dir)
            .map_err(|source| DaemonError::Io {
                path: self.launch.data_dir.principal_token(),
                source,
            })?;
        let listener = UnixListener::bind(&self.launch.socket).map_err(socket_error)?;
        std::fs::set_permissions(&self.launch.socket, std::fs::Permissions::from_mode(0o600))
            .map_err(socket_error)?;
        Ok(listener)
    }

    pub(crate) fn presence(&self) -> &Mutex<Presence> {
        &self.presence
    }

    /// What this daemon has heard of the other devices so far.
    pub(crate) fn presence_now(&self) -> Presence {
        self.presence
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
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

    use wf_core::testing::Fixture;
    use wf_store::{RedbStore, Replica};

    #[tokio::test]
    async fn a_workforce_whose_policy_arrives_after_it_loads_gets_its_bare_repository_then() {
        let directory = tempfile::TempDir::new().unwrap();
        let fixture = Fixture::new(1);
        let id = fixture.workforce();
        let data_dir = DataDir::new(directory.path().to_path_buf());
        // A joined workforce: its store is there, its log not yet synced.
        std::fs::create_dir_all(data_dir.workforce(&id)).unwrap();
        drop(Replica::open(RedbStore::open(data_dir.store(&id)).unwrap(), id).unwrap());
        let daemon = Daemon::start(
            Launch {
                data_dir: data_dir.clone(),
                socket: directory.path().join("daemon.sock"),
                network: wf_net::Network::Direct,
                harnesses: Default::default(),
                machine_config: None,
                wf_binary: None,
            },
            fixture.devices[0].clone(),
        )
        .await
        .unwrap();
        let loaded = daemon.open_workforce(id).await.unwrap();
        let bare = wf_vcs::bare_repository(&data_dir.workforce(&id));
        assert!(!bare.join("HEAD").exists(), "no policy to name the target");

        let genesis = fixture.genesis.clone();
        loaded
            .storage
            .with(move |replica| replica.receive(genesis))
            .await
            .unwrap()
            .unwrap();
        for _ in 0..250 {
            if bare.join("HEAD").is_file() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("the repository was not made when the policy arrived");
    }
}

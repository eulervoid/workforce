//! Keeping the commits the log names present on this device, and the refs that follow them.
//!
//! Whenever the log changes, the daemon lists the commits it names, fetches the ones this
//! repository lacks (from the device that wrote the event first, else from any other), and moves
//! the refs of the items: `wf/<item>` at the head of the session holding the claim, the heads of
//! sessions that lost it under `refs/wf/diverged/`, and none for items that are finished. What
//! cannot be fetched is tried again later and shown as awaited.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use n0_future::boxed::BoxFuture;
use tokio::sync::Notify;
use tokio::task::JoinSet;
use wf_core::commits::{ItemHeads, NamedCommit};
use wf_core::id::{ItemId, SessionId, WorkforceId};
use wf_core::identity::DeviceKey;
use wf_core::session::{CommitId, SessionRole};
use wf_vcs::{GitCli, VcsError, diverged_ref, item_branch, item_repository};

use crate::peers::{Backoff, peers_of};
use crate::runner::SessionError;
use crate::{Daemon, Loaded};

/// How long to wait before looking again while commits are missing and nothing says when a
/// device may be back.
const LOOK_AGAIN: Duration = Duration::from_secs(60);
/// Writes that follow each other closely are handled in one pass.
const SETTLE: Duration = Duration::from_millis(300);
/// How many commits are fetched at once.
const FETCHES_AT_ONCE: usize = 4;

/// Where a commit is fetched from: a device. The daemon fetches over iroh; the tests from
/// repositories on disk.
pub(crate) trait CommitFetcher: Send + Sync {
    fn fetch(&self, device: DeviceKey, commit: CommitId) -> BoxFuture<Result<(), String>>;
}

/// Fetches from other devices with `wf/git/1`.
pub(crate) struct DeviceFetcher {
    daemon: Weak<Daemon>,
    workforce: WorkforceId,
}

impl DeviceFetcher {
    pub(crate) fn new(daemon: &Arc<Daemon>, workforce: WorkforceId) -> Arc<Self> {
        Arc::new(Self {
            daemon: Arc::downgrade(daemon),
            workforce,
        })
    }
}

impl CommitFetcher for DeviceFetcher {
    fn fetch(&self, device: DeviceKey, commit: CommitId) -> BoxFuture<Result<(), String>> {
        let (daemon, workforce) = (self.daemon.clone(), self.workforce);
        Box::pin(async move {
            let daemon = daemon.upgrade().ok_or("the daemon is shutting down")?;
            daemon
                .fetch_commit_from(workforce, device, commit)
                .await
                .map_err(|error| wf_core::error_chain(&error))
        })
    }
}

/// What this device waits for before it can go on, for status and the dashboard, and when
/// something arrived.
#[derive(Default)]
pub(crate) struct Commits {
    awaited: Mutex<Vec<NamedCommit>>,
    /// Notified when commits arrived, so that what waited for them starts.
    pub(crate) arrived: Notify,
}

impl Commits {
    /// The commits the log names that this device lacks.
    pub(crate) fn awaited(&self) -> Vec<NamedCommit> {
        self.awaited
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn set_awaited(&self, awaited: Vec<NamedCommit>) {
        *self
            .awaited
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = awaited;
    }
}

/// An item that is done or cancelled, whose refs and repository are to go.
struct Finished {
    item: ItemId,
    /// Its work sessions, which may have refs of work set apart.
    sessions: Vec<SessionId>,
    /// A session of this machine still runs on it, so nothing is removed yet.
    busy: bool,
}

/// What a pass reads from the log.
struct Reading {
    named: Vec<NamedCommit>,
    heads: Vec<ItemHeads>,
    peers: Vec<DeviceKey>,
    finished: Vec<Finished>,
}

fn read(state: &wf_core::state::State, own: DeviceKey) -> Reading {
    let heads = state.item_heads();
    let finished = heads
        .iter()
        .filter(|heads| heads.finished)
        .map(|heads| {
            let work = || {
                state
                    .sessions_of(heads.item)
                    .filter(|session| session.role() == SessionRole::Work)
            };
            Finished {
                item: heads.item,
                sessions: work().map(|session| session.id()).collect(),
                busy: work().any(|session| session.is_live() && session.device() == own),
            }
        })
        .collect();
    Reading {
        named: state.named_commits(),
        heads,
        peers: peers_of(state, own),
        finished,
    }
}

/// The devices to ask for a commit, the one that wrote the event first.
fn devices_for(author: DeviceKey, own: DeviceKey, peers: &[DeviceKey]) -> Vec<DeviceKey> {
    let mut devices = Vec::new();
    if author != own {
        devices.push(author);
    }
    devices.extend(peers.iter().copied().filter(|peer| *peer != author));
    devices
}

/// One pass: fetches what is missing and is due, then sets the refs. Returns how long to wait
/// before another pass if something is still missing.
pub(crate) async fn reconcile(
    daemon: &Arc<Daemon>,
    workforce: WorkforceId,
    loaded: &Loaded,
    fetcher: &Arc<dyn CommitFetcher>,
    retries: &mut BTreeMap<CommitId, Backoff>,
) -> Option<Duration> {
    let own = daemon.device();
    daemon
        .ensure_bare_repository(workforce, &loaded.storage)
        .await;
    let bare = daemon.bare_repository(workforce);
    if !bare.join("HEAD").is_file() {
        return None;
    }
    let reading = loaded
        .storage
        .with(move |replica| read(replica.state(), own))
        .await
        .ok()?;
    let mut wanted: Vec<NamedCommit> = Vec::new();
    for named in &reading.named {
        if !wanted.iter().any(|seen| seen.commit == named.commit) {
            wanted.push(*named);
        }
    }
    let commits: Vec<CommitId> = wanted.iter().map(|named| named.commit).collect();
    let repository = bare.clone();
    let missing =
        tokio::task::spawn_blocking(move || GitCli::open(&repository)?.missing_commits(&commits))
            .await
            .ok()?;
    let missing: BTreeSet<CommitId> = match missing {
        Ok(missing) => missing.into_iter().collect(),
        Err(error) => {
            tracing::warn!(
                "could not tell which commits are missing: {}",
                wf_core::error_chain(&error)
            );
            return None;
        }
    };
    wanted.retain(|named| missing.contains(&named.commit));
    retries.retain(|commit, _| missing.contains(commit));
    loaded.commits.set_awaited(wanted.clone());

    let now = Instant::now();
    let semaphore = Arc::new(tokio::sync::Semaphore::new(FETCHES_AT_ONCE));
    let mut fetching = JoinSet::new();
    for named in wanted.iter().filter(|named| {
        retries
            .get(&named.commit)
            .is_none_or(|retry| retry.is_due(now))
    }) {
        let devices = devices_for(named.author, own, &reading.peers);
        let (commit, fetcher, semaphore) =
            (named.commit, Arc::clone(fetcher), Arc::clone(&semaphore));
        fetching.spawn(async move {
            let _slot = semaphore.acquire().await;
            for device in devices {
                match fetcher.fetch(device, commit).await {
                    Ok(()) => return (commit, true),
                    Err(error) => tracing::debug!(
                        "could not fetch {} from {}: {error}",
                        commit.short(),
                        device.fmt_short()
                    ),
                }
            }
            (commit, false)
        });
    }
    let mut fetched = BTreeSet::new();
    while let Some(done) = fetching.join_next().await {
        if let Ok((commit, got)) = done {
            retries
                .entry(commit)
                .or_default()
                .record(got, Instant::now());
            if got {
                fetched.insert(commit);
            }
        }
    }
    let still_missing: BTreeSet<CommitId> = missing.difference(&fetched).copied().collect();
    if !fetched.is_empty() {
        wanted.retain(|named| still_missing.contains(&named.commit));
        loaded.commits.set_awaited(wanted);
    }

    let workforce_directory = daemon.workforce_directory(workforce);
    let referred = still_missing.clone();
    let referring = tokio::task::spawn_blocking(move || {
        set_refs(
            &bare,
            &workforce_directory,
            own,
            &reading.heads,
            &reading.finished,
            &referred,
        )
    })
    .await;
    match referring {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(
            "could not move the refs of the items: {}",
            wf_core::error_chain(&error)
        ),
        Err(error) => tracing::warn!("could not move the refs of the items: {error}"),
    }
    if !fetched.is_empty() {
        loaded.commits.arrived.notify_one();
    }
    if still_missing.is_empty() {
        return None;
    }
    let waits = retries.values().map(|retry| retry.wait(Instant::now()));
    Some(waits.min().map_or(LOOK_AGAIN, |wait| wait.min(LOOK_AGAIN)))
}

/// Moves the refs of the items to the heads the log names, for those whose commits are here,
/// and removes what finished items left.
fn set_refs(
    bare: &std::path::Path,
    workforce_directory: &std::path::Path,
    own: DeviceKey,
    heads: &[ItemHeads],
    finished: &[Finished],
    missing: &BTreeSet<CommitId>,
) -> Result<(), VcsError> {
    let git = GitCli::open(bare)?;
    let apart: BTreeSet<String> = git.refs_apart()?.into_iter().collect();
    let branches: BTreeSet<String> = git.item_branches()?.into_iter().collect();
    for item in heads.iter().filter(|item| !item.finished) {
        let Some(holder) = item
            .holder
            .filter(|holder| !missing.contains(&holder.commit))
        else {
            continue;
        };
        // What runs here keeps its own branch, which the import moves.
        if holder.device != own {
            let at_head = git.branch_head(&item_branch(item.item)).ok() == Some(holder.commit);
            if !at_head {
                git.set_item_branch(item.item, holder.commit)?;
            }
        }
        for other in item
            .others
            .iter()
            .filter(|other| !missing.contains(&other.commit))
        {
            let kept = apart.contains(&diverged_ref(other.session));
            if git.is_ancestor(other.commit, holder.commit)? {
                if kept {
                    git.drop_apart(other.session)?;
                }
            } else if !kept {
                git.keep_apart(other.session, other.commit)?;
            }
        }
    }
    for done in finished.iter().filter(|done| !done.busy) {
        for session in &done.sessions {
            if apart.contains(&diverged_ref(*session)) {
                git.drop_apart(*session)?;
            }
        }
        if branches.contains(&item_branch(done.item)) {
            git.delete_branch(&item_branch(done.item))?;
        }
        let directory = item_repository(workforce_directory, done.item);
        if directory.exists() {
            wf_vcs::remove_agent_repository(&directory)?;
        }
    }
    Ok(())
}

/// Keeps the commits of a workforce present for as long as the daemon runs and the storage of the
/// workforce does.
pub(crate) async fn run_commits(
    daemon: Weak<Daemon>,
    workforce: WorkforceId,
    loaded: Arc<Loaded>,
    fetcher: Arc<dyn CommitFetcher>,
) {
    let Some(mut halted) = daemon.upgrade().map(|strong| strong.halted.subscribe()) else {
        return;
    };
    let mut heads = loaded.storage.watch_heads();
    let mut retries = BTreeMap::new();
    loop {
        let Some(strong) = daemon.upgrade() else {
            return;
        };
        let wait = reconcile(&strong, workforce, &loaded, &fetcher, &mut retries)
            .await
            .unwrap_or(LOOK_AGAIN);
        drop(strong);
        tokio::select! {
            changed = heads.changed() => {
                if changed.is_err() {
                    return;
                }
                tokio::time::sleep(SETTLE).await;
                heads.borrow_and_update();
            }
            () = async { let _ = halted.wait_for(|halted| *halted).await; } => return,
            () = tokio::time::sleep(wait) => {}
        }
    }
}

impl Daemon {
    /// Fails with [`SessionError::AwaitingCommit`] unless `commit` is in the bare repository of
    /// `workforce`: what starts on a commit waits until it has arrived.
    pub(crate) async fn require_commit(
        &self,
        workforce: WorkforceId,
        commit: CommitId,
    ) -> Result<(), SessionError> {
        let repository = self.bare_repository(workforce);
        let present =
            tokio::task::spawn_blocking(move || GitCli::open(&repository)?.has_commit(commit))
                .await??;
        if present {
            Ok(())
        } else {
            Err(SessionError::AwaitingCommit(commit))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};

    use tempfile::TempDir;
    use wf_core::event::{Commented, EventBody};
    use wf_core::identity::Actor;
    use wf_core::item::ItemState;
    use wf_core::testing::*;
    use wf_net::Network;
    use wf_store::{RedbStore, Replica};
    use wf_vcs::testing::TestRepository;

    use super::*;
    use crate::{DataDir, Launch};

    /// Fetches from repositories on disk, by device, as long as it is online.
    struct Repositories {
        bare: PathBuf,
        repositories: BTreeMap<DeviceKey, PathBuf>,
        online: AtomicBool,
        asked: Mutex<Vec<DeviceKey>>,
    }

    impl CommitFetcher for Repositories {
        fn fetch(&self, device: DeviceKey, commit: CommitId) -> BoxFuture<Result<(), String>> {
            self.asked.lock().unwrap().push(device);
            let result = if !self.online.load(Ordering::SeqCst) {
                Err("offline".to_string())
            } else if let Some(path) = self.repositories.get(&device) {
                GitCli::open(&self.bare)
                    .and_then(|bare| bare.fetch_commit_from_path(path, commit))
                    .map_err(|error| error.to_string())
            } else {
                Err("unknown device".to_string())
            };
            Box::pin(async move { result })
        }
    }

    /// A daemon on device 0 of a workforce with two devices, and a repository for device 1.
    struct Scene {
        _directory: TempDir,
        fixture: Fixture,
        daemon: Arc<Daemon>,
        loaded: Arc<Loaded>,
        remote: TestRepository,
        fetcher: Arc<Repositories>,
        retries: BTreeMap<CommitId, Backoff>,
        clock: u64,
    }

    impl Scene {
        async fn new() -> Self {
            let directory = TempDir::new().unwrap();
            let fixture = Fixture::new(2);
            let workforce = fixture.workforce();
            let store_directory = directory.path().join(workforce.to_string());
            std::fs::create_dir_all(&store_directory).unwrap();
            let mut replica = Replica::open(
                RedbStore::open(store_directory.join(crate::data_dir::STORE_FILE)).unwrap(),
                workforce,
            )
            .unwrap();
            replica.receive(fixture.genesis.clone()).unwrap();
            drop(replica);
            let daemon = Daemon::start(
                Launch {
                    data_dir: DataDir::new(directory.path().to_path_buf()),
                    socket: directory.path().join("daemon.sock"),
                    network: Network::Direct,
                    harnesses: Default::default(),
                    machine_config: None,
                    wf_binary: None,
                },
                fixture.devices[0].clone(),
            )
            .await
            .unwrap();
            let loaded = daemon.workforce(workforce).await.unwrap();
            let remote = TestRepository::new();
            let fetcher = Arc::new(Repositories {
                bare: daemon.bare_repository(workforce),
                repositories: BTreeMap::from([(fixture.devices[1].key(), remote.directory())]),
                online: AtomicBool::new(true),
                asked: Mutex::default(),
            });
            Self {
                _directory: directory,
                fixture,
                daemon,
                loaded,
                remote,
                fetcher,
                retries: BTreeMap::new(),
                clock: 2_000,
            }
        }

        /// Writes `body` as `actor` on device `device`.
        async fn write(
            &mut self,
            device: usize,
            actor: Actor,
            body: EventBody,
        ) -> wf_core::event::Event {
            self.clock += 1;
            let (signer, now) = (self.fixture.devices[device].clone(), self.clock);
            self.loaded
                .storage
                .with(move |replica| replica.author(&signer, actor, now, body))
                .await
                .unwrap()
                .unwrap()
        }

        async fn work_session(&mut self, item: ItemId) -> SessionId {
            let founder = self.fixture.founder_actor();
            let started = self.write(1, founder, start_session("coder", item)).await;
            started.id().into()
        }

        async fn name_head(&mut self, session: SessionId, item: ItemId, head: CommitId) {
            let actor = self.fixture.agent_actor("coder", session);
            let comment = EventBody::Commented(Commented {
                head: Some(head),
                ..Commented::plain(item, "step done")
            });
            self.write(1, actor, comment).await;
        }

        /// An item with the agent defined, on device 0.
        async fn item(&mut self) -> ItemId {
            let founder = self.fixture.founder_actor();
            self.write(0, founder.clone(), define_agent("coder")).await;
            self.write(0, founder, create("task", "work"))
                .await
                .id()
                .into()
        }

        async fn reconcile(&mut self) -> Option<Duration> {
            let fetcher: Arc<dyn CommitFetcher> = Arc::clone(&self.fetcher) as _;
            reconcile(
                &self.daemon,
                self.fixture.workforce(),
                &self.loaded,
                &fetcher,
                &mut self.retries,
            )
            .await
        }

        fn bare(&self) -> GitCli {
            GitCli::open(&self.daemon.bare_repository(self.fixture.workforce())).unwrap()
        }
    }

    #[tokio::test]
    async fn the_head_a_device_names_is_fetched_from_that_device_and_becomes_the_branch() {
        let mut scene = Scene::new().await;
        let item = scene.item().await;
        let session = scene.work_session(item).await;
        let head = scene.remote.work("wf/work", "a.txt", "a\n");
        scene.name_head(session, item, head).await;
        assert!(!scene.bare().has_commit(head).unwrap());

        let again = scene.reconcile().await;
        assert_eq!(again, None, "nothing is missing any more");
        assert!(scene.bare().has_commit(head).unwrap());
        assert_eq!(scene.bare().branch_head(&item_branch(item)).unwrap(), head);
        assert!(scene.loaded.commits.awaited().is_empty());
        let asked = scene.fetcher.asked.lock().unwrap().clone();
        assert_eq!(asked.first(), Some(&scene.fixture.devices[1].key()));
    }

    #[tokio::test]
    async fn a_commit_that_cannot_be_fetched_is_awaited_and_holds_back_what_starts_on_it() {
        let mut scene = Scene::new().await;
        let item = scene.item().await;
        let session = scene.work_session(item).await;
        let head = scene.remote.work("wf/work", "a.txt", "a\n");
        scene.name_head(session, item, head).await;
        scene.fetcher.online.store(false, Ordering::SeqCst);

        let again = scene.reconcile().await;
        assert!(again.is_some(), "it looks again");
        let awaited = scene.loaded.commits.awaited();
        assert_eq!(
            awaited.iter().map(|named| named.commit).collect::<Vec<_>>(),
            vec![head]
        );
        let workforce = scene.fixture.workforce();
        let waiting = scene.daemon.require_commit(workforce, head).await;
        assert!(
            matches!(waiting, Err(SessionError::AwaitingCommit(commit)) if commit == head),
            "{waiting:?}"
        );

        // Retried when its time comes, and once the device is back the commit arrives.
        scene.fetcher.online.store(true, Ordering::SeqCst);
        scene.retries.clear();
        assert_eq!(scene.reconcile().await, None);
        scene.daemon.require_commit(workforce, head).await.unwrap();
        assert!(scene.loaded.commits.awaited().is_empty());
    }

    #[tokio::test]
    async fn the_work_of_a_session_that_lost_the_claim_is_kept_apart_unless_it_is_an_ancestor() {
        let mut scene = Scene::new().await;
        let item = scene.item().await;
        let holder = scene.work_session(item).await;
        let first = scene.remote.work("wf/work", "a.txt", "a\n");
        let worktree = scene.remote.worktree_path("wf/work");
        scene.remote.write(&worktree, "b.txt", "b\n");
        let second = scene.remote.commit(&worktree, "second");
        scene.name_head(holder, item, second).await;
        // Started later, so it lost the claim: one head is in the branch's history, one is not.
        let behind = scene.work_session(item).await;
        scene.name_head(behind, item, first).await;
        let lost = scene.work_session(item).await;
        let elsewhere = scene.remote.work("wf/other", "c.txt", "c\n");
        scene.name_head(lost, item, elsewhere).await;

        assert_eq!(scene.reconcile().await, None);
        let bare = scene.bare();
        assert_eq!(bare.branch_head(&item_branch(item)).unwrap(), second);
        assert_eq!(bare.refs_apart().unwrap(), vec![diverged_ref(lost)]);
    }

    #[tokio::test]
    async fn a_cancelled_item_loses_its_refs() {
        let mut scene = Scene::new().await;
        let item = scene.item().await;
        let holder = scene.work_session(item).await;
        let head = scene.remote.work("wf/work", "a.txt", "a\n");
        scene.name_head(holder, item, head).await;
        let lost = scene.work_session(item).await;
        let elsewhere = scene.remote.work("wf/other", "c.txt", "c\n");
        scene.name_head(lost, item, elsewhere).await;
        scene.reconcile().await;
        assert_eq!(scene.bare().refs_apart().unwrap().len(), 1);
        assert_eq!(
            scene.bare().item_branches().unwrap(),
            vec![item_branch(item)]
        );

        let founder = scene.fixture.founder_actor();
        scene
            .write(0, founder, moved(item, ItemState::Cancelled))
            .await;
        scene.reconcile().await;
        assert!(scene.bare().refs_apart().unwrap().is_empty());
        assert!(scene.bare().item_branches().unwrap().is_empty());
    }
}

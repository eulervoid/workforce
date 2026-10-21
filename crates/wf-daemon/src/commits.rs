//! Keeping the commits the log names present on this device, and the refs that follow them.
//!
//! Whenever the log changes, the daemon lists the commits it names, starts fetching the ones this
//! repository lacks (from the device that wrote the event first, else from any other), and moves
//! the refs of the items: `wf/<item>` at the head of the session holding the claim, the heads of
//! sessions that lost it under `refs/wf/diverged/`, and none for items that are finished. Fetches
//! run beside all this, so one that hangs holds up nothing else. What no device could give is
//! tried again now and then and shown as unavailable.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use n0_future::boxed::BoxFuture;
use tokio::sync::{Notify, Semaphore};
use tokio::task::JoinSet;
use tracing::instrument::WithSubscriber;
use wf_core::commits::{AwaitedCommit, Fetching, ItemHeads, NamedCommit};
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
    awaited: Mutex<Vec<AwaitedCommit>>,
    /// Notified when commits arrived, so that what waited for them starts.
    pub(crate) arrived: Notify,
    /// Notified when commits were copied in here other than by a fetch, so that the refs pass
    /// looks again at once.
    copied: Notify,
}

impl Commits {
    /// The commits the log names that this device lacks.
    pub(crate) fn awaited(&self) -> Vec<AwaitedCommit> {
        self.awaited
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Says that commits were copied into the repository here other than by a fetch, as an import
    /// from an agent's repository does: if any were awaited, the refs pass looks at once, and
    /// tells what waited for them.
    pub(crate) fn copied_here(&self) {
        if !self.awaited().is_empty() {
            self.copied.notify_one();
        }
    }

    fn set_awaited(&self, awaited: Vec<AwaitedCommit>) {
        *self
            .awaited
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = awaited;
    }
}

/// How fetching one missing commit goes.
struct Tracked {
    backoff: Backoff,
    fetching: Fetching,
}

/// The fetches of the commits this device lacks: those running, and how each went so far.
pub(crate) struct Fetches {
    tracked: BTreeMap<CommitId, Tracked>,
    running: JoinSet<bool>,
    /// The commit each running task fetches.
    running_commits: HashMap<tokio::task::Id, CommitId>,
    slots: Arc<Semaphore>,
}

impl Fetches {
    pub(crate) fn new() -> Self {
        Self {
            tracked: BTreeMap::new(),
            running: JoinSet::new(),
            running_commits: HashMap::new(),
            slots: Arc::new(Semaphore::new(FETCHES_AT_ONCE)),
        }
    }

    fn is_running(&self, commit: CommitId) -> bool {
        self.running_commits
            .values()
            .any(|running| *running == commit)
    }

    /// Starts fetching `commit` from `devices`, one after the other until one has it.
    fn start(
        &mut self,
        commit: CommitId,
        devices: Vec<DeviceKey>,
        fetcher: &Arc<dyn CommitFetcher>,
    ) {
        let (fetcher, slots) = (Arc::clone(fetcher), Arc::clone(&self.slots));
        let task = self.running.spawn(
            async move {
                let _slot = slots.acquire_owned().await;
                for device in devices {
                    match fetcher.fetch(device, commit).await {
                        Ok(()) => return true,
                        Err(error) => tracing::debug!(
                            "could not fetch {} from {}: {error}",
                            commit.short(),
                            device.fmt_short()
                        ),
                    }
                }
                false
            }
            .with_current_subscriber(),
        );
        self.running_commits.insert(task.id(), commit);
    }

    /// Waits for the next fetch to end, and notes how it went. Pending while none runs.
    async fn next_done(&mut self) {
        let Some(done) = self.running.join_next_with_id().await else {
            return std::future::pending().await;
        };
        let (id, got) = match done {
            Ok((id, got)) => (id, got),
            Err(error) => (error.id(), false),
        };
        let Some(commit) = self.running_commits.remove(&id) else {
            return;
        };
        let Some(tracked) = self.tracked.get_mut(&commit) else {
            return;
        };
        let now = Instant::now();
        if got {
            tracked.backoff.record(true, now);
        } else {
            // Every device was asked: it is not to be had for now.
            tracked.backoff.wait_longest(now);
            if tracked.fetching == Fetching::Underway {
                tracked.fetching = Fetching::Unavailable {
                    since_ms: wf_store::now_ms(),
                };
            }
        }
    }

    /// Waits for every running fetch to end, for the tests.
    #[cfg(test)]
    async fn finish(&mut self) {
        while !self.running.is_empty() {
            self.next_done().await;
        }
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
    /// The items a work session of this device runs on: their branch is the session's, which
    /// its import moves.
    worked_here: BTreeSet<ItemId>,
}

fn read(state: &wf_core::state::State, own: DeviceKey) -> Reading {
    let heads = state.item_heads();
    let work_of = |item: ItemId| {
        state
            .sessions_of(item)
            .filter(|session| session.role() == SessionRole::Work)
    };
    let worked_here = heads
        .iter()
        .map(|heads| heads.item)
        .filter(|item| work_of(*item).any(|session| session.is_live() && session.device() == own))
        .collect::<BTreeSet<_>>();
    let finished = heads
        .iter()
        .filter(|heads| heads.finished)
        .map(|heads| Finished {
            item: heads.item,
            sessions: work_of(heads.item).map(|session| session.id()).collect(),
            busy: worked_here.contains(&heads.item),
        })
        .collect();
    Reading {
        named: state.named_commits(),
        heads,
        peers: peers_of(state, own),
        finished,
        worked_here,
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

/// One pass: starts fetching what is missing and is due, sets the refs, and tells what waited
/// when commits arrived. Returns how long to wait before another pass if something is still
/// missing. Waits for no fetch: [`Fetches::next_done`] does.
pub(crate) async fn reconcile(
    daemon: &Arc<Daemon>,
    workforce: WorkforceId,
    loaded: &Loaded,
    fetcher: &Arc<dyn CommitFetcher>,
    fetches: &mut Fetches,
) -> Option<Duration> {
    let own = daemon.device();
    daemon
        .ensure_bare_repository(workforce, &loaded.storage)
        .await;
    let bare = daemon.bare_repository(workforce);
    if !bare.join("HEAD").is_file() {
        return None;
    }
    // Read only once no other write of the refs is under way, so that none comes between the
    // reading and the refs set from it.
    let _writing = loaded.item_refs.lock().await;
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
    let arrived = fetches
        .tracked
        .keys()
        .any(|commit| !missing.contains(commit));
    fetches.tracked.retain(|commit, _| missing.contains(commit));
    wanted.retain(|named| missing.contains(&named.commit));

    let now = Instant::now();
    for named in &wanted {
        let tracked = fetches.tracked.entry(named.commit).or_insert(Tracked {
            backoff: Backoff::default(),
            fetching: Fetching::Underway,
        });
        if tracked.backoff.is_due(now) && !fetches.is_running(named.commit) {
            let devices = devices_for(named.author, own, &reading.peers);
            fetches.start(named.commit, devices, fetcher);
        }
    }
    loaded.commits.set_awaited(
        wanted
            .iter()
            .map(|named| AwaitedCommit {
                named: *named,
                fetching: fetches.tracked[&named.commit].fetching,
            })
            .collect(),
    );

    let workforce_directory = daemon.workforce_directory(workforce);
    let referring = tokio::task::spawn_blocking(move || {
        set_refs(&bare, &workforce_directory, &reading, &missing)
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
    if arrived {
        loaded.commits.arrived.notify_one();
    }
    if fetches.tracked.is_empty() {
        return None;
    }
    let waits = fetches
        .tracked
        .values()
        .map(|tracked| tracked.backoff.wait(Instant::now()));
    Some(waits.min().map_or(LOOK_AGAIN, |wait| wait.min(LOOK_AGAIN)))
}

/// Moves the refs of the items to the heads the log names, for those whose commits are here,
/// and removes what finished items left: nothing else removes them. Runs while it holds
/// [`Loaded::item_refs`], which the imports and starts that move a branch hold too. Each branch
/// moves only from where it was read, and not at all while a work session of this device runs on
/// the item.
fn set_refs(
    bare: &std::path::Path,
    workforce_directory: &std::path::Path,
    reading: &Reading,
    missing: &BTreeSet<CommitId>,
) -> Result<(), VcsError> {
    let git = GitCli::open(bare)?;
    let apart: BTreeSet<String> = git.refs_apart()?.into_iter().collect();
    let branches: BTreeSet<String> = git.item_branches()?.into_iter().collect();
    for item in reading.heads.iter().filter(|item| !item.finished) {
        let Some(holder) = item
            .holder
            .filter(|holder| !missing.contains(&holder.commit))
        else {
            continue;
        };
        if !reading.worked_here.contains(&item.item) {
            let branch = match git.branch_head(&item_branch(item.item)) {
                Ok(branch) => Some(branch),
                Err(VcsError::NoBranch(_)) => None,
                Err(error) => return Err(error),
            };
            if branch != Some(holder.commit) {
                git.move_item_branch(item.item, branch, holder.commit)?;
            }
        }
        // The holder's work, and work that became the same as it, is the branch's.
        let others: BTreeSet<SessionId> = item.others.iter().map(|other| other.session).collect();
        for session in item
            .sessions
            .iter()
            .filter(|session| !others.contains(session))
        {
            if apart.contains(&diverged_ref(*session)) {
                git.drop_apart(*session)?;
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
            } else {
                git.keep_apart(other.session, other.commit)?;
            }
        }
    }
    for done in reading.finished.iter().filter(|done| !done.busy) {
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
    let mut fetches = Fetches::new();
    loop {
        let Some(strong) = daemon.upgrade() else {
            return;
        };
        let wait = reconcile(&strong, workforce, &loaded, &fetcher, &mut fetches)
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
            // A fetch ended: the next pass sets the refs and tells what waited for it.
            () = fetches.next_done() => {}
            () = loaded.commits.copied.notified() => {}
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
    use wf_core::session::EndReason;
    use wf_core::testing::*;
    use wf_net::Network;
    use wf_store::{RedbStore, Replica};
    use wf_vcs::testing::TestRepository;

    use super::*;
    use crate::{DataDir, Launch};

    /// Fetches from repositories on disk, by device, as long as it is online. A fetch of a commit
    /// in `hanging` never ends.
    struct Repositories {
        bare: PathBuf,
        repositories: BTreeMap<DeviceKey, PathBuf>,
        online: AtomicBool,
        asked: Mutex<Vec<DeviceKey>>,
        hanging: Mutex<BTreeSet<CommitId>>,
    }

    impl CommitFetcher for Repositories {
        fn fetch(&self, device: DeviceKey, commit: CommitId) -> BoxFuture<Result<(), String>> {
            self.asked.lock().unwrap().push(device);
            if self.hanging.lock().unwrap().contains(&commit) {
                return Box::pin(std::future::pending());
            }
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
        fetches: Fetches,
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
                hanging: Mutex::default(),
            });
            Self {
                _directory: directory,
                fixture,
                daemon,
                loaded,
                remote,
                fetcher,
                fetches: Fetches::new(),
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

        /// One pass, which starts the fetches that are due and returns at once.
        async fn pass(&mut self) -> Option<Duration> {
            let fetcher: Arc<dyn CommitFetcher> = Arc::clone(&self.fetcher) as _;
            reconcile(
                &self.daemon,
                self.fixture.workforce(),
                &self.loaded,
                &fetcher,
                &mut self.fetches,
            )
            .await
        }

        /// A pass, the fetches it started, and the pass that follows them.
        async fn reconcile(&mut self) -> Option<Duration> {
            self.pass().await;
            self.fetches.finish().await;
            self.pass().await
        }

        /// Makes every commit that waits due again.
        fn retry_now(&mut self) {
            for tracked in self.fetches.tracked.values_mut() {
                tracked.backoff = Backoff::default();
            }
        }

        async fn end(&mut self, session: SessionId, reason: EndReason) {
            let founder = self.fixture.founder_actor();
            self.write(1, founder, end_session(session, reason)).await;
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

        scene.pass().await;
        let awaited = scene.loaded.commits.awaited();
        assert_eq!(
            awaited
                .iter()
                .map(|awaited| (awaited.named.commit, awaited.fetching))
                .collect::<Vec<_>>(),
            vec![(head, Fetching::Underway)]
        );
        scene.fetches.finish().await;
        let again = scene.pass().await;
        assert!(
            again.is_some_and(|wait| wait <= LOOK_AGAIN),
            "it looks again: {again:?}"
        );
        // No device had it: shown as unavailable, and asked for again only after a long while.
        let awaited = scene.loaded.commits.awaited();
        assert!(
            matches!(
                awaited.as_slice(),
                [AwaitedCommit { named, fetching: Fetching::Unavailable { .. } }] if named.commit == head
            ),
            "{awaited:?}"
        );
        assert!(
            !scene.fetches.tracked[&head]
                .backoff
                .is_due(Instant::now() + LOOK_AGAIN)
        );
        let asked = scene.fetcher.asked.lock().unwrap().len();
        scene.pass().await;
        scene.fetches.finish().await;
        assert_eq!(
            scene.fetcher.asked.lock().unwrap().len(),
            asked,
            "not due yet"
        );
        let workforce = scene.fixture.workforce();
        let waiting = scene.daemon.require_commit(workforce, head).await;
        assert!(
            matches!(waiting, Err(SessionError::AwaitingCommit(commit)) if commit == head),
            "{waiting:?}"
        );

        // Retried when its time comes, and once the device is back the commit arrives.
        scene.fetcher.online.store(true, Ordering::SeqCst);
        scene.retry_now();
        assert_eq!(scene.reconcile().await, None);
        scene.daemon.require_commit(workforce, head).await.unwrap();
        assert!(scene.loaded.commits.awaited().is_empty());
    }

    #[tokio::test]
    async fn an_awaited_commit_copied_in_here_is_told_of_at_once() {
        let mut scene = Scene::new().await;
        let item = scene.item().await;
        let session = scene.work_session(item).await;
        let head = scene.remote.work("wf/work", "a.txt", "a\n");
        scene.name_head(session, item, head).await;
        // The daemon's own refs pass finds that no device can give it, and leaves it for a while.
        let deadline = Instant::now() + Duration::from_secs(10);
        while !matches!(
            scene.loaded.commits.awaited().as_slice(),
            [AwaitedCommit {
                fetching: Fetching::Unavailable { .. },
                ..
            }]
        ) {
            assert!(Instant::now() < deadline, "the head is not awaited");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // It turns up without a fetch, as the commits of a session do when they are imported. The
        // pass that finds it here tells the dispatcher, which waits on `arrived`.
        scene
            .bare()
            .fetch_commit_from_path(&scene.remote.directory(), head)
            .unwrap();
        scene.loaded.commits.copied_here();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !scene.loaded.commits.awaited().is_empty() {
            assert!(
                Instant::now() < deadline,
                "the refs pass looks again at once, not after {LOOK_AGAIN:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
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

    #[tokio::test]
    async fn a_fetch_that_hangs_holds_up_no_ref_and_no_other_fetch() {
        let mut scene = Scene::new().await;
        let stuck_item = scene.item().await;
        let stuck_session = scene.work_session(stuck_item).await;
        let stuck = scene.remote.work("wf/stuck", "s.txt", "s\n");
        scene.fetcher.hanging.lock().unwrap().insert(stuck);
        scene.name_head(stuck_session, stuck_item, stuck).await;
        scene.pass().await;

        let item = scene.item().await;
        let session = scene.work_session(item).await;
        let head = scene.remote.work("wf/work", "a.txt", "a\n");
        scene.name_head(session, item, head).await;
        let passed = tokio::time::timeout(Duration::from_secs(10), async {
            scene.pass().await;
            // The other fetch ends while the stuck one still runs.
            scene.fetches.next_done().await;
            scene.pass().await
        })
        .await
        .expect("a pass does not wait for a fetch");
        assert!(passed.is_some(), "the stuck commit is still missing");
        assert_eq!(scene.bare().branch_head(&item_branch(item)).unwrap(), head);
        assert!(scene.fetches.is_running(stuck));
        let awaited = scene.loaded.commits.awaited();
        assert_eq!(
            awaited
                .iter()
                .map(|awaited| awaited.named.commit)
                .collect::<Vec<_>>(),
            vec![stuck]
        );
    }

    #[tokio::test]
    async fn a_done_item_loses_its_refs_and_its_repository() {
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
        let workforce = scene.fixture.workforce();
        let repository = item_repository(&scene.daemon.workforce_directory(workforce), item);
        std::fs::create_dir_all(&repository).unwrap();

        scene.end(lost, EndReason::Released).await;
        scene.end(holder, EndReason::Submitted).await;
        let founder = scene.fixture.founder_actor();
        scene.write(0, founder, moved(item, ItemState::Done)).await;
        assert_eq!(scene.reconcile().await, None);
        assert!(scene.bare().refs_apart().unwrap().is_empty());
        assert!(scene.bare().item_branches().unwrap().is_empty());
        assert!(!repository.exists());
        assert!(
            scene.bare().has_commit(head).unwrap(),
            "the commits stay, only the refs go"
        );
    }

    #[tokio::test]
    async fn the_end_leaves_the_branch_to_the_refs_pass_so_a_stale_pass_brings_nothing_back() {
        let mut scene = Scene::new().await;
        let item = scene.item().await;
        let holder = scene.work_session(item).await;
        let head = scene.remote.work("wf/work", "a.txt", "a\n");
        scene.name_head(holder, item, head).await;
        scene.reconcile().await;
        assert_eq!(scene.bare().branch_head(&item_branch(item)).unwrap(), head);

        // A pass reads the log just before the end, and sets the refs just after it.
        let own = scene.daemon.device();
        let stale = scene
            .loaded
            .storage
            .with(move |replica| read(replica.state(), own))
            .await
            .unwrap();
        scene.end(holder, EndReason::Submitted).await;
        let founder = scene.fixture.founder_actor();
        scene.write(0, founder, moved(item, ItemState::Done)).await;
        // The end removes nothing itself: the refs pass is the only one that does, and each reads
        // the log only once no other write of the refs is under way.
        let workforce = scene.fixture.workforce();
        set_refs(
            &scene.daemon.bare_repository(workforce),
            &scene.daemon.workforce_directory(workforce),
            &stale,
            &BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(scene.bare().branch_head(&item_branch(item)).unwrap(), head);
        assert_eq!(scene.reconcile().await, None);
        assert!(scene.bare().item_branches().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_branch_follows_the_claim_when_it_passes_and_the_new_holder_is_not_set_apart() {
        let mut scene = Scene::new().await;
        let item = scene.item().await;
        let first = scene.work_session(item).await;
        let first_head = scene.remote.work("wf/first", "a.txt", "a\n");
        scene.name_head(first, item, first_head).await;
        // Started later, so it lost the claim, and its work went elsewhere.
        let second = scene.work_session(item).await;
        let second_head = scene.remote.work("wf/second", "b.txt", "b\n");
        scene.name_head(second, item, second_head).await;
        scene.reconcile().await;
        assert_eq!(
            scene.bare().branch_head(&item_branch(item)).unwrap(),
            first_head
        );
        assert_eq!(
            scene.bare().refs_apart().unwrap(),
            vec![diverged_ref(second)]
        );

        // The first lets go, so the second holds the claim: the branch is its work now, and the
        // work of the first is what is set apart.
        scene.end(first, EndReason::Released).await;
        assert_eq!(scene.reconcile().await, None);
        assert_eq!(
            scene.bare().branch_head(&item_branch(item)).unwrap(),
            second_head
        );
        assert_eq!(
            scene.bare().refs_apart().unwrap(),
            vec![diverged_ref(first)]
        );
    }

    #[tokio::test]
    async fn the_branch_of_an_item_a_session_of_this_device_works_on_is_left_to_it() {
        let mut scene = Scene::new().await;
        let item = scene.item().await;
        // A session of this device that lost the claim on another device's, after a partition.
        let founder = scene.fixture.founder_actor();
        let here: SessionId = scene
            .write(0, founder, start_session("coder", item))
            .await
            .id()
            .into();
        let local_head = scene.remote.work("wf/here", "h.txt", "h\n");
        scene
            .bare()
            .fetch_commit_from_path(&scene.remote.directory(), local_head)
            .unwrap();
        assert!(
            scene
                .bare()
                .move_item_branch(item, None, local_head)
                .unwrap()
        );
        let elsewhere = scene.work_session(item).await;
        let head = scene.remote.work("wf/work", "a.txt", "a\n");
        scene.name_head(elsewhere, item, head).await;

        scene.reconcile().await;
        assert!(scene.bare().has_commit(head).unwrap());
        assert_eq!(
            scene.bare().branch_head(&item_branch(item)).unwrap(),
            local_head,
            "the import of the session here moves it"
        );

        let founder = scene.fixture.founder_actor();
        scene
            .write(0, founder, end_session(here, EndReason::Outclaimed))
            .await;
        scene.reconcile().await;
        assert_eq!(scene.bare().branch_head(&item_branch(item)).unwrap(), head);
    }
}

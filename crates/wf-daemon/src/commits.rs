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

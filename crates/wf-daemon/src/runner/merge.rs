//! Merging reviewed work into the target branch.

use std::collections::{BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};

use tokio::sync::Notify;
use wf_core::event::{Commented, EventBody, MergeClaimed, Merged};
use wf_core::id::{CommentId, HashId, ItemId, SessionId, WorkforceId};
use wf_core::identity::{Actor, PrincipalKey};
use wf_core::item::{ItemEntry, ItemState};
use wf_core::lifecycle::{MergeFailure, MergePlan};
use wf_core::policy::MergeMode;
use wf_core::review::PendingMerge;
use wf_core::session::CommitId;
use wf_vcs::{CommitSigning, GitCli, MergeOutcome, MergeTree};

use super::check::CheckStage;
use super::target::TargetAt;
use super::{SessionError, blocking};
use crate::machine_config::MergeSigningConfig;
use crate::{Daemon, Loaded};

/// How often a merge that found the target moved is tried again before giving up for now.
const MERGE_ATTEMPTS: usize = 3;

/// Whether a merge is the first try of a piece of work, or one more try of work that waited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Attempt {
    /// Made as the session that did the work ends, so the item moves for that session.
    First { session: SessionId },
    /// Waiting again is not worth another comment.
    Retry,
}

impl Attempt {
    pub(super) fn by_session(self) -> Option<SessionId> {
        match self {
            Attempt::First { session } => Some(session),
            Attempt::Retry => None,
        }
    }
}

/// Reviewed work to merge into the target branch, and what to do with its item afterwards.
#[derive(Clone)]
pub(super) struct MergeRequest {
    pub(super) item: ItemId,
    /// The reviewed commit.
    pub(super) commit: CommitId,
    /// The comment that says why, for that move.
    pub(super) because: Option<CommentId>,
    pub(super) attempt: Attempt,
}

/// The merges of one workforce that wait for its merge task, which makes them one after the
/// other, so that a slow merge holds up only the merges behind it.
#[derive(Default)]
pub(crate) struct MergeQueue {
    queued: Mutex<Queued>,
    added: Notify,
}

#[derive(Default)]
struct Queued {
    /// At most one request per item, in the order they came.
    requests: VecDeque<MergeRequest>,
    /// The items whose session is ending and will queue the first attempt: no retry of them is
    /// queued meanwhile.
    reserved: BTreeSet<ItemId>,
}

/// The first attempt at merging the work of a session that is ending, kept from the queue until
/// the session has ended. Until then no retry of the item is queued; dropped, it lets them in.
#[must_use]
pub(super) struct Reservation<'queue> {
    queue: &'queue MergeQueue,
    request: MergeRequest,
}

impl Reservation<'_> {
    pub(super) fn queue(self) {
        self.queue.push(self.request.clone());
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        self.queue.lock().reserved.remove(&self.request.item);
    }
}

impl MergeQueue {
    /// Holds the first attempt of a session that is ending until [`Reservation::queue`], and
    /// keeps retries of its item out until then, so that the move to `done` is the session's.
    pub(super) fn reserve(&self, request: MergeRequest) -> Reservation<'_> {
        let mut queued = self.lock();
        queued.reserved.insert(request.item);
        queued.requests.retain(|queued| queued.item != request.item);
        drop(queued);
        Reservation {
            queue: self,
            request,
        }
    }

    /// Queues a merge, unless one of the item is queued already or reserved for a first attempt.
    /// A first attempt takes the place of a queued retry, so that the session that did the work
    /// still hears of it.
    pub(super) fn push(&self, request: MergeRequest) {
        let mut queued = self.lock();
        if request.attempt == Attempt::Retry && queued.reserved.contains(&request.item) {
            return;
        }
        match queued
            .requests
            .iter_mut()
            .find(|queued| queued.item == request.item)
        {
            Some(queued) => {
                if let Attempt::First { .. } = request.attempt {
                    *queued = request;
                }
            }
            None => queued.requests.push_back(request),
        }
        drop(queued);
        self.added.notify_one();
    }

    /// The next merge to make, once there is one.
    async fn next(&self) -> MergeRequest {
        loop {
            if let Some(request) = self.lock().requests.pop_front() {
                return request;
            }
            self.added.notified().await;
        }
    }

    /// Makes the queued merges with `merge`, one at a time, until `stop` completes. A merge that
    /// has begun is finished first.
    async fn run<F: Future<Output = ()>>(
        &self,
        stop: impl Future<Output = ()>,
        mut merge: impl FnMut(MergeRequest) -> F,
    ) {
        let mut stop = std::pin::pin!(stop);
        loop {
            let request = tokio::select! {
                request = self.next() => request,
                () = &mut stop => return,
            };
            merge(request).await;
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Queued> {
        self.queued
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Makes the merges of a workforce for as long as the daemon runs and the storage of the
/// workforce does.
pub(crate) async fn run_merger(daemon: Weak<Daemon>, workforce: WorkforceId, loaded: Arc<Loaded>) {
    let Some(mut halted) = daemon.upgrade().map(|strong| strong.halted.subscribe()) else {
        return;
    };
    let mut heads = loaded.storage.watch_heads();
    let stop = async {
        tokio::select! {
            _ = halted.wait_for(|halted| *halted) => {}
            () = async { while heads.changed().await.is_ok() {} } => {}
        }
    };
    let (daemon, loaded) = (&daemon, &loaded);
    let merge = move |request: MergeRequest| async move {
        let item = request.item;
        let Some(daemon) = daemon.upgrade() else {
            return;
        };
        if let Err(error) = daemon.merge_item(workforce, loaded, request).await {
            tracing::warn!(
                "could not merge #{}: {}",
                item.short(),
                wf_core::error_chain(&error)
            );
        }
    };
    loaded.merges.run(stop, merge).await;
}

impl Daemon {
    /// Merges reviewed work into the target branch and finishes the item. What cannot be merged
    /// now leaves the item where the principal or a later session will see it.
    async fn merge_item(
        &self,
        workforce: WorkforceId,
        loaded: &Loaded,
        request: MergeRequest,
    ) -> Result<(), SessionError> {
        let MergeRequest {
            item,
            commit,
            because,
            attempt,
        } = request;
        let state = self.snapshot(loaded).await?;
        let ItemEntry::Live(live) = state.item(item).ok_or(SessionError::UnknownItem(item))? else {
            return Err(SessionError::Archived(item));
        };
        let policy = &state.policy().ok_or(SessionError::NoPolicy)?.policy;
        let plan = MergePlan::of(live, policy, commit);
        if plan == MergePlan::Nothing {
            return Ok(());
        }
        // Queued while an earlier merge of the item ran, which sent it back to work or left it
        // waiting on the principal, as failed checks do.
        let pending = PendingMerge { item, commit };
        if attempt == Attempt::Retry && !state.pending_merges().contains(&pending) {
            return Ok(());
        }
        // The submission's checks, or anyone, left a blocker for the principal: it waits for them.
        if live.has_open_blocker() {
            return Ok(());
        }
        let principal = self.principal_of_device(&state)?;
        if plan == MergePlan::MoveToDone {
            self.move_item(
                loaded,
                principal,
                item,
                ItemState::Done,
                because,
                attempt.by_session(),
            )
            .await?;
            return Ok(());
        }
        // Another device that is there holds the merge: it makes it.
        if !self
            .hold_merge(loaded, principal, item, commit, Claiming::Claim)
            .await?
        {
            return Ok(());
        }
        let target = policy.merge.target.clone();
        let message = format!("Merge #{}: {}", HashId::short(&item), live.title());
        let open = state.after_failed_merge(item, MergeFailure::Conflict);
        let comment = |body: String| {
            self.author(
                loaded,
                Actor::Daemon(principal),
                EventBody::Commented(Commented::plain(item, body)),
            )
        };

        // A merge that cannot be made leaves the item in `review`, where the session that did the
        // work moved it and the dispatcher tries again.
        let cannot_merge = |why: String| {
            let note = format!("Could not merge into {target}: {why}. The daemon tries again.");
            let comment = &comment;
            async move {
                if attempt != Attempt::Retry {
                    comment(note).await?;
                }
                Ok::<_, SessionError>(())
            }
        };
        self.ensure_bare_repository(workforce, &loaded.storage)
            .await;
        let repository = self.bare_repository(workforce);
        let signing = match self.merge_signing() {
            Ok(signing) => signing,
            Err(error) => return cannot_merge(wf_core::error_chain(&error)).await,
        };
        let checks = match policy.merge.mode {
            MergeMode::Local => policy.merge.checks.as_slice(),
        };
        let mut outcome = MergeOutcome::TargetMoved;
        // The tip the merge moved the target from; none for work found landed already.
        let mut onto = None;
        // Held from moving the target until the merge is written, so it is not taken for a push.
        let mut moving = None;
        for _ in 0..MERGE_ATTEMPTS {
            onto = None;
            moving = None;
            // A merge starts only on the tip the log names, or a fork would be made on purpose.
            if self.follow_target(workforce, loaded).await? == TargetAt::Elsewhere {
                return Ok(());
            }
            let made = {
                let (repository, target) = (repository.clone(), target.clone());
                blocking(move || GitCli::open(&repository)?.merge_tree(&target, commit))
                    .await
                    .and_then(|made| made.map_err(SessionError::from))
            };
            let made = match made {
                Ok(made) => made,
                Err(error) => {
                    cannot_merge(wf_core::error_chain(&error)).await?;
                    return Err(error);
                }
            };
            outcome = match made {
                MergeTree::Landed { commit } => MergeOutcome::Merged { commit },
                MergeTree::Conflict { paths } => MergeOutcome::Conflict { paths },
                MergeTree::Clean { base, tree } => {
                    onto = Some(base);
                    let failed = match self
                        .check_merge(
                            workforce,
                            &repository,
                            checks,
                            base,
                            tree,
                            CheckStage::Merge,
                        )
                        .await
                    {
                        Ok(failed) => failed,
                        Err(error) => {
                            cannot_merge(wf_core::error_chain(&error)).await?;
                            return Err(error);
                        }
                    };
                    if !failed.is_empty() {
                        self.checks_failed(loaded, principal, item, attempt, &target, failed)
                            .await?;
                        return Ok(());
                    }
                    // Another device's claim, which came while the checks ran, holds it now.
                    if !self
                        .hold_merge(loaded, principal, item, commit, Claiming::Keep)
                        .await?
                    {
                        return Ok(());
                    }
                    moving = Some(loaded.target.seen.lock().await);
                    let (repository, target, message, signing) = (
                        repository.clone(),
                        target.clone(),
                        message.clone(),
                        signing.clone(),
                    );
                    let merged = blocking(move || {
                        GitCli::open(&repository)?
                            .commit_merge(&target, base, tree, commit, &message, &signing)
                    })
                    .await
                    .and_then(|merged| merged.map_err(SessionError::from));
                    match merged {
                        Ok(outcome) => outcome,
                        Err(error) => {
                            cannot_merge(wf_core::error_chain(&error)).await?;
                            return Err(error);
                        }
                    }
                }
            };
            // The target moved while it was checked or merged: the new merge is checked again.
            if outcome != MergeOutcome::TargetMoved {
                break;
            }
        }
        match outcome {
            MergeOutcome::Merged { commit: merged } => {
                self.author(
                    loaded,
                    Actor::Daemon(principal),
                    EventBody::Merged(Merged {
                        item,
                        reviewed: commit,
                        commit: merged,
                        onto,
                    }),
                )
                .await?;
                if let Some(mut seen) = moving.take() {
                    *seen = Some(merged);
                }
                // The refs pass removes its branch and repository: a merged item is finished.
                self.move_item(
                    loaded,
                    principal,
                    item,
                    ItemState::Done,
                    because,
                    attempt.by_session(),
                )
                .await?;
            }
            MergeOutcome::Conflict { paths } => {
                let written = comment(format!(
                    "Merging into {target} conflicts in {}. The next session resolves it: its submission rebases the branch onto {target}.",
                    paths.join(", ")
                ))
                .await?;
                if let Some(open) = open {
                    let because = Some(CommentId::from(written.id()));
                    self.move_item(loaded, principal, item, open, because, attempt.by_session())
                        .await?;
                }
            }
            MergeOutcome::TargetMoved if attempt == Attempt::Retry => {}
            MergeOutcome::TargetMoved => {
                comment(format!("Waiting to merge: {target} kept moving.")).await?;
            }
        }
        Ok(())
    }
}

/// Whether a device that holds no claim on a merge claims it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Claiming {
    Claim,
    /// Only asks whether its claim still holds.
    Keep,
}

impl Daemon {
    /// Whether this device holds the merge of the reviewed `commit` of `item`: its claim is the
    /// earliest of a device that is there. With `Claiming::Claim` it claims a merge nobody holds,
    /// if it meets `[merge] requires`.
    pub(super) async fn hold_merge(
        &self,
        loaded: &Loaded,
        principal: PrincipalKey,
        item: ItemId,
        commit: CommitId,
        claiming: Claiming,
    ) -> Result<bool, SessionError> {
        let (own, presence, now_ms) = (self.device(), self.presence_now(), wf_store::now_ms());
        let (holder, may_merge) = self
            .read(loaded, move |state| {
                let holder = state
                    .merge_holder(item, commit, own, &presence, now_ms)
                    .map(|claim| claim.device);
                (holder, state.may_merge(own))
            })
            .await?;
        match (holder, claiming) {
            (Some(device), Claiming::Claim | Claiming::Keep) => Ok(device == own),
            (None, Claiming::Keep) => Ok(false),
            // Its hardware does not meet `[merge] requires`: a device that does merges it.
            (None, Claiming::Claim) if !may_merge => Ok(false),
            (None, Claiming::Claim) => {
                self.author(
                    loaded,
                    Actor::Daemon(principal),
                    EventBody::MergeClaimed(MergeClaimed { item, commit }),
                )
                .await?;
                Ok(true)
            }
        }
    }

    /// How the merge commit is signed, as the machine config says now.
    fn merge_signing(&self) -> Result<CommitSigning, SessionError> {
        Ok(match self.machine_config()?.commits.merge_signing {
            MergeSigningConfig::AsConfigured => CommitSigning::AsConfigured,
            MergeSigningConfig::Unsigned => CommitSigning::Unsigned,
            MergeSigningConfig::Device => CommitSigning::Key(self.device_signing_key()?),
        })
    }

    /// The device's key as an SSH key file that `ssh-keygen` signs merges with, written beside the
    /// device key, readable only by its owner.
    fn device_signing_key(&self) -> Result<PathBuf, SessionError> {
        let path = self.launch.data_dir.device_ssh_key();
        let text = wf_vcs::signing::device_private_key(&self.device)?;
        if std::fs::read_to_string(&path).ok().as_deref() == Some(text.as_str()) {
            return Ok(path);
        }
        crate::data_dir::write_private(&path, text.as_bytes()).map_err(|source| {
            SessionError::Io {
                path: path.clone(),
                source,
            }
        })?;
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use tokio::sync::{Notify, mpsc};
    use wf_core::id::{ItemId, SessionId};
    use wf_core::session::CommitId;

    use super::{Attempt, MergeQueue, MergeRequest};

    fn request(seed: u8, attempt: Attempt) -> MergeRequest {
        MergeRequest {
            item: ItemId::from_bytes([seed; 32]),
            commit: CommitId::Git("00112233445566778899aabbccddeeff00112233".parse().unwrap()),
            because: None,
            attempt,
        }
    }

    #[tokio::test]
    async fn a_slow_merge_holds_up_only_the_merges_behind_it() {
        let queue = Arc::new(MergeQueue::default());
        let release = Arc::new(Notify::new());
        let (started, mut starts) = mpsc::unbounded_channel();
        let merger = tokio::spawn({
            let (queue, release) = (Arc::clone(&queue), Arc::clone(&release));
            async move {
                queue
                    .run(std::future::pending(), |request| {
                        let (release, started) = (Arc::clone(&release), started.clone());
                        async move {
                            started.send(request.item).unwrap();
                            release.notified().await;
                        }
                    })
                    .await;
            }
        });

        queue.push(request(1, Attempt::Retry));
        assert_eq!(starts.recv().await, Some(ItemId::from_bytes([1; 32])));
        // The dispatcher only queues: it goes on while the first merge has not ended.
        queue.push(request(2, Attempt::Retry));
        queue.push(request(2, Attempt::Retry));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(starts.try_recv().is_err(), "the second waits for the first");

        release.notify_one();
        assert_eq!(starts.recv().await, Some(ItemId::from_bytes([2; 32])));
        release.notify_one();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(starts.try_recv().is_err(), "an item is queued once");
        merger.abort();
    }

    #[tokio::test]
    async fn two_items_ready_at_once_are_merged_one_after_the_other() {
        let queue = MergeQueue::default();
        queue.push(request(1, Attempt::Retry));
        queue.push(request(2, Attempt::Retry));
        let running = AtomicUsize::new(0);
        let merged = std::sync::Mutex::new(Vec::new());
        let done = Notify::new();
        queue
            .run(done.notified(), |request| {
                let (running, merged, done) = (&running, &merged, &done);
                async move {
                    assert_eq!(running.fetch_add(1, Ordering::SeqCst), 0, "one at a time");
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    running.fetch_sub(1, Ordering::SeqCst);
                    let mut merged = merged.lock().unwrap();
                    merged.push(request.item);
                    if merged.len() == 2 {
                        done.notify_one();
                    }
                }
            })
            .await;
        assert_eq!(
            merged.into_inner().unwrap(),
            [ItemId::from_bytes([1; 32]), ItemId::from_bytes([2; 32])]
        );
    }

    #[test]
    fn the_first_attempt_takes_the_place_of_a_queued_retry() {
        let queue = MergeQueue::default();
        let session = SessionId::from_bytes([9; 32]);
        queue.push(request(1, Attempt::Retry));
        queue.push(request(1, Attempt::First { session }));
        queue.push(request(1, Attempt::Retry));
        let requests = &queue.lock().requests;
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].attempt, Attempt::First { session });
    }

    #[test]
    fn no_retry_is_queued_while_a_session_holds_its_first_attempt() {
        let queue = MergeQueue::default();
        let session = SessionId::from_bytes([9; 32]);
        let first_attempt = queue.reserve(request(1, Attempt::First { session }));
        queue.push(request(1, Attempt::Retry));
        assert!(queue.lock().requests.is_empty(), "the retry waits");
        first_attempt.queue();
        queue.push(request(1, Attempt::Retry));
        let requests = &queue.lock().requests;
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].attempt, Attempt::First { session });
    }

    #[test]
    fn a_dropped_reservation_lets_retries_in() {
        let queue = MergeQueue::default();
        let session = SessionId::from_bytes([9; 32]);
        drop(queue.reserve(request(1, Attempt::First { session })));
        queue.push(request(1, Attempt::Retry));
        assert_eq!(queue.lock().requests.len(), 1);
    }
}

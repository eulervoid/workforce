//! Merging reviewed work into the target branch.

use std::collections::{BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};

use tokio::sync::Notify;
use wf_core::event::{Comment, EventBody, Merged};
use wf_core::id::{CommentId, HashId, ItemId, SessionId, WorkforceId};
use wf_core::identity::Actor;
use wf_core::item::{ItemState, ItemView};
use wf_core::lifecycle::MergeFailure;
use wf_core::session::CommitId;
use wf_vcs::{CommitSigning, GitCli, MergeOutcome, VcsAdapter, item_branch, item_worktree};

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
    fn by_session(self) -> Option<SessionId> {
        match self {
            Attempt::First { session } => Some(session),
            Attempt::Retry => None,
        }
    }
}

/// Reviewed work to merge into the target branch, and what to do with its item afterwards.
pub(super) struct MergeRequest {
    pub(super) item: ItemId,
    /// The reviewed commit.
    pub(super) commit: CommitId,
    /// The comment that says why, for that move.
    pub(super) because: Option<CommentId>,
    pub(super) attempt: Attempt,
}

/// What became of a merge request, as far as the principal is concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MergeEnd {
    /// Merged, refused, made by another request already, or left for the daemon to try again.
    Handled,
    /// The target branch is checked out with changes: the merge waits on the principal.
    WaitsForCleanCheckout,
}

/// The merges of one workforce that wait for its merge task, which makes them one after the
/// other, so that a slow merge holds up only the merges behind it.
#[derive(Default)]
pub(crate) struct MergeQueue {
    queued: Mutex<Queued>,
    added: Notify,
    waiting_changed: Notify,
}

#[derive(Default)]
struct Queued {
    /// At most one request per item, in the order they came.
    requests: VecDeque<MergeRequest>,
    /// The items whose last merge found the target checked out with changes.
    waiting: BTreeSet<ItemId>,
}

impl MergeQueue {
    /// Queues a merge, unless one of the item is queued already. A first attempt takes the place
    /// of a queued retry, so that the session that did the work still hears of it.
    pub(super) fn push(&self, request: MergeRequest) {
        let mut queued = self.lock();
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

    /// The items whose merge waits for the target branch to be checked out without changes.
    pub(super) fn waiting(&self) -> BTreeSet<ItemId> {
        self.lock().waiting.clone()
    }

    /// Completes once [`MergeQueue::waiting`] changed.
    pub(super) async fn waiting_changed(&self) {
        self.waiting_changed.notified().await;
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

    fn record(&self, item: ItemId, end: MergeEnd) {
        let changed = match end {
            MergeEnd::Handled => self.lock().waiting.remove(&item),
            MergeEnd::WaitsForCleanCheckout => self.lock().waiting.insert(item),
        };
        if changed {
            self.waiting_changed.notify_one();
        }
    }

    /// Makes the queued merges with `merge`, one at a time, until `stop` completes. A merge that
    /// has begun is finished first.
    async fn run<F: Future<Output = MergeEnd>>(
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
            let item = request.item;
            let end = merge(request).await;
            self.record(item, end);
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
            return MergeEnd::Handled;
        };
        match daemon.merge_item(workforce, loaded, request).await {
            Ok(end) => end,
            Err(error) => {
                tracing::warn!(
                    "could not merge #{}: {}",
                    item.short(),
                    wf_core::error_chain(&error)
                );
                MergeEnd::Handled
            }
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
    ) -> Result<MergeEnd, SessionError> {
        let MergeRequest {
            item,
            commit,
            because,
            attempt,
        } = request;
        let state = self.snapshot(loaded).await?;
        if matches!(
            state.review_status(item),
            Some(wf_core::review::ReviewStatus::Merged { .. })
        ) {
            return Ok(MergeEnd::Handled);
        }
        let principal = self.principal_of_device(&state)?;
        let ItemView::Live(live) = state.item(item).ok_or(SessionError::UnknownItem(item))? else {
            return Err(SessionError::Archived(item));
        };
        let policy = &state.policy().expect("a workforce has a policy").policy;
        let target = policy.merge.target.clone();
        let message = format!("Merge #{}: {}", HashId::short(&item), live.title());
        let open = state.after_failed_merge(item, MergeFailure::Conflict);
        let comment = |body: String| {
            self.author(
                loaded,
                Actor::Daemon(principal),
                EventBody::Commented(Comment::plain(item, body)),
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
                Ok::<_, SessionError>(MergeEnd::Handled)
            }
        };
        let Some(repository) = self.repository_of(workforce) else {
            return cannot_merge(
                "the daemon does not know the repository; run any `wf` command in its main checkout"
                    .to_string(),
            )
            .await;
        };
        let signing = match self.merge_signing() {
            Ok(signing) => signing,
            Err(error) => return cannot_merge(wf_core::error_chain(&error)).await,
        };
        let mut outcome = MergeOutcome::TargetMoved;
        for _ in 0..MERGE_ATTEMPTS {
            let (repository, target, message, signing) = (
                repository.clone(),
                target.clone(),
                message.clone(),
                signing.clone(),
            );
            let merged = blocking(move || {
                GitCli::open(&repository)?.merge(&target, commit, &message, &signing)
            })
            .await
            .and_then(|merged| merged.map_err(SessionError::from));
            outcome = match merged {
                Ok(outcome) => outcome,
                Err(error) => {
                    cannot_merge(wf_core::error_chain(&error)).await?;
                    return Err(error);
                }
            };
            if outcome != MergeOutcome::TargetMoved {
                break;
            }
        }
        let end = match outcome {
            MergeOutcome::DirtyCheckout { .. } => MergeEnd::WaitsForCleanCheckout,
            MergeOutcome::Merged { .. }
            | MergeOutcome::Conflict { .. }
            | MergeOutcome::TargetMoved => MergeEnd::Handled,
        };
        match outcome {
            MergeOutcome::Merged { commit: merged } => {
                self.author(
                    loaded,
                    Actor::Daemon(principal),
                    EventBody::Merged(Merged {
                        item,
                        reviewed: commit,
                        commit: merged,
                    }),
                )
                .await?;
                let (branch, worktree) = (
                    item_branch(item),
                    item_worktree(&self.workforce_directory(workforce), item),
                );
                let cleaned = blocking(move || {
                    let git = GitCli::open(&repository)?;
                    git.remove_worktree(&worktree)?;
                    git.delete_branch(&branch)
                })
                .await?;
                if let Err(error) = cleaned {
                    tracing::warn!(
                        "could not remove the branch of #{}: {}",
                        item.short(),
                        wf_core::error_chain(&error)
                    );
                }
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
                comment(format!(
                    "Merging into {target} conflicts in {}. The next session merges {target} into the branch first.",
                    paths.join(", ")
                ))
                .await?;
                if let Some(open) = open {
                    self.move_item(loaded, principal, item, open, None, attempt.by_session())
                        .await?;
                }
            }
            MergeOutcome::DirtyCheckout(_) | MergeOutcome::TargetMoved
                if attempt == Attempt::Retry => {}
            MergeOutcome::DirtyCheckout(dirty) => {
                comment(format!(
                    "Waiting to merge: {target} is checked out in {} with changes to {}. Commit or stash them.",
                    dirty.worktree.display(),
                    dirty.files()
                ))
                .await?;
            }
            MergeOutcome::TargetMoved => {
                comment(format!("Waiting to merge: {target} kept moving.")).await?;
            }
        }
        Ok(end)
    }
}

impl Daemon {
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
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        let path = self.config.data_dir.join("keys").join("device-ssh");
        let text = wf_vcs::signing::device_private_key(&self.device)?;
        if std::fs::read_to_string(&path).ok().as_deref() == Some(text.as_str()) {
            return Ok(path);
        }
        let io = |source| SessionError::Io {
            path: path.clone(),
            source,
        };
        if let Some(directory) = path.parent() {
            std::fs::create_dir_all(directory).map_err(io)?;
        }
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .and_then(|mut file| file.write_all(text.as_bytes()))
            .map_err(io)?;
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

    use super::{Attempt, MergeEnd, MergeQueue, MergeRequest};

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
                            MergeEnd::Handled
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
                    MergeEnd::Handled
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
    fn a_merge_that_waits_on_a_clean_checkout_is_told_of_until_it_is_handled() {
        let queue = MergeQueue::default();
        let item = ItemId::from_bytes([1; 32]);
        queue.record(item, MergeEnd::WaitsForCleanCheckout);
        assert!(queue.waiting().contains(&item));
        queue.record(item, MergeEnd::Handled);
        assert!(queue.waiting().is_empty());
    }
}

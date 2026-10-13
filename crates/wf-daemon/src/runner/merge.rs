//! Merging reviewed work into the target branch.

use wf_core::event::{Comment, EventBody, Merged};
use wf_core::id::{CommentId, HashId, ItemId, SessionId, WorkforceId};
use wf_core::identity::Actor;
use wf_core::item::ItemView;
use wf_core::lifecycle::MergeFailure;
use wf_core::name::StateName;
use wf_core::session::CommitId;
use wf_vcs::{GitCli, MergeOutcome, VcsAdapter, item_branch, item_worktree};

use super::{SessionError, blocking};
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
    /// Where the item goes once merged.
    pub(super) done: StateName,
    /// The comment that says why, for that move.
    pub(super) because: Option<CommentId>,
    pub(super) attempt: Attempt,
}

impl Daemon {
    /// Merges reviewed work into the target branch and finishes the item. What cannot be merged
    /// now leaves the item where the principal or a later session will see it.
    pub(super) async fn merge_item(
        &self,
        workforce: WorkforceId,
        loaded: &Loaded,
        request: MergeRequest,
    ) -> Result<(), SessionError> {
        let MergeRequest {
            item,
            commit,
            done,
            because,
            attempt,
        } = request;
        let Some(_merging) = self.merging.take(item) else {
            return Ok(());
        };
        let state = self.snapshot(loaded).await?;
        if matches!(
            state.review_status(item),
            Some(wf_core::review::ReviewStatus::Merged { .. })
        ) {
            return Ok(());
        }
        let principal = self.principal_of_device(&state)?;
        let ItemView::Live(live) = state.item(item).ok_or(SessionError::UnknownItem(item))? else {
            return Err(SessionError::Archived(item));
        };
        let policy = &state.policy().expect("a workforce has a policy").policy;
        let target = policy.merge.target.clone();
        let message = format!("Merge #{}: {}", HashId::short(&item), live.title());
        let open = state.after_failed_merge(item, MergeFailure::Conflict);
        let review = state.after_failed_merge(item, MergeFailure::Transient);
        let comment = |body: String| {
            self.author(
                loaded,
                Actor::Daemon(principal),
                EventBody::Commented(Comment::plain(item, body)),
            )
        };

        // A merge that cannot be made leaves the item in its review state, where the dispatcher
        // tries again, never in its active state without a session.
        let cannot_merge = |why: String| {
            let note = format!("Could not merge into {target}: {why}. The daemon tries again.");
            let (comment, review) = (&comment, review.clone());
            async move {
                if attempt != Attempt::Retry {
                    comment(note).await?;
                    if let Some(review) = review {
                        self.move_item(loaded, principal, item, review, None, attempt.by_session())
                            .await?;
                    }
                }
                Ok::<_, SessionError>(())
            }
        };
        let Some(repository) = self.repository_of(workforce) else {
            return cannot_merge(
                "the daemon does not know the repository; run any `wf` command in its main checkout"
                    .to_string(),
            )
            .await;
        };
        let mut outcome = MergeOutcome::TargetMoved;
        for _ in 0..MERGE_ATTEMPTS {
            let (repository, target, message) =
                (repository.clone(), target.clone(), message.clone());
            let merged =
                blocking(move || GitCli::open(&repository)?.merge(&target, commit, &message))
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
                self.move_item(loaded, principal, item, done, because, attempt.by_session())
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
            MergeOutcome::DirtyCheckout { .. } | MergeOutcome::TargetMoved
                if attempt == Attempt::Retry => {}
            MergeOutcome::DirtyCheckout { worktree } => {
                comment(format!(
                    "Waiting to merge: {target} is checked out in {} with changes.",
                    worktree.display()
                ))
                .await?;
                if let Some(review) = review.clone() {
                    self.move_item(loaded, principal, item, review, None, attempt.by_session())
                        .await?;
                }
            }
            MergeOutcome::TargetMoved => {
                comment(format!("Waiting to merge: {target} kept moving.")).await?;
                if let Some(review) = review {
                    self.move_item(loaded, principal, item, review, None, attempt.by_session())
                        .await?;
                }
            }
        }
        Ok(())
    }
}

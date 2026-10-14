//! Merging reviewed work into the target branch.

use std::path::PathBuf;

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

impl Daemon {
    /// Merges reviewed work into the target branch and finishes the item. What cannot be merged
    /// now leaves the item where the principal or a later session will see it.
    pub(super) async fn merge_item(
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
        let Some(_merging) = self.merging.take(item) else {
            return Ok(MergeEnd::Handled);
        };
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
        let review = state.after_failed_merge(item, MergeFailure::Transient);
        let comment = |body: String| {
            self.author(
                loaded,
                Actor::Daemon(principal),
                EventBody::Commented(Comment::plain(item, body)),
            )
        };

        // A merge that cannot be made leaves the item in `review`, where the dispatcher tries
        // again, never `active` without a session.
        let cannot_merge = |why: String| {
            let note = format!("Could not merge into {target}: {why}. The daemon tries again.");
            let (comment, review) = (&comment, review);
            async move {
                if attempt != Attempt::Retry {
                    comment(note).await?;
                    if let Some(review) = review {
                        self.move_item(loaded, principal, item, review, None, attempt.by_session())
                            .await?;
                    }
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
                if let Some(review) = review {
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

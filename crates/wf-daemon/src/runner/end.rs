//! Ending sessions: what is written when one ends, and what follows for its item.

use std::sync::Arc;

use wf_core::event::{Comment, EventBody, ItemMoved, SessionEnded};
use wf_core::id::{CommentId, HashId, ItemId, SessionId, WorkforceId};
use wf_core::identity::Actor;
use wf_core::item::ItemState;
use wf_core::lifecycle::{AfterEnd, Closing, Finish, SessionEnd};
use wf_core::session::{CommitId, EndReason};
use wf_core::state::State;
use wf_vcs::{GitCli, VcsAdapter, VcsError, item_branch, item_worktree};

use super::merge::{Attempt, MergeRequest, Reservation};
use super::{SessionError, Stop, blocking};
use crate::{Daemon, Loaded};

impl Daemon {
    pub(super) async fn move_item(
        &self,
        loaded: &Loaded,
        principal: wf_core::identity::PrincipalKey,
        item: ItemId,
        to: ItemState,
        because: Option<CommentId>,
        by_session: Option<SessionId>,
    ) -> Result<(), SessionError> {
        self.author(
            loaded,
            Actor::Daemon(principal),
            EventBody::ItemMoved(ItemMoved {
                item,
                to: to.into(),
                by_session,
                because,
            }),
        )
        .await?;
        Ok(())
    }

    /// Ends a live session of this machine and applies what follows to its item. Ending one that
    /// has ended already only makes sure its task is stopped.
    pub(crate) async fn finish_session(
        self: &Arc<Self>,
        workforce: WorkforceId,
        loaded: &Arc<Loaded>,
        session: SessionId,
        end: SessionEnd,
    ) -> Result<(), SessionError> {
        let _ending = self.ending.lock().await;
        let state = self.snapshot(loaded).await?;
        let folded = state
            .session(session)
            .ok_or(SessionError::UnknownSession(session))?;
        if folded.device() != self.device() {
            return Err(SessionError::NotOurs(session));
        }
        if !folded.is_live() {
            self.runners.stop(session, Stop::Now);
            return Ok(());
        }
        let principal = self.principal_of_device(&state)?;
        let item = folded.item();
        let role = folded.role();
        let submitted = match &end {
            SessionEnd::Finish(Finish::Submit { .. }) => {
                self.submitted_commit(workforce, &state, item).await?
            }
            SessionEnd::Finish(
                Finish::Release { .. } | Finish::Reviewed | Finish::Supervised | Finish::Diagnosed,
            )
            | SessionEnd::Interruption(_) => None,
        };
        let Closing {
            reason,
            actor,
            note,
        } = state
            .closing(session, &end, submitted)
            .ok_or(SessionError::NotBound)?;
        // A submission with open change requests is refused before anything is written.
        state
            .check_policy(&EventBody::SessionEnded(SessionEnded { session, reason }))
            .map_err(|violation| SessionError::Authoring(violation.into()))?;
        // The comment comes first: once the session has ended it can no longer write.
        let because = match note {
            Some(note) => {
                let written = self
                    .author(
                        loaded,
                        actor.clone(),
                        EventBody::Commented(Comment::plain(item, note)),
                    )
                    .await?;
                Some(CommentId::from(written.id()))
            }
            None => None,
        };
        // The item moves while the session still holds it, so that it is never ready in between.
        let effect = match state.after_end(session, reason) {
            None | Some(AfterEnd::Nothing) => Ok(None),
            Some(AfterEnd::MoveTo(to)) => self
                .move_item(loaded, principal, item, to, because, Some(session))
                .await
                .map(|()| None),
            Some(AfterEnd::MergeThenDone { wait_in }) => {
                let EndReason::Submitted {
                    commit: Some(commit),
                } = reason
                else {
                    unreachable!("only submitted work with commits is merged");
                };
                // Reserved before the move, which wakes the dispatcher: none of its retries may
                // merge the item before the session's first attempt.
                let first_attempt = loaded.merges.reserve(MergeRequest {
                    item,
                    commit,
                    because,
                    attempt: Attempt::First { session },
                });
                match wait_in {
                    Some(to) => self
                        .move_item(loaded, principal, item, to, because, Some(session))
                        .await
                        .map(|()| Some(first_attempt)),
                    None => Ok(Some(first_attempt)),
                }
            }
        };
        self.author(
            loaded,
            actor,
            EventBody::SessionEnded(SessionEnded { session, reason }),
        )
        .await?;
        // The merge task merges it after the merges before it, now that the submission is
        // written; one that waits on the principal is told of by the dispatcher, which queues it
        // again.
        let effect = effect.map(|first_attempt| first_attempt.map(Reservation::queue));
        tracing::info!("session {}: ended, {reason:?}", session.short());
        let how = match end {
            SessionEnd::Finish(_) => Stop::AfterTurn,
            SessionEnd::Interruption(_) => Stop::Now,
        };
        // The task of a running session removes its directory once the harness has closed, so that
        // nothing the agent still runs writes into it.
        let running = self.runners.is_running(session);
        self.runners.stop(session, how);
        if !running {
            self.discard_session_directory(workforce, item, role).await;
        }
        if matches!(reason, EndReason::Submitted { commit: None }) {
            self.discard_branch(workforce, item).await;
        }
        effect?;
        Ok(())
    }

    /// Removes the worktree and the branch of an item whose work left nothing to keep.
    async fn discard_branch(&self, workforce: WorkforceId, item: ItemId) {
        let Some(repository) = self.repository_of(workforce) else {
            return;
        };
        let branch = item_branch(item);
        let worktree = item_worktree(&self.workforce_directory(workforce), item);
        let removed = blocking(move || {
            let git = GitCli::open(&repository)?;
            git.remove_worktree(&worktree)?;
            git.delete_branch(&branch)
        })
        .await;
        match removed {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::warn!(
                    "could not remove the branch of #{}: {}",
                    item.short(),
                    wf_core::error_chain(&error)
                );
            }
            Err(error) => {
                tracing::warn!(
                    "could not remove the branch of #{}: {}",
                    item.short(),
                    wf_core::error_chain(&error)
                );
            }
        }
    }

    /// The head of the branch of `item`, if it has commits of its own.
    async fn submitted_commit(
        &self,
        workforce: WorkforceId,
        state: &State,
        item: ItemId,
    ) -> Result<Option<CommitId>, SessionError> {
        let Some(repository) = self.repository_of(workforce) else {
            return Ok(None);
        };
        let target = state
            .policy()
            .expect("a workforce has a policy")
            .policy
            .merge
            .target
            .clone();
        let branch = item_branch(item);
        let commit = blocking(move || {
            let git = GitCli::open(&repository)?;
            let head = git.branch_head(&branch)?;
            Ok::<_, VcsError>((git.commits_ahead(&target, head)? > 0).then_some(head))
        })
        .await??;
        Ok(commit)
    }
}

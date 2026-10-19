//! Ending sessions: what is written when one ends, and what follows for its item.

use std::sync::Arc;

use wf_core::event::{Commented, EventBody, ItemMoved, SessionEnded};
use wf_core::hlc::Hlc;
use wf_core::id::{CommentId, EventId, HashId, ItemId, SessionId, WorkforceId};
use wf_core::identity::Actor;
use wf_core::item::{ItemEntry, ItemState};
use wf_core::lifecycle::{AfterEnd, Closing, Finish, SessionEnd};
use wf_core::session::{CommitId, EndReason};
use wf_core::state::State;
use wf_vcs::{GitCli, Rebase, VcsError, item_branch, item_worktree};

use super::merge::{Attempt, MergeRequest, Reservation};
use super::start::git_identity;
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

    /// Moves `item` to `to` for the session that is ending, unless it is there already.
    #[allow(clippy::too_many_arguments)]
    async fn move_item_once(
        &self,
        loaded: &Loaded,
        state: &State,
        principal: wf_core::identity::PrincipalKey,
        item: ItemId,
        to: ItemState,
        because: Option<CommentId>,
        session: SessionId,
    ) -> Result<(), SessionError> {
        if state.live_state(item) == Some(to) {
            return Ok(());
        }
        self.move_item(loaded, principal, item, to, because, Some(session))
            .await
    }

    /// The comment `actor` wrote on `item` with this `note` since `since`, when the session
    /// started, if an earlier try of the end did. A daemon comment carries no session, so only
    /// the time keeps one of an earlier session with the same text from counting.
    async fn written_note(
        &self,
        loaded: &Loaded,
        state: &State,
        item: ItemId,
        since: Hlc,
        actor: &Actor,
        note: &str,
    ) -> Result<Option<CommentId>, SessionError> {
        let Some(ItemEntry::Live(live)) = state.item(item) else {
            return Ok(None);
        };
        let candidates: Vec<CommentId> = live
            .comments()
            .iter()
            .filter(|(_, comment)| &comment.by == actor && comment.at > since)
            .map(|(id, _)| *id)
            .collect();
        let note = note.to_string();
        let found = loaded
            .storage
            .with(move |replica| {
                candidates.into_iter().find(|id| {
                    let event = wf_store::EventStore::event(replica.store(), &EventId::from(*id));
                    matches!(
                        event,
                        Ok(Some(event))
                            if matches!(&event.content.body, EventBody::Commented(written) if written.text == note)
                    )
                })
            })
            .await?;
        Ok(found)
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
        let _ending = self.ending.lock(session).await;
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
                if state.rebases_at_submit(session) {
                    let agent = folded.agent();
                    let committer = git_identity(&state, agent.principal, &agent.name)?;
                    self.rebase_at_submit(workforce, &state, item, committer)
                        .await?;
                }
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
            .check_policy(&EventBody::SessionEnded(SessionEnded {
                session,
                reason,
                because: None,
            }))
            .map_err(|violation| SessionError::Authoring(violation.into()))?;
        // The comment comes first: once the session has ended it can no longer write. A retry
        // after a failed write finds what an earlier try wrote and goes on from there.
        let because = match note {
            Some(note) => match self
                .written_note(loaded, &state, item, folded.started().hlc, &actor, &note)
                .await?
            {
                Some(written) => Some(written),
                None => {
                    let written = self
                        .author(
                            loaded,
                            actor.clone(),
                            EventBody::Commented(Commented::plain(item, note)),
                        )
                        .await?;
                    Some(CommentId::from(written.id()))
                }
            },
            None => None,
        };
        // The item moves while the session still holds it, so that it is never ready in between.
        // A move that could not be written is tried again with the whole end, which is not
        // written before it; one that is refused does not hold up the end.
        let effect = match state.after_end(session, reason) {
            None | Some(AfterEnd::Nothing) => Ok(None),
            Some(AfterEnd::MoveTo(to)) => self
                .move_item_once(loaded, &state, principal, item, to, because, session)
                .await
                .map(|()| None),
            Some(AfterEnd::MergeThenDone { commit, wait_in }) => {
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
                        .move_item_once(loaded, &state, principal, item, to, because, session)
                        .await
                        .map(|()| Some(first_attempt)),
                    None => Ok(Some(first_attempt)),
                }
            }
        };
        if let Err(error) = &effect
            && error.is_failed_write()
        {
            return effect.map(|_| ());
        }
        self.author(
            loaded,
            actor,
            EventBody::SessionEnded(SessionEnded {
                session,
                reason,
                because,
            }),
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
            self.discard_session_directory(workforce, loaded, item, session, role)
                .await;
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

    /// Rebases the branch of `item` onto the target as its work is submitted, the commits signed
    /// as the agent's own are. A conflict, or uncommitted changes the rebase would have to carry,
    /// refuse the submission before anything is written: the session goes on, and the rebase
    /// stopped at the conflict waits in the worktree for the next submission to go on with it.
    async fn rebase_at_submit(
        &self,
        workforce: WorkforceId,
        state: &State,
        item: ItemId,
        committer: String,
    ) -> Result<(), SessionError> {
        let repository = self
            .repository_of(workforce)
            .ok_or(SessionError::NoRepository)?;
        let target = target_of(state)?;
        let worktree = item_worktree(&self.workforce_directory(workforce), item);
        let signing = self.machine_config()?.commits.agent_signing.signing();
        let onto = target.clone();
        let rebased = blocking(move || {
            GitCli::open(&repository)?.rebase(&worktree, &onto, &signing, &committer)
        })
        .await??;
        match rebased {
            Rebase::UpToDate => Ok(()),
            Rebase::Rebased => {
                tracing::info!("rebased the branch of #{} onto {target}", item.short());
                Ok(())
            }
            Rebase::Conflict { paths } => Err(SessionError::RebaseConflict { target, paths }),
            Rebase::DirtyCheckout(dirty) => Err(SessionError::RebaseDirty { target, dirty }),
        }
    }

    /// The head of the branch of `item`, if it has commits of its own.
    async fn submitted_commit(
        &self,
        workforce: WorkforceId,
        state: &State,
        item: ItemId,
    ) -> Result<Option<CommitId>, SessionError> {
        let repository = self
            .repository_of(workforce)
            .ok_or(SessionError::NoRepository)?;
        let target = target_of(state)?;
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

/// The branch that work merges into, as the policy says.
fn target_of(state: &State) -> Result<String, SessionError> {
    let config = state.policy().ok_or(SessionError::NoPolicy)?;
    Ok(config.policy.merge.target.clone())
}

//! Ending sessions: what is written when one ends, and what follows for its item.

use std::sync::Arc;

use wf_core::claims::Presence;
use wf_core::event::{Commented, EventBody, ItemMoved, SessionEnded};
use wf_core::hlc::Hlc;
use wf_core::id::{CommentId, EventId, HashId, ItemId, SessionId, WorkforceId};
use wf_core::identity::Actor;
use wf_core::item::{ItemEntry, ItemState};
use wf_core::lifecycle::{AfterEnd, Closing, Finish, Interruption, SessionEnd};
use wf_core::session::{CommitId, SessionRole};
use wf_core::state::State;
use wf_vcs::{GitCli, Import, ImportMode, VcsError, item_branch, item_repository};

use super::merge::{Attempt, MergeRequest, Reservation};
use super::submit::Submission;
use super::{SessionError, Stop, blocking};
use crate::proto::EndAnswer;
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
                to,
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
    ///
    /// A submission whose checks have not passed yet is not ended: its checks start, and the
    /// answer says so.
    pub(crate) async fn finish_session(
        self: &Arc<Self>,
        workforce: WorkforceId,
        loaded: &Arc<Loaded>,
        session: SessionId,
        end: SessionEnd,
    ) -> Result<EndAnswer, SessionError> {
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
            return Ok(EndAnswer::Ended);
        }
        let principal = self.principal_of_device(&state)?;
        let item = folded.item();
        let role = folded.role();
        let head = match &end {
            SessionEnd::Finish(Finish::Submit { .. }) => match self.submissions.passed(session) {
                // Checks that passed end the session with the commit they ran on: what the agent
                // committed since is not copied, and neither runs them again nor goes unrebased.
                Some(checked) => Some(checked),
                // The agent rebased the branch onto the target before it submitted (`wf work
                // submit`), unless it is approved already; either way what it committed is
                // copied into this repository now.
                None => {
                    let mode = if state.rebases_at_submit(session) {
                        ImportMode::Submission
                    } else {
                        ImportMode::InProgress
                    };
                    self.import_commits(workforce, loaded, &state, item, mode)
                        .await?
                }
            },
            SessionEnd::Finish(
                Finish::Release { .. } | Finish::Reviewed | Finish::Supervised | Finish::Diagnosed,
            )
            | SessionEnd::Interruption(_) => {
                // What the work has got to stays on the branch of the item, for the next session
                // and for the principal to look at; the end names it.
                match role {
                    SessionRole::Work => {
                        match self
                            .import_commits(workforce, loaded, &state, item, ImportMode::InProgress)
                            .await
                        {
                            Ok(head) => head,
                            Err(error) => {
                                tracing::warn!(
                                    "could not copy the commits of session {}: {}",
                                    session.short(),
                                    wf_core::error_chain(&error)
                                );
                                None
                            }
                        }
                    }
                    SessionRole::Review { .. } | SessionRole::Supervise | SessionRole::Diagnose => {
                        None
                    }
                }
            }
        };
        let Closing {
            reason,
            actor,
            note,
            head,
        } = state
            .closing(session, &end, head)
            .ok_or(SessionError::UnknownSession(session))?;
        // A submission with open change requests is refused before anything is written.
        state
            .check_policy(&EventBody::SessionEnded(SessionEnded {
                session,
                reason,
                because: None,
                head,
            }))
            .map_err(|violation| SessionError::Authoring(violation.into()))?;
        // The checks of the policy run on the submission before anyone reviews it.
        if let (SessionEnd::Finish(finish @ Finish::Submit { .. }), Some(commit)) = (&end, head)
            && !self.submission_checked(session, commit)
        {
            let checks = Self::checks_of(&state)?;
            if !checks.is_empty() {
                let submission = Submission {
                    workforce,
                    session,
                    item,
                    commit,
                    finish: finish.clone(),
                };
                return self.begin_submission_checks(loaded, submission, checks);
            }
        }
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
        let effect = match state.after_end(session, reason, head) {
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
            return effect.map(|_| EndAnswer::Ended);
        }
        self.author(
            loaded,
            actor,
            EventBody::SessionEnded(SessionEnded {
                session,
                reason,
                because,
                head,
            }),
        )
        .await?;
        self.submissions.clear(session);
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
            self.discard_session_directory(workforce, item, session, role)
                .await;
        }
        effect?;
        Ok(EndAnswer::Ended)
    }

    /// Ends `session`, which runs on another device of this principal that has been gone for
    /// longer than the lease, `LeaseExpired`, so that a session started here takes over what it
    /// claimed. The comment before the end says which device was gone and since when.
    pub(super) async fn take_over(
        &self,
        loaded: &Loaded,
        session: SessionId,
        presence: &Presence,
    ) -> Result<(), SessionError> {
        let _ending = self.ending.lock(session).await;
        let state = self.snapshot(loaded).await?;
        let folded = state
            .session(session)
            .ok_or(SessionError::UnknownSession(session))?;
        if !folded.is_live() {
            return Ok(());
        }
        let Closing {
            reason,
            actor,
            note: _,
            head,
        } = state
            .closing(
                session,
                &SessionEnd::Interruption(Interruption::LeaseExpired),
                None,
            )
            .ok_or(SessionError::UnknownSession(session))?;
        let device = folded.device();
        let silent_since = i64::try_from(presence.silent_since_ms(device))
            .ok()
            .and_then(|ms| jiff::Timestamp::from_millisecond(ms).ok())
            .map_or_else(
                || "a while".to_string(),
                |at| at.strftime("%Y-%m-%d %H:%M:%S UTC").to_string(),
            );
        let note = format!(
            "Taken over: device {} was not heard from since {silent_since}, longer than the lease \
             of {}.",
            crate::peers::peer_label(&state, device),
            humantime::format_duration(state.lease()),
        );
        let written = self
            .author(
                loaded,
                actor.clone(),
                EventBody::Commented(Commented::plain(folded.item(), note)),
            )
            .await?;
        let because = Some(CommentId::from(written.id()));
        // The item goes back as after any interruption, while the session still holds it: the
        // start that takes over moves it on at once, and one that fails leaves it ready again.
        if let Some(AfterEnd::MoveTo(to)) = state.after_end(session, reason, head) {
            let principal = self.principal_of_device(&state)?;
            self.move_item_once(
                loaded,
                &state,
                principal,
                folded.item(),
                to,
                because,
                session,
            )
            .await?;
        }
        self.author(
            loaded,
            actor,
            EventBody::SessionEnded(SessionEnded {
                session,
                reason,
                because,
                head,
            }),
        )
        .await?;
        tracing::info!(
            "session {}: taken over from device {}, gone since {silent_since}",
            session.short(),
            device.fmt_short()
        );
        Ok(())
    }

    /// `body`, written for the work session `session`, with the head it names. Only a comment on
    /// its item names one, and only when the branch moved since the log named one for the session
    /// last: the commits are copied into this repository first, so that they exist here when the
    /// event is written. The agent never names a head itself, so one it brings is dropped. A
    /// comment is not held up by commits that cannot be copied; the end of the session names them.
    pub(crate) async fn with_head(
        &self,
        workforce: WorkforceId,
        loaded: &Loaded,
        session: SessionId,
        body: EventBody,
    ) -> EventBody {
        let EventBody::Commented(comment) = body else {
            return body;
        };
        let comment = Commented {
            head: None,
            ..comment
        };
        let head = self
            .moved_head(workforce, loaded, session, comment.item)
            .await;
        EventBody::Commented(Commented { head, ..comment })
    }

    async fn moved_head(
        &self,
        workforce: WorkforceId,
        loaded: &Loaded,
        session: SessionId,
        item: ItemId,
    ) -> Option<CommitId> {
        let state = self.snapshot(loaded).await.ok()?;
        let folded = state.session(session)?;
        if folded.role() != SessionRole::Work || folded.item() != item || !folded.is_live() {
            return None;
        }
        let head = match self
            .import_commits(workforce, loaded, &state, item, ImportMode::InProgress)
            .await
        {
            Ok(head) => head?,
            Err(error) => {
                tracing::warn!(
                    "could not copy the commits of session {}: {}",
                    session.short(),
                    wf_core::error_chain(&error)
                );
                return None;
            }
        };
        (folded.head() != Some(head)).then_some(head)
    }

    /// The commit the target branch is at in the principal's repository.
    pub(crate) async fn target_commit(
        &self,
        workforce: WorkforceId,
        loaded: &Loaded,
    ) -> Result<CommitId, SessionError> {
        let state = self.snapshot(loaded).await?;
        let target = target_of(&state)?;
        let repository = self.bare_repository(workforce);
        Ok(blocking(move || GitCli::open(&repository)?.branch_head(&target)).await??)
    }

    /// Copies the commits of the agent's repository for `item` into the principal's, as `mode`
    /// says, and returns the head of the branch of `item` there if it has commits of its own. A
    /// submission that lacks the tip of the target is refused before anything is written: the
    /// session goes on and rebases again.
    pub(super) async fn import_commits(
        &self,
        workforce: WorkforceId,
        loaded: &Loaded,
        state: &State,
        item: ItemId,
        mode: ImportMode,
    ) -> Result<Option<CommitId>, SessionError> {
        let repository = self.bare_repository(workforce);
        let target = target_of(state)?;
        let agent_repository = item_repository(&self.workforce_directory(workforce), item);
        let branch = item_branch(item);
        let onto = target.clone();
        let _writing = loaded.item_refs.lock().await;
        let imported = blocking(move || {
            let git = GitCli::open(&repository)?;
            let import = git.import_commits(&agent_repository, item, &onto, mode)?;
            let head = match import {
                Import::Imported { commit } => commit,
                Import::BehindTarget => return Ok(Err(())),
                // Nothing in the agent's repository: what an earlier session imported stays.
                Import::NoCommits => match git.branch_head(&branch) {
                    Ok(head) => head,
                    Err(VcsError::NoBranch(_)) => return Ok(Ok(None)),
                    Err(error) => return Err(error),
                },
            };
            Ok::<_, VcsError>(Ok((git.commits_ahead(&onto, head)? > 0).then_some(head)))
        })
        .await??;
        loaded.commits.copied_here();
        imported.map_err(|()| SessionError::BehindTarget { target })
    }
}

/// The branch that work merges into, as the policy says.
fn target_of(state: &State) -> Result<String, SessionError> {
    let config = state.policy().ok_or(SessionError::NoPolicy)?;
    Ok(config.policy.merge.target.clone())
}

//! Claims across devices, as the log has them after a sync: a session of this device whose claim
//! an earlier one holds ends `Outclaimed`, and one that another device ended while this one was
//! gone is stopped.

use std::sync::Arc;

use wf_core::event::{Commented, EventBody};
use wf_core::id::{HashId, SessionId, WorkforceId};
use wf_core::lifecycle::{Interruption, SessionEnd};
use wf_core::session::SessionRole;
use wf_vcs::ImportMode;

use super::{SessionError, Stop};

use crate::{Daemon, Loaded};

impl Daemon {
    /// Ends the sessions of this device that lost their claim to an earlier one, at once and
    /// keeping their commits: the end names the head of a work session's branch.
    pub(super) async fn end_lost_claims(
        self: &Arc<Self>,
        workforce: WorkforceId,
        loaded: &Arc<Loaded>,
    ) {
        let device = self.device();
        let Ok(lost) = self
            .read(loaded, move |state| state.outclaimed(device))
            .await
        else {
            return;
        };
        for session in lost {
            let ended = self
                .finish_session(
                    workforce,
                    loaded,
                    session,
                    SessionEnd::Interruption(Interruption::Outclaimed),
                )
                .await;
            match ended {
                Ok(_) => tracing::info!(
                    "session {}: an earlier claim holds its item",
                    session.short()
                ),
                Err(error) => tracing::warn!(
                    "could not end session {}, which lost its claim: {}",
                    session.short(),
                    wf_core::error_chain(&error)
                ),
            }
        }
    }

    /// Stops the harnesses of the sessions that run here but ended on another device, which took
    /// them over while this one was gone. A work session's commits are copied first and its last
    /// head is named in a comment, so they reach every device and the next brief names them.
    pub(super) async fn stop_sessions_ended_elsewhere(
        &self,
        workforce: WorkforceId,
        loaded: &Loaded,
    ) {
        let running = self.runners.running();
        let Ok(ended) = self
            .read(loaded, move |state| {
                running
                    .into_iter()
                    .filter(|session| {
                        state
                            .session(*session)
                            .is_some_and(|folded| !folded.is_live())
                    })
                    .collect::<Vec<SessionId>>()
            })
            .await
        else {
            return;
        };
        for session in ended {
            if let Err(error) = self.stop_ended_elsewhere(workforce, loaded, session).await {
                tracing::warn!(
                    "could not name the last head of session {}, which was taken over: {}",
                    session.short(),
                    wf_core::error_chain(&error)
                );
            }
            self.runners.stop(session, Stop::Now);
        }
    }

    async fn stop_ended_elsewhere(
        &self,
        workforce: WorkforceId,
        loaded: &Loaded,
        session: SessionId,
    ) -> Result<(), SessionError> {
        // An end of this device's own holds the lock until it has stopped the task itself.
        let _ending = self.ending.lock(session).await;
        if !self.runners.is_running(session) {
            return Ok(());
        }
        let state = self.snapshot(loaded).await?;
        let folded = state
            .session(session)
            .ok_or(SessionError::UnknownSession(session))?;
        if folded.role() != SessionRole::Work {
            return Ok(());
        }
        let item = folded.item();
        let head = self
            .import_commits(workforce, loaded, &state, item, ImportMode::InProgress)
            .await?;
        let Some(head) = head.filter(|head| folded.head() != Some(*head)) else {
            return Ok(());
        };
        let note = format!(
            "This session was taken over while its device was gone; its last commit is {}.",
            head.short()
        );
        self.author(
            loaded,
            folded.actor(),
            EventBody::Commented(Commented {
                head: Some(head),
                ..Commented::plain(item, note)
            }),
        )
        .await?;
        tracing::info!(
            "session {}: taken over elsewhere, its last head {} named",
            session.short(),
            head.short()
        );
        Ok(())
    }
}

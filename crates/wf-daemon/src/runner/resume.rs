//! Resuming the sessions an earlier daemon left live. The agent's own id of each session is kept
//! next to its transcript, and a daemon that starts reopens the conversation by it, so that a
//! restart costs the agent nothing. A session that cannot be resumed ends `Interrupted`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use wf_core::id::{CommentId, HashId, SessionId, WorkforceId};
use wf_core::lifecycle::{Interruption, SessionEnd, TurnOutcome};
use wf_core::state::State;
use wf_vcs::GitCli;

use super::drive::at_ms;
use super::{Begin, SessionError, blocking};
use crate::processes::{Mark, Owner, Place};
use crate::{Daemon, Loaded};

/// What the daemon keeps of the agent's side of a session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AgentSessionRecord {
    /// The id the agent gave the session.
    pub(crate) id: String,
    /// When the daemon last prompted the agent, in milliseconds since the epoch; `None` before
    /// the brief was sent.
    pub(crate) prompted_ms: Option<u64>,
    /// The current value of the agent's model option once the session opened; `None` when the
    /// agent has no model option or the record is older than this field.
    pub(crate) model: Option<String>,
    /// The comments written for the session that its prompts told so far, kept once the agent
    /// accepted the prompt; `None` in a record older than this field.
    pub(crate) told: Option<BTreeSet<CommentId>>,
}

/// Where the record of `session` is kept, beside its transcript.
pub(crate) fn record_path(transcripts: &Path, session: SessionId) -> PathBuf {
    transcripts.join(format!("{session}.agent-session.json"))
}

/// The model `session` runs with, as the record beside its transcript in `transcripts` keeps it;
/// `None` when it is not known on this machine.
pub fn session_model(transcripts: &Path, session: SessionId) -> Option<String> {
    AgentSessionRecord::read(&record_path(transcripts, session))?.model
}

/// Copies the record of `old` in `from` as the record of `new` in `to`, for `wf import --resign`:
/// the comments it told are named by `comment`, and those it does not name are left out. `false`
/// when there is no readable record of `old`.
pub fn copy_session_record(
    from: &Path,
    old: SessionId,
    to: &Path,
    new: SessionId,
    comment: impl Fn(CommentId) -> Option<CommentId>,
) -> std::io::Result<bool> {
    let Some(mut record) = AgentSessionRecord::read(&record_path(from, old)) else {
        return Ok(false);
    };
    record.told = record
        .told
        .map(|told| told.into_iter().filter_map(&comment).collect());
    record.write(&record_path(to, new))?;
    Ok(true)
}

impl AgentSessionRecord {
    pub(crate) fn read(path: &Path) -> Option<Self> {
        serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
    }

    /// Replaces the record through a rename, so that a daemon that stops halfway leaves the old
    /// one.
    pub(crate) fn write(&self, path: &Path) -> std::io::Result<()> {
        crate::data_dir::write_private(path, &serde_json::to_vec(self)?)
    }
}

#[derive(Debug, thiserror::Error)]
enum NotResumed {
    #[error("its item moved on")]
    MovedOn,
    #[error("the agent's id of the session was not kept")]
    NoRecord,
    #[error("{} is gone", .0.display())]
    NoDirectory(PathBuf),
    #[error(transparent)]
    Session(#[from] SessionError),
}

impl Daemon {
    /// Resumes the sessions of this machine that the previous daemon left live, and ends the
    /// ones it cannot.
    pub(crate) async fn resume_sessions(
        self: &Arc<Self>,
        workforce: WorkforceId,
        loaded: &Arc<Loaded>,
    ) {
        let Ok(state) = self.snapshot(loaded).await else {
            return;
        };
        let stale: Vec<SessionId> = state
            .live_sessions()
            .filter(|session| session.device() == self.device())
            .filter(|session| !self.runners.is_running(session.id()))
            .map(|session| session.id())
            .collect();
        for session in stale {
            let interruption = match self.resume_session(workforce, loaded, session).await {
                Ok(()) => continue,
                Err(NotResumed::MovedOn) => Interruption::MovedOn,
                Err(reason) => {
                    tracing::info!(
                        "session {}: not resumed, {}",
                        session.short(),
                        wf_core::error_chain(&reason)
                    );
                    Interruption::Interrupted
                }
            };
            if let Err(error) = self
                .finish_session(
                    workforce,
                    loaded,
                    session,
                    SessionEnd::Interruption(interruption),
                )
                .await
            {
                tracing::warn!(
                    "could not end stale session {session}: {}",
                    wf_core::error_chain(&error)
                );
            }
        }
    }

    /// Stops what the sessions of this device in `workforce` that ended left running: those
    /// that ended while no daemon ran, or before a daemon stopped what a session left at its end.
    /// A live session, resumed or not, keeps its processes, and so does one the workforce does not
    /// know.
    pub(crate) async fn stop_what_ended_sessions_left(
        &self,
        workforce: WorkforceId,
        loaded: &Loaded,
    ) {
        let Ok(state) = self.snapshot(loaded).await else {
            return;
        };
        let spared = self.live_places(&state, workforce);
        let ended: Vec<Owner> = state
            .sessions()
            .filter(|session| session.device() == self.device())
            .filter_map(|session| {
                let ended = session.ended()?.at().wall_ms();
                let directory = self.session_directory(
                    workforce,
                    session.item(),
                    session.id(),
                    &session.role(),
                );
                Some(Owner {
                    mark: Mark::Session {
                        workforce,
                        session: session.id(),
                    },
                    place: Some(Place::new(
                        &directory,
                        at_ms(session.started().hlc.wall_ms()),
                        Some(at_ms(ended)),
                    )),
                    spared: spared.clone(),
                })
            })
            .collect();
        for owner in crate::processes::with_processes(ended).await {
            let (Mark::Session { session, .. }, Some(place)) = (owner.mark, owner.place) else {
                continue;
            };
            crate::processes::stop_session(workforce, session, place, spared.clone()).await;
        }
        tracing::debug!("workforce {workforce}: stopped what ended sessions left running");
    }

    /// Where the sessions of this device in `workforce` that are live run, and since when.
    pub(crate) fn live_places(&self, state: &State, workforce: WorkforceId) -> Vec<Place> {
        state
            .live_sessions()
            .filter(|session| session.device() == self.device())
            .map(|session| {
                let directory = self.session_directory(
                    workforce,
                    session.item(),
                    session.id(),
                    &session.role(),
                );
                Place::new(&directory, at_ms(session.started().hlc.wall_ms()), None)
            })
            .collect()
    }

    async fn resume_session(
        self: &Arc<Self>,
        workforce: WorkforceId,
        loaded: &Arc<Loaded>,
        session: SessionId,
    ) -> Result<(), NotResumed> {
        let state = self.snapshot(loaded).await.map_err(SessionError::from)?;
        let live = state
            .session(session)
            .ok_or(SessionError::UnknownSession(session))?;
        let (agent, item, role) = (live.agent().name.clone(), live.item(), live.role());
        let setup = self.setup(&state, workforce, agent, item, &role)?;
        // The session itself asks what is wrong with the machine, before it is prompted.
        match state.turn_outcome(session, 0, setup.definition.autonomy.nudges, None) {
            TurnOutcome::Close => return Err(NotResumed::MovedOn),
            TurnOutcome::Continue
            | TurnOutcome::Park
            | TurnOutcome::GiveUp
            | TurnOutcome::MachineFault(_) => {}
        }
        let record = AgentSessionRecord::read(&record_path(&setup.transcripts, session))
            .ok_or(NotResumed::NoRecord)?;
        let directory = self.session_directory(workforce, item, session, &role);
        if !directory.is_dir() {
            return Err(NotResumed::NoDirectory(directory));
        }
        let repository = self.bare_repository(workforce);
        let git = blocking(move || GitCli::open(&repository))
            .await?
            .map_err(SessionError::from)?;
        let run = self.run(setup, session, directory, git, Begin::Resume(record));
        tracing::info!(
            "session {}: {} resumed on item {}{}",
            session.short(),
            run.agent,
            item.short(),
            run.isolation()
        );
        self.spawn_run(loaded, run);
        Ok(())
    }
}

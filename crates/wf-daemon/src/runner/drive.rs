//! The task of a running session: prompts the harness turn after turn, parks it while its
//! item waits, and ends it when nobody else does.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;
use tokio::sync::mpsc;
use tokio::time::Instant;
use wf_core::id::SessionId;
use wf_core::lifecycle::{Interruption, Resume, SessionEnd, TurnOutcome};
use wf_core::session::{CommitId, HarnessFault, MachineFault, SessionRole};
use wf_core::state::State;
use wf_harness::{AgentSessionId, Conversation, Launch, Opening};
use wf_vcs::StoppedRebase;

use super::brief::{brief, restarted, resumed};
use super::resume::AgentSessionRecord;
use super::start::sandboxed;
use super::{Begin, CANCEL_GRACE, End, GRACE, Run, SILENCE_LIMIT, START_TIMEOUT, Stop, blocking};
use crate::faults::Faults;
use crate::{Daemon, Loaded};

/// The task of one session: runs the harness, and ends the session if nobody else does.
pub(super) async fn run_session(
    daemon: Arc<Daemon>,
    loaded: Arc<Loaded>,
    run: Run,
    mut stopped: mpsc::UnboundedReceiver<Stop>,
) {
    let end = drive(&daemon, &loaded, &run, &mut stopped).await;
    // The session stays among the runners until its end is written, so that it is not resumed
    // while its end is tried again.
    let loaded = match end {
        End::Interrupt(interruption) => interrupt(&daemon, loaded, &run, interruption).await,
        End::Over => loaded,
    };
    daemon.runners.stop(run.session, Stop::Now);
    daemon.held.forget(run.session);
    daemon.callers.revoke(run.session);
    daemon.submissions.clear(run.session);
    // A session that is still live was only stopped, as when the daemon halts.
    let session = run.session;
    let ended = daemon
        .read(&loaded, move |state| !session_is_live(state, session))
        .await
        .unwrap_or(false);
    if ended {
        daemon
            .discard_session_directory(run.workforce, &loaded, run.item, run.session, run.role)
            .await;
        daemon.keep_caches_in_budget(run.workforce, &loaded).await;
    }
}

/// How long to wait before trying again to end a session whose end could not be written.
const RETRY_END: Duration = Duration::from_secs(30);

/// Ends the session with `interruption`. An end that cannot be written, as on a full disk, is
/// tried again until it is or the daemon halts, so the session does not stay live without a
/// harness; one refused for any other reason is not. Returns the workforce as it was loaded
/// last, since a store that failed a write may have been opened again.
async fn interrupt(
    daemon: &Arc<Daemon>,
    mut loaded: Arc<Loaded>,
    run: &Run,
    interruption: Interruption,
) -> Arc<Loaded> {
    let mut reported = false;
    loop {
        let ended = daemon
            .finish_session(
                run.workforce,
                &loaded,
                run.session,
                SessionEnd::Interruption(interruption.clone()),
            )
            .await;
        let Err(error) = ended else {
            return loaded;
        };
        if !error.is_failed_write() {
            tracing::warn!(
                "could not end session {}: {}",
                run.session,
                wf_core::error_chain(&error)
            );
            return loaded;
        }
        if !reported {
            tracing::warn!(
                "could not end session {}, trying again every {}: {}",
                run.session,
                humantime::format_duration(RETRY_END),
                wf_core::error_chain(&error)
            );
            reported = true;
        }
        tokio::select! {
            () = tokio::time::sleep(RETRY_END) => {}
            () = daemon.until_halted() => return loaded,
        }
        // A store that failed a write may have been closed and opened again.
        if let Ok(reopened) = daemon.workforce(run.workforce).await {
            loaded = reopened;
        }
    }
}

async fn drive(
    daemon: &Arc<Daemon>,
    loaded: &Arc<Loaded>,
    run: &Run,
    stopped: &mut mpsc::UnboundedReceiver<Stop>,
) -> End {
    let transcript = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&run.transcript)
    {
        Ok(transcript) => transcript,
        Err(error) => return transcript_failed(&daemon.faults, &run.transcript, &error),
    };
    let mut path =
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect::<Vec<_>>();
    if let Some(directory) = daemon.bin_dir() {
        path.insert(0, directory);
    }
    let path = std::env::join_paths(path).unwrap_or_default();
    // What the agent's `wf` presents to the daemon, so it acts as the session and nothing else.
    let token = daemon.callers.issue(run.workforce, run.session);
    let mut agent = run
        .command
        .clone()
        .env(crate::caller::SESSION_VARIABLE, run.session.to_string())
        .env(crate::caller::SESSION_TOKEN_VARIABLE, token.to_string())
        .env(crate::caller::WORKFORCE_VARIABLE, run.workforce.to_string())
        .env("PATH", path.to_string_lossy());
    for (name, value) in &run.git_environment {
        agent = agent.env(name, value);
    }
    let agent = match sandboxed(daemon, run, agent) {
        Ok(agent) => agent,
        Err(problem) => {
            tracing::warn!(
                "could not sandbox {} for session {}: {problem}",
                run.agent,
                run.session
            );
            return failed(format!("the sandbox could not be set up: {problem}"));
        }
    };
    let agent = crate::environment::confine(
        agent,
        &run.environment,
        crate::environment::daemon_variables(),
    );
    let opening = match &run.begin {
        Begin::New => Opening::New,
        Begin::Resume(record) => Opening::Resume(AgentSessionId(record.id.clone())),
    };
    let start = Conversation::start(Launch {
        agent,
        opening,
        model: run.model.clone(),
        directory: run.worktree.clone(),
        permissions: Arc::clone(&run.permissions),
        transcript,
        silence_limit: SILENCE_LIMIT,
    });
    // A harness that waits on a login or hangs at start can still be stopped; giving up the
    // start stops it.
    let started = tokio::select! {
        started = start => started,
        Some(_) = stopped.recv() => return End::Over,
        () = tokio::time::sleep(START_TIMEOUT) => {
            return harness_fault(daemon, run, HarnessFault::Unreachable, format!(
                "{} did not start within {}",
                run.harness,
                humantime::format_duration(START_TIMEOUT)
            ));
        }
    };
    let mut conversation = match started {
        Ok(conversation) => conversation,
        Err(error) if error.needs_login() => {
            return harness_fault(
                daemon,
                run,
                HarnessFault::LoginLapsed,
                wf_core::error_chain(&error),
            );
        }
        Err(error) if error.usage_limit(Timestamp::now()).is_some() => {
            return usage_limit(daemon, run, &error);
        }
        // The agent could not take the conversation up again: the session ends as any session an
        // earlier daemon left behind.
        Err(error) if matches!(run.begin, Begin::Resume(_)) => {
            tracing::info!(
                "session {}: {} could not resume it: {}",
                run.session,
                run.agent,
                wf_core::error_chain(&error)
            );
            return End::Interrupt(Interruption::Interrupted);
        }
        Err(wf_harness::Error::Transcript(error)) => {
            return transcript_failed(&daemon.faults, &run.transcript, &error);
        }
        // The agent's own config is at fault, not the machine.
        Err(error @ wf_harness::Error::UnknownModel { .. }) => {
            return failed(format!(
                "{} did not start: {}",
                run.agent,
                wf_core::error_chain(&error)
            ));
        }
        Err(
            error @ (wf_harness::Error::Protocol(_)
            | wf_harness::Error::Panicked
            | wf_harness::Error::ExitedBeforeOpen
            | wf_harness::Error::Killed
            | wf_harness::Error::Ended
            | wf_harness::Error::Silent(_)
            | wf_harness::Error::CannotResume),
        ) => {
            let report = wf_core::error_chain(&error);
            tracing::warn!(
                "could not start {} for session {}: {report}",
                run.agent,
                run.session
            );
            return harness_fault(
                daemon,
                run,
                HarnessFault::Unreachable,
                format!("{} did not start: {report}", run.harness),
            );
        }
    };
    let mut record = AgentSessionRecord {
        id: conversation.session().0.clone(),
        prompted_ms: match &run.begin {
            Begin::New => None,
            Begin::Resume(record) => record.prompted_ms,
        },
        // An agent that does not report its options again on a resume runs the model it ran.
        model: conversation
            .model()
            .map(str::to_string)
            .or_else(|| match &run.begin {
                Begin::New => None,
                Begin::Resume(record) => record.model.clone(),
            }),
    };
    // Without the record, a restart could not resume the session.
    if let Err(error) = record.write(&run.agent_session) {
        let _ = conversation.close().await;
        return not_kept(&daemon.faults, &run.agent_session, &error);
    }
    let end = turns(daemon, loaded, run, stopped, &mut conversation, &mut record).await;
    let _ = conversation.close().await;
    end
}

async fn turns(
    daemon: &Arc<Daemon>,
    loaded: &Arc<Loaded>,
    run: &Run,
    stopped: &mut mpsc::UnboundedReceiver<Stop>,
    conversation: &mut Conversation,
    record: &mut AgentSessionRecord,
) -> End {
    // A resumed session's time counts from the resume: how long the daemon was down is unknown.
    let mut deadline = run.max_duration.map(|duration| Instant::now() + duration);
    let mut prompt = match first_prompt(daemon, loaded, run, stopped, &deadline, record).await {
        Ok(prompt) => prompt,
        Err(end) => return end,
    };
    let cancel = conversation.cancel();
    let mut last_head = branch_head(run).await;
    let mut turns_without_commit = 0;
    loop {
        // Refs packed since the last turn (by the principal's git, outside the sandbox) cannot be
        // recreated from inside the session.
        if run.role == SessionRole::Work {
            make_branch_loose(run).await;
        }
        record.prompted_ms = Some(wf_store::now_ms());
        if let Err(error) = record.write(&run.agent_session) {
            return not_kept(&daemon.faults, &run.agent_session, &error);
        }
        let turn = conversation.reply(prompt);
        tokio::pin!(turn);
        let result = tokio::select! {
            result = &mut turn => result,
            Some(how) = stopped.recv() => {
                if matches!(how, Stop::AfterTurn)
                    && tokio::time::timeout(GRACE, &mut turn).await.is_ok()
                {
                    return End::Over;
                }
                cancel.cancel();
                let _ = tokio::time::timeout(CANCEL_GRACE, &mut turn).await;
                return End::Over;
            }
            () = sleep_until(deadline) => {
                cancel.cancel();
                let _ = tokio::time::timeout(CANCEL_GRACE, &mut turn).await;
                return End::Interrupt(Interruption::TimedOut);
            }
        };
        let (_stop_reason, last_message) = match result {
            Ok(reply) => reply,
            Err(error) if error.needs_login() => {
                return harness_fault(
                    daemon,
                    run,
                    HarnessFault::LoginLapsed,
                    wf_core::error_chain(&error),
                );
            }
            Err(error) if error.usage_limit(Timestamp::now()).is_some() => {
                return usage_limit(daemon, run, &error);
            }
            Err(wf_harness::Error::Transcript(error)) => {
                return transcript_failed(&daemon.faults, &run.transcript, &error);
            }
            // A harness that crashes, hangs or refuses a turn it was reached for may do so for
            // the work.
            Err(
                error @ (wf_harness::Error::Protocol(_)
                | wf_harness::Error::Panicked
                | wf_harness::Error::ExitedBeforeOpen
                | wf_harness::Error::Killed
                | wf_harness::Error::Ended
                | wf_harness::Error::Silent(_)
                | wf_harness::Error::CannotResume
                | wf_harness::Error::UnknownModel { .. }),
            ) => {
                return failed(format!(
                    "{} stopped answering: {}",
                    run.agent,
                    wf_core::error_chain(&error)
                ));
            }
        };
        daemon.faults.harness_answered(&run.harness);

        // The agent submitted and ended its turn while the checks run: nothing to say to it until
        // they are done. The daemon runs them, not the agent, so the wait does not count against
        // the session's time.
        let waiting_since = Instant::now();
        tokio::select! {
            () = daemon.submissions.until_settled(run.session) => {}
            Some(_) = stopped.recv() => return End::Over,
        }
        let waited = waiting_since.elapsed();
        deadline = deadline.map(|deadline| deadline + waited);
        if let Some(failures) = daemon.submissions.take_failure(run.session) {
            last_head = branch_head(run).await;
            turns_without_commit = 0;
            prompt = failures;
            continue;
        }

        let head = branch_head(run).await;
        if head != last_head {
            turns_without_commit = 0;
            last_head = head;
        } else {
            turns_without_commit += 1;
        }
        let machine_fault = daemon.faults.blocking_all();
        let (session, nudges) = (run.session, run.nudges);
        let Ok((outcome, live)) = daemon
            .read(loaded, move |state| {
                (
                    state.turn_outcome(session, turns_without_commit, nudges, machine_fault),
                    session_is_live(state, session),
                )
            })
            .await
        else {
            return failed("the workforce could not be read".to_string());
        };
        match outcome {
            TurnOutcome::Close => return closed(live),
            TurnOutcome::Continue => prompt = nudge(run).await,
            TurnOutcome::MachineFault(fault) => {
                return End::Interrupt(Interruption::MachineFault(fault));
            }
            TurnOutcome::GiveUp => {
                return End::Interrupt(Interruption::GaveUp {
                    turns: turns_without_commit,
                    last_message,
                });
            }
            TurnOutcome::Park => match park(daemon, loaded, run, stopped, &deadline).await {
                None => {
                    prompt = resumed(loaded, run).await;
                    turns_without_commit = 0;
                }
                Some(end) => return end,
            },
        }
    }
}

/// What the session is told first: its brief, or after a restart that it was resumed. A session
/// that was parked when the daemon stopped waits until its item is ready again.
async fn first_prompt(
    daemon: &Arc<Daemon>,
    loaded: &Arc<Loaded>,
    run: &Run,
    stopped: &mut mpsc::UnboundedReceiver<Stop>,
    deadline: &Option<Instant>,
    record: &AgentSessionRecord,
) -> Result<String, End> {
    let Some(prompted_ms) = record.prompted_ms else {
        return brief(loaded, run).await.map_err(failed);
    };
    let (session, nudges) = (run.session, run.nudges);
    let machine_fault = daemon.faults.blocking_all();
    let Ok((outcome, resume, live)) = daemon
        .read(loaded, move |state| {
            (
                state.turn_outcome(session, 0, nudges, machine_fault),
                state.resume_after_restart(session, prompted_ms),
                session_is_live(state, session),
            )
        })
        .await
    else {
        return Err(failed("the workforce could not be read".to_string()));
    };
    match outcome {
        TurnOutcome::MachineFault(fault) => Err(End::Interrupt(Interruption::MachineFault(fault))),
        TurnOutcome::Continue | TurnOutcome::GiveUp => match resume {
            Resume::AfterWait => Ok(resumed(loaded, run).await),
            Resume::AfterRestart => Ok(restarted(loaded, run, prompted_ms).await),
        },
        TurnOutcome::Close => Err(closed(live)),
        TurnOutcome::Park => match park(daemon, loaded, run, stopped, deadline).await {
            None => Ok(resumed(loaded, run).await),
            Some(end) => Err(end),
        },
    }
}

fn failed(because: String) -> End {
    End::Interrupt(Interruption::Failed { because })
}

/// The session's record at `path` cannot be written, so a restart would not resume it as it is.
fn not_kept(faults: &Faults, path: &Path, error: &std::io::Error) -> End {
    if faults.note_space("the record of a session", error) {
        return End::Interrupt(Interruption::MachineFault(MachineFault::NoSpace));
    }
    failed(format!(
        "the record of the session {} cannot be written: {error}",
        path.display()
    ))
}

/// The session's transcript at `path` cannot be written, so nobody could follow it.
fn transcript_failed(faults: &Faults, path: &Path, error: &std::io::Error) -> End {
    if faults.note_space("a transcript", error) {
        return End::Interrupt(Interruption::MachineFault(MachineFault::NoSpace));
    }
    failed(format!(
        "the transcript {} cannot be written: {error}",
        path.display()
    ))
}

/// The harness of the session cannot carry it, as `what` says: the session ends for `fault`, and
/// the agents of the harness wait until it is tried again.
fn harness_fault(daemon: &Daemon, run: &Run, fault: HarnessFault, what: String) -> End {
    daemon.faults.note_harness(&run.harness, fault, what, None);
    End::Interrupt(Interruption::MachineFault(fault.into()))
}

/// The harness of the session says its usage limit is reached, as `error` tells: the session
/// ends for it, and the agents of the harness wait until the reset, or until they are tried
/// again when `error` gives none.
fn usage_limit(daemon: &Daemon, run: &Run, error: &wf_harness::Error) -> End {
    let resets_at_ms = error
        .usage_limit(Timestamp::now())
        .and_then(|limit| limit.resets_at)
        .and_then(|at| u64::try_from(at.as_millisecond()).ok());
    daemon.faults.note_harness(
        &run.harness,
        HarnessFault::UsageLimit,
        wf_core::error_chain(error),
        resets_at_ms,
    );
    End::Interrupt(Interruption::MachineFault(MachineFault::UsageLimit))
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// What a session that goes on is told after a turn: how to end it, and for work whose
/// submission left a rebase stopped at a conflict, which files to resolve.
async fn nudge(run: &Run) -> String {
    let stopped = match run.role {
        SessionRole::Work => {
            let (git, worktree) = (run.git.clone(), run.worktree.clone());
            blocking(move || git.stopped_rebase(&worktree))
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or(StoppedRebase::NotStopped)
        }
        SessionRole::Review { .. } | SessionRole::Supervise | SessionRole::Diagnose => {
            StoppedRebase::NotStopped
        }
    };
    match stopped {
        StoppedRebase::Conflict { paths } => {
            wf_core::brief::rebase_nudge(run.item, &run.target, &paths)
        }
        StoppedRebase::Stopped | StoppedRebase::NotStopped => {
            wf_core::brief::nudge(run.item, run.role)
        }
    }
}

async fn make_branch_loose(run: &Run) {
    let (git, branch) = (run.git.clone(), run.branch.clone());
    match blocking(move || git.make_branch_loose(&branch)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!("session {}: {error}", run.session),
        Err(error) => tracing::warn!("session {}: {error}", run.session),
    }
}

async fn branch_head(run: &Run) -> Option<CommitId> {
    let (git, branch) = (run.git.clone(), run.branch.clone());
    blocking(move || git.branch_head(&branch)).await.ok()?.ok()
}

/// How the task ends when a turn outcome says to close: the session ended already, or its item
/// moved on under it.
fn closed(live: bool) -> End {
    if live {
        End::Interrupt(Interruption::MovedOn)
    } else {
        End::Over
    }
}

fn session_is_live(state: &State, session: SessionId) -> bool {
    state
        .session(session)
        .is_some_and(|session| session.is_live())
}

/// Waits until the item is ready again, keeping the harness and its conversation. `None` when it
/// is; otherwise how the task ends instead.
async fn park(
    daemon: &Arc<Daemon>,
    loaded: &Arc<Loaded>,
    run: &Run,
    stopped: &mut mpsc::UnboundedReceiver<Stop>,
    deadline: &Option<Instant>,
) -> Option<End> {
    let mut heads = loaded.storage.watch_heads();
    let wake = Instant::now() + run.park_timeout;
    loop {
        let (session, nudges) = (run.session, run.nudges);
        let Ok((outcome, live)) = daemon
            .read(loaded, move |state| {
                (
                    state.turn_outcome(session, 0, nudges, None),
                    session_is_live(state, session),
                )
            })
            .await
        else {
            return Some(End::Over);
        };
        // A parked session needs nothing of the machine: it waits, whatever is wrong with it.
        match outcome {
            TurnOutcome::Park | TurnOutcome::MachineFault(_) => {}
            TurnOutcome::Close => return Some(closed(live)),
            TurnOutcome::Continue | TurnOutcome::GiveUp => return None,
        }
        tokio::select! {
            changed = heads.changed() => {
                if changed.is_err() {
                    return Some(End::Over);
                }
            }
            Some(_) = stopped.recv() => return Some(End::Over),
            () = tokio::time::sleep_until(wake) => return Some(End::Interrupt(Interruption::TimedOut)),
            () = sleep_until(*deadline) => return Some(End::Interrupt(Interruption::TimedOut)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_write_that_fails_for_lack_of_space_ends_the_session_for_the_machine() {
        let path = Path::new("session.json");
        for end in [not_kept, transcript_failed] {
            let faults = Faults::default();
            let full = std::io::Error::from(std::io::ErrorKind::StorageFull);
            assert!(matches!(
                end(&faults, path, &full),
                End::Interrupt(Interruption::MachineFault(MachineFault::NoSpace))
            ));
            assert_eq!(faults.blocking_all(), Some(MachineFault::NoSpace));

            let faults = Faults::default();
            let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
            assert!(matches!(
                end(&faults, path, &denied),
                End::Interrupt(Interruption::Failed { because }) if because.contains("session.json")
            ));
            assert!(faults.current().is_empty(), "other failures are the work's");
        }
    }
}

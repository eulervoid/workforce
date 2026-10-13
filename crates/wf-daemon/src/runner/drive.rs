//! The task of a running session: prompts the harness turn after turn, parks it while its
//! item waits, and ends it when nobody else does.

use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::time::Instant;
use wf_core::event::CommentKind;
use wf_core::item::ItemView;
use wf_core::lifecycle::{Interruption, SessionEnd, TurnOutcome};
use wf_core::session::CommitId;
use wf_harness::{AgentSessionId, Conversation, Launch, Opening};
use wf_vcs::{GitCli, VcsAdapter};

use super::brief::{brief, restarted, resumed};
use super::resume::AgentSessionRecord;
use super::start::sandboxed;
use super::{Begin, CANCEL_GRACE, End, GRACE, Run, START_TIMEOUT, Stop, blocking};
use crate::{Daemon, Loaded};

/// The task of one session: runs the harness, and ends the session if nobody else does.
pub(super) async fn run_session(
    daemon: Arc<Daemon>,
    loaded: Arc<Loaded>,
    run: Run,
    mut stopped: mpsc::UnboundedReceiver<Stop>,
) {
    let end = drive(&daemon, &loaded, &run, &mut stopped).await;
    if let End::Interrupt(interruption) = end
        && let Err(error) = daemon
            .finish_session(
                run.workforce,
                &loaded,
                run.session,
                SessionEnd::Interruption(interruption),
            )
            .await
    {
        tracing::warn!(
            "could not end session {}: {}",
            run.session,
            wf_core::error_chain(&error)
        );
    }
    daemon.runners.stop(run.session, Stop::Now);
    daemon.held.forget(run.session);
}

async fn drive(
    daemon: &Arc<Daemon>,
    loaded: &Arc<Loaded>,
    run: &Run,
    stopped: &mut mpsc::UnboundedReceiver<Stop>,
) -> End {
    let Ok(transcript) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&run.transcript)
    else {
        return failed(format!(
            "the transcript {} cannot be written",
            run.transcript.display()
        ));
    };
    let mut path =
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect::<Vec<_>>();
    if let Some(directory) = daemon.bin_dir() {
        path.insert(0, directory);
    }
    let path = std::env::join_paths(path).unwrap_or_default();
    let agent = run
        .harness
        .clone()
        .env("WF_SESSION", run.session.to_string())
        .env("WF_WORKFORCE", run.workforce.to_string())
        .env("PATH", path.to_string_lossy());
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
    });
    // A harness that waits on a login or hangs at start can still be stopped; giving up the
    // start stops it.
    let started = tokio::select! {
        started = start => started,
        Some(_) = stopped.recv() => return End::Over,
        () = tokio::time::sleep(START_TIMEOUT) => {
            return failed(format!(
                "{} did not start within {}",
                run.agent,
                humantime::format_duration(START_TIMEOUT)
            ));
        }
    };
    let mut conversation = match started {
        Ok(conversation) => conversation,
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
        Err(error) => {
            let report = wf_core::error_chain(&error);
            tracing::warn!(
                "could not start {} for session {}: {report}",
                run.agent,
                run.session
            );
            return failed(format!("{} did not start: {report}", run.agent));
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
    if let Err(error) = record.write(&run.agent_session) {
        // Without the record the session still runs; it only cannot be resumed after a restart.
        tracing::warn!(
            "session {}: could not keep the agent's id of it in {}: {error}",
            run.session,
            run.agent_session.display()
        );
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
    let deadline = run.max_duration.map(|duration| Instant::now() + duration);
    let mut prompt = match first_prompt(daemon, loaded, run, stopped, &deadline, record).await {
        Ok(prompt) => prompt,
        Err(end) => return end,
    };
    let cancel = conversation.cancel();
    let mut last_head = branch_head(run).await;
    let mut turns_without_commit = 0;
    loop {
        record.prompted_ms = Some(crate::now_ms());
        if let Err(error) = record.write(&run.agent_session) {
            tracing::warn!(
                "session {}: could not keep when it was prompted: {error}",
                run.session
            );
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
            Err(error) => {
                return failed(format!(
                    "{} stopped answering: {}",
                    run.agent,
                    wf_core::error_chain(&error)
                ));
            }
        };

        let head = branch_head(run).await;
        if head != last_head {
            turns_without_commit = 0;
            last_head = head;
        } else {
            turns_without_commit += 1;
        }
        let Ok(state) = daemon.snapshot(loaded).await else {
            return failed("the workforce could not be read".to_string());
        };
        match state.turn_outcome(run.session, turns_without_commit, run.nudges) {
            TurnOutcome::Close => {
                let live = state
                    .session(run.session)
                    .is_some_and(|session| session.is_live());
                return if live {
                    End::Interrupt(Interruption::MovedOn)
                } else {
                    End::Over
                };
            }
            TurnOutcome::Continue => prompt = wf_core::brief::nudge(run.item, run.role),
            TurnOutcome::GiveUp => {
                return End::Interrupt(Interruption::GaveUp {
                    turns: turns_without_commit,
                    last_message,
                });
            }
            TurnOutcome::Park => match park(daemon, loaded, run, stopped, &deadline).await {
                Parked::Ready => {
                    prompt = resumed(loaded, run).await;
                    turns_without_commit = 0;
                }
                Parked::Over => return End::Over,
                Parked::TimedOut => return End::Interrupt(Interruption::TimedOut),
                Parked::MovedOn => return End::Interrupt(Interruption::MovedOn),
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
    let Ok(state) = daemon.snapshot(loaded).await else {
        return Err(failed("the workforce could not be read".to_string()));
    };
    // A session whose last turn asked a blocker was parked when the daemon stopped, even if the
    // blocker is answered by now.
    let asked = match state.item(run.item) {
        Some(ItemView::Live(item)) => item.comments().values().any(|comment| {
            comment.session() == Some(run.session)
                && comment.kind == CommentKind::Blocker
                && comment.at.wall_ms() >= prompted_ms
        }),
        Some(ItemView::Archived(_)) | None => false,
    };
    match state.turn_outcome(run.session, 0, run.nudges) {
        TurnOutcome::Continue | TurnOutcome::GiveUp if asked => Ok(resumed(loaded, run).await),
        TurnOutcome::Continue | TurnOutcome::GiveUp => {
            Ok(restarted(loaded, run, prompted_ms).await)
        }
        TurnOutcome::Close => {
            let live = state
                .session(run.session)
                .is_some_and(|session| session.is_live());
            Err(if live {
                End::Interrupt(Interruption::MovedOn)
            } else {
                End::Over
            })
        }
        TurnOutcome::Park => match park(daemon, loaded, run, stopped, deadline).await {
            Parked::Ready => Ok(resumed(loaded, run).await),
            Parked::Over => Err(End::Over),
            Parked::TimedOut => Err(End::Interrupt(Interruption::TimedOut)),
            Parked::MovedOn => Err(End::Interrupt(Interruption::MovedOn)),
        },
    }
}

fn failed(because: String) -> End {
    End::Interrupt(Interruption::Failed { because })
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

async fn branch_head(run: &Run) -> Option<CommitId> {
    let (repository, branch) = (run.repository.clone(), run.branch.clone());
    blocking(move || GitCli::open(&repository)?.branch_head(&branch))
        .await
        .ok()?
        .ok()
}

enum Parked {
    Ready,
    Over,
    TimedOut,
    MovedOn,
}

/// Waits until the item is ready again, keeping the harness and its conversation.
async fn park(
    daemon: &Arc<Daemon>,
    loaded: &Arc<Loaded>,
    run: &Run,
    stopped: &mut mpsc::UnboundedReceiver<Stop>,
    deadline: &Option<Instant>,
) -> Parked {
    let mut heads = loaded.storage.watch_heads();
    let wake = Instant::now() + run.park_timeout;
    loop {
        let Ok(state) = daemon.snapshot(loaded).await else {
            return Parked::Over;
        };
        match state.turn_outcome(run.session, 0, run.nudges) {
            TurnOutcome::Park => {}
            TurnOutcome::Close => {
                let live = state
                    .session(run.session)
                    .is_some_and(|session| session.is_live());
                return if live { Parked::MovedOn } else { Parked::Over };
            }
            TurnOutcome::Continue | TurnOutcome::GiveUp => return Parked::Ready,
        }
        tokio::select! {
            changed = heads.changed() => {
                if changed.is_err() {
                    return Parked::Over;
                }
            }
            Some(_) = stopped.recv() => return Parked::Over,
            () = tokio::time::sleep_until(wake) => return Parked::TimedOut,
            () = sleep_until(*deadline) => return Parked::TimedOut,
        }
    }
}

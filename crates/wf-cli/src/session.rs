use std::io::Write;
use std::path::Path;
use std::time::Duration;

use crate::transcript::{self, Format, Printer, Verbosity};
use anyhow::{anyhow, bail};
use wf_core::id::{HashId, SessionId};
use wf_harness::Permission;

use crate::cli::{SessionCommand, SessionStartArgs};
use crate::commands::{Scope, choose_item, emit};
use crate::interactive::{self, Choice, Mode};
use crate::render;
use crate::view;
use crate::workspace::{self, Workspace};

const TRANSCRIPT_TIMEOUT: Duration = Duration::from_secs(10);
const FOLLOW_INTERVAL: Duration = Duration::from_millis(200);

pub fn session(
    json: bool,
    mode: Mode,
    command: SessionCommand,
    workspace: &mut Workspace,
    cwd: &Path,
) -> anyhow::Result<()> {
    match command {
        SessionCommand::Start(args) => start(json, mode, workspace, args, cwd),
        SessionCommand::List { live } => {
            let views: Vec<_> = view::session_views(workspace.state(), session_model(workspace))
                .into_iter()
                .filter(|view| !live || view.ended.is_none())
                .collect();
            emit(json, &views, |views| render::sessions(views))
        }
        SessionCommand::Show { session } => {
            let id = choose_session(workspace, mode, session, Which::Any)?;
            let views = view::session_views(workspace.state(), session_model(workspace));
            let shown = views
                .into_iter()
                .find(|view| view.id == id.to_base32())
                .expect("resolved sessions exist");
            emit(json, &shown, render::session)
        }
        SessionCommand::Stop { session } => {
            let id = choose_session(workspace, mode, session, Which::LocalLive)?;
            workspace.stop_session(id)?;
            emit(
                json,
                &crate::commands::Done {
                    message: format!("stopped session {}", id.short()),
                    event: None,
                    session: None,
                    item: None,
                },
                |done| format!("{}\n", done.message),
            )
        }
        SessionCommand::Allow { session } => {
            answer(json, mode, workspace, session, Permission::Allow)
        }
        SessionCommand::Deny { session } => {
            answer(json, mode, workspace, session, Permission::Deny)
        }
        SessionCommand::Watch(args) => {
            if json {
                bail!("`wf session watch` has no --json output; the transcript is JSON lines");
            }
            let id = choose_session_or_item(workspace, mode, args.session)?;
            watch(workspace, id, args.no_follow, args.raw, args.full)
        }
    }
}

fn answer(
    json: bool,
    mode: Mode,
    workspace: &mut Workspace,
    session: Option<String>,
    answer: Permission,
) -> anyhow::Result<()> {
    let id = choose_session(workspace, mode, session, Which::Permission)?;
    let oldest = workspace
        .held_permissions()?
        .into_iter()
        .filter(|held| held.session == id)
        .map(|held| held.since_ms)
        .min()
        .ok_or_else(|| anyhow!("that session is not waiting for an answer"))?;
    let held = workspace.answer_permission(id, oldest, answer)?;
    let verb = match answer {
        Permission::Allow => "allowed",
        Permission::Deny => "denied",
    };
    emit(
        json,
        &crate::commands::Done {
            message: format!("{verb}: {}", held.what),
            event: None,
            session: None,
            item: None,
        },
        |done| format!("{}\n", done.message),
    )
}

/// Which sessions a picker offers.
#[derive(Clone, Copy)]
enum Which {
    LocalLive,
    Permission,
    Transcript,
    Any,
}

fn choose_session(
    workspace: &Workspace,
    mode: Mode,
    given: Option<String>,
    which: Which,
) -> anyhow::Result<SessionId> {
    match given {
        Some(text) => Ok(workspace.state().resolver().session(&text)?),
        None => pick_session(workspace, mode, which),
    }
}

fn pick_session(workspace: &Workspace, mode: Mode, which: Which) -> anyhow::Result<SessionId> {
    if mode == Mode::Scripted {
        bail!("missing session; pass a session id, or run at a terminal to pick one");
    }
    let state = workspace.state();
    let permissions = match which {
        Which::Permission => workspace.held_permissions()?,
        Which::LocalLive | Which::Transcript | Which::Any => Vec::new(),
    };
    let eligible: std::collections::HashSet<String> = state
        .sessions()
        .filter(|session| match which {
            Which::LocalLive => session.is_live() && session.device() == workspace.device_key(),
            Which::Permission => permissions.iter().any(|held| held.session == session.id()),
            Which::Transcript => transcript_path(workspace, session.id()).is_file(),
            Which::Any => true,
        })
        .map(|session| session.id().to_base32())
        .collect();
    let choices = view::session_views(state, session_model(workspace))
        .iter()
        .filter(|view| eligible.contains(&view.id))
        .map(|view| Choice {
            value: view.id.clone(),
            label: render::session_choice(view),
        })
        .collect();
    let picked = interactive::pick("session", choices)?;
    Ok(state.resolver().session(&picked)?)
}

/// A session, or an item's id for its latest session.
fn choose_session_or_item(
    workspace: &Workspace,
    mode: Mode,
    given: Option<String>,
) -> anyhow::Result<SessionId> {
    let Some(text) = given else {
        return pick_session(workspace, mode, Which::Transcript);
    };
    let state = workspace.state();
    match state.resolver().session(&text) {
        Ok(session) => Ok(session),
        Err(session_error) => {
            let item = state
                .resolver()
                .item(text.trim_start_matches('#'))
                .map_err(|_| session_error)?;
            state
                .sessions()
                .filter(|session| session.item() == item)
                .max_by_key(|session| session.started())
                .map(|session| session.id())
                .ok_or_else(|| anyhow!("#{} has no sessions", item.short()))
        }
    }
}

fn start(
    json: bool,
    mode: Mode,
    workspace: &mut Workspace,
    args: SessionStartArgs,
    cwd: &Path,
) -> anyhow::Result<()> {
    if args.watch && json {
        bail!("`--watch` has no --json output; the transcript is JSON lines");
    }
    let agent = match args.agent {
        Some(agent) => agent,
        None => workspace
            .state()
            .policy()
            .and_then(|stored| stored.policy.default_agent.clone())
            .ok_or_else(|| {
                anyhow!("missing --agent; pass one, or set `default_agent` in the policy")
            })?,
    };
    let item = choose_item(workspace, mode, args.item, Scope::Unclaimed)?;
    let (file, _) = workspace::find_workforce(cwd)?;
    let repository = file
        .parent()
        .ok_or_else(|| anyhow!("the workforce file has no directory"))?
        .to_path_buf();
    let session = workspace.start_session(agent.clone(), item, &repository)?;
    emit(
        json,
        &crate::commands::Done {
            message: format!(
                "started session {} of {} on #{}",
                session.short(),
                agent,
                item.short()
            ),
            event: None,
            session: Some(session.to_base32()),
            item: Some(item.to_base32()),
        },
        |done| format!("{}\n", done.message),
    )?;
    if args.watch {
        wait_for_transcript(&transcript_path(workspace, session))?;
        watch(workspace, session, false, args.raw, args.full)?;
    }
    Ok(())
}

/// The agent writes its transcript once it is up, which can be a moment after the session starts.
fn wait_for_transcript(path: &Path) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + TRANSCRIPT_TIMEOUT;
    while !path.exists() {
        if std::time::Instant::now() >= deadline {
            bail!(
                "no transcript at {} yet; try `wf session watch`",
                path.display()
            );
        }
        std::thread::sleep(FOLLOW_INTERVAL);
    }
    Ok(())
}

pub(crate) fn transcripts(workspace: &Workspace) -> std::path::PathBuf {
    workspace
        .data
        .workforce(&workspace.workforce())
        .join("transcripts")
}

pub(crate) fn transcript_path(workspace: &Workspace, session: SessionId) -> std::path::PathBuf {
    transcripts(workspace).join(format!("{session}.jsonl"))
}

/// The model of a session as the daemon of this machine recorded it beside the transcript.
fn session_model(workspace: &Workspace) -> impl Fn(SessionId) -> Option<String> {
    let transcripts = transcripts(workspace);
    move |session| wf_daemon::session_model(&transcripts, session)
}

fn watch(
    workspace: &mut Workspace,
    session: SessionId,
    no_follow: bool,
    raw: bool,
    full: bool,
) -> anyhow::Result<()> {
    let mut reader = transcript::Reader::open(&transcript_path(workspace, session))?;
    let agent = workspace.state().session(session).map_or_else(
        || "agent".to_owned(),
        |session| session.agent().name.to_string(),
    );
    let format = if raw { Format::Raw } else { Format::Styled };
    let verbosity = if full {
        Verbosity::Full
    } else {
        Verbosity::Abridged
    };
    let mut printer = Printer::new(
        &agent,
        transcript::terminal_width(),
        format,
        verbosity,
        jiff::tz::TimeZone::system(),
    );
    let mut ended = false;
    let mut last_refresh = std::time::Instant::now();
    loop {
        let written = reader.read()?;
        if let Some(error) = written.malformed {
            anstream::eprintln!("wf: {error:#}");
        }
        let entries = written.entries;
        if entries.is_empty() {
            if no_follow || ended {
                let ended = ended
                    || workspace
                        .state()
                        .session(session)
                        .is_some_and(|session| !session.is_live());
                let session = if ended {
                    transcript::Session::Ended
                } else {
                    transcript::Session::Live
                };
                anstream::print!("{}", printer.finish(session));
                std::io::stdout().flush()?;
                return Ok(());
            }
            anstream::print!("{}", printer.caught_up(jiff::Timestamp::now()));
            std::io::stdout().flush()?;
            if last_refresh.elapsed() >= Duration::from_secs(1) {
                workspace.refresh()?;
                ended = workspace
                    .state()
                    .session(session)
                    .is_some_and(|session| !session.is_live());
                last_refresh = std::time::Instant::now();
                // Read again after observing the end to drain the final transcript entries.
                if ended {
                    continue;
                }
            }
            std::thread::sleep(FOLLOW_INTERVAL);
            continue;
        }
        for entry in &entries {
            anstream::print!("{}", printer.render(entry));
        }
        std::io::stdout().flush()?;
    }
}

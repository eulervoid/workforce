use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use wf_core::id::{HashId, SessionId};
use wf_daemon::proto::{Answer, Finish};
use wf_harness::{Renderer, TranscriptEntry};

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
        SessionCommand::List => {
            let views = view::session_views(workspace.state());
            emit(json, &views, |views| render::sessions(views))
        }
        SessionCommand::Show { session } => {
            let id = choose_session(workspace, mode, session, Which::Any)?;
            let views = view::session_views(workspace.state());
            let shown = views
                .into_iter()
                .find(|view| view.id == id.to_base32())
                .expect("resolved sessions exist");
            emit(json, &shown, render::session)
        }
        SessionCommand::Stop { session } => {
            let id = choose_session(workspace, mode, session, Which::LocalLive)?;
            workspace.end_session(id, Finish::Stop)?;
            emit(
                json,
                &crate::commands::Done {
                    message: format!("stopped session {}", id.short()),
                    event: String::new(),
                    item: None,
                },
                |done| format!("{}\n", done.message),
            )
        }
        SessionCommand::Allow { session } => answer(json, mode, workspace, session, Answer::Allow),
        SessionCommand::Deny { session } => answer(json, mode, workspace, session, Answer::Deny),
        SessionCommand::Watch(args) => {
            if json {
                bail!("`wf session watch` has no --json output; the transcript is JSON lines");
            }
            let id = choose_session_or_item(workspace, mode, args.session)?;
            watch(workspace, id, args.no_follow)
        }
    }
}

fn answer(
    json: bool,
    mode: Mode,
    workspace: &mut Workspace,
    session: Option<String>,
    answer: Answer,
) -> anyhow::Result<()> {
    let id = choose_session(workspace, mode, session, Which::Permission)?;
    let held = workspace.answer_permission(id, answer)?;
    let verb = match answer {
        Answer::Allow => "allowed",
        Answer::Deny => "denied",
    };
    emit(
        json,
        &crate::commands::Done {
            message: format!("{verb}: {}", held.what),
            event: String::new(),
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
        Which::Permission => workspace.held_permissions(),
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
    let choices = view::session_views(state)
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
    let item = choose_item(workspace, mode, args.item, Scope::Live)?;
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
            event: session.to_base32(),
            item: Some(item.to_base32()),
        },
        |done| format!("{}\n", done.message),
    )?;
    if args.watch {
        wait_for_transcript(&transcript_path(workspace, session))?;
        watch(workspace, session, false)?;
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

fn transcript_path(workspace: &Workspace, session: SessionId) -> std::path::PathBuf {
    workspace
        .data
        .workforce(&workspace.workforce())
        .join("transcripts")
        .join(format!("{session}.jsonl"))
}

fn watch(workspace: &Workspace, session: SessionId, no_follow: bool) -> anyhow::Result<()> {
    let path = transcript_path(workspace, session);
    let mut file = File::open(&path).with_context(|| {
        format!(
            "opening {}; is this the machine the session ran on?",
            path.display()
        )
    })?;
    let mut renderer = Renderer::default();
    let mut pending = String::new();
    let mut buffer = [0u8; 8192];
    let mut unfinished_bytes = Vec::new();
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            if no_follow {
                return Ok(());
            }
            std::thread::sleep(FOLLOW_INTERVAL);
            continue;
        }
        unfinished_bytes.extend_from_slice(&buffer[..read]);
        let valid = match std::str::from_utf8(&unfinished_bytes) {
            Ok(text) => text.len(),
            Err(error) => error.valid_up_to(),
        };
        pending.push_str(std::str::from_utf8(&unfinished_bytes[..valid])?);
        unfinished_bytes.drain(..valid);
        while let Some(newline) = pending.find('\n') {
            let line: String = pending.drain(..=newline).collect();
            let entry: TranscriptEntry = serde_json::from_str(line.trim_end())
                .with_context(|| format!("reading {}", path.display()))?;
            anstream::print!("{}", renderer.render(&entry));
            std::io::stdout().flush()?;
        }
    }
}

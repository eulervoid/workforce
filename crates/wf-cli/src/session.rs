use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use wf_core::id::{HashId, SessionId};
use wf_daemon::proto::{Answer, Finish};
use wf_harness::{Renderer, TranscriptEntry};

use crate::cli::{SessionCommand, SessionStartArgs, SessionWatchArgs};
use crate::commands::emit;
use crate::render;
use crate::view;
use crate::workspace::{self, Workspace};

const FOLLOW_INTERVAL: Duration = Duration::from_millis(200);

pub fn session(
    json: bool,
    command: SessionCommand,
    workspace: &mut Workspace,
    cwd: &Path,
) -> anyhow::Result<()> {
    match command {
        SessionCommand::Start(args) => start(json, workspace, args, cwd),
        SessionCommand::List => {
            let views = view::session_views(workspace.state());
            emit(json, &views, |views| render::sessions(views))
        }
        SessionCommand::Show { session } => {
            let id = resolve(workspace, &session)?;
            let views = view::session_views(workspace.state());
            let shown = views
                .into_iter()
                .find(|view| view.id == id.to_base32())
                .expect("resolved sessions exist");
            emit(json, &shown, render::session)
        }
        SessionCommand::Stop { session } => {
            let id = resolve(workspace, &session)?;
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
        SessionCommand::Allow { session } => answer(json, workspace, &session, Answer::Allow),
        SessionCommand::Deny { session } => answer(json, workspace, &session, Answer::Deny),
        SessionCommand::Watch(args) => {
            if json {
                bail!("`wf session watch` has no --json output; the transcript is JSON lines");
            }
            watch(workspace, args)
        }
    }
}

fn answer(
    json: bool,
    workspace: &mut Workspace,
    session: &str,
    answer: Answer,
) -> anyhow::Result<()> {
    let id = resolve(workspace, session)?;
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

fn resolve(workspace: &Workspace, text: &str) -> anyhow::Result<SessionId> {
    Ok(workspace.state().resolver().session(text)?)
}

fn start(
    json: bool,
    workspace: &mut Workspace,
    args: SessionStartArgs,
    cwd: &Path,
) -> anyhow::Result<()> {
    let item = workspace.resolve_item(&args.item)?;
    let (file, _) = workspace::find_workforce(cwd)?;
    let repository = file
        .parent()
        .ok_or_else(|| anyhow!("the workforce file has no directory"))?
        .to_path_buf();
    let session = workspace.start_session(args.agent.clone(), item, &repository)?;
    emit(
        json,
        &crate::commands::Done {
            message: format!(
                "started session {} of {} on #{}",
                session.short(),
                args.agent,
                item.short()
            ),
            event: session.to_base32(),
            item: Some(item.to_base32()),
        },
        |done| format!("{}\n", done.message),
    )
}

/// The transcript of a session: given its id, or an item's id for the latest session on it.
fn transcript_of(workspace: &Workspace, text: &str) -> anyhow::Result<std::path::PathBuf> {
    let state = workspace.state();
    let session = match state.resolver().session(text) {
        Ok(session) => session,
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
                .ok_or_else(|| anyhow!("#{} has no sessions", item.short()))?
        }
    };
    Ok(workspace
        .data
        .workforce(&workspace.workforce())
        .join("transcripts")
        .join(format!("{session}.jsonl")))
}

fn watch(workspace: &Workspace, args: SessionWatchArgs) -> anyhow::Result<()> {
    let path = transcript_of(workspace, &args.session)?;
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
            if args.no_follow {
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

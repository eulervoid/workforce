use std::io::Write;
use std::path::Path;
use std::time::Duration;

use crate::transcript::{self, Format, Printer, Verbosity};
use anyhow::{anyhow, bail};
use wf_core::id::{HashId, ItemId, SessionId};
use wf_core::name::ReviewStepName;
use wf_core::review::step_names;
use wf_daemon::proto::TranscriptCopy;
use wf_harness::Permission;

use crate::cli::{SessionCommand, SessionStartArgs};
use crate::commands::{Scope, choose_item, emit};
use crate::interactive::{self, Choice, Mode};
use crate::render;
use crate::view;
use crate::workspace::Workspace;

const TRANSCRIPT_TIMEOUT: Duration = Duration::from_secs(10);
const FOLLOW_INTERVAL: Duration = Duration::from_millis(200);
/// How long the daemon may take to first hear from the device of a session about its transcript:
/// longer than it tries to reach a device.
const COPY_TIMEOUT: Duration = Duration::from_secs(60);

pub fn session(
    json: bool,
    mode: Mode,
    command: SessionCommand,
    workspace: &mut Workspace,
) -> anyhow::Result<()> {
    match command {
        SessionCommand::Start(args) => start(json, mode, workspace, args),
        SessionCommand::List { live } => {
            let views: Vec<_> = view::session_views(workspace.state(), session_model(workspace))
                .into_iter()
                .filter(|view| !live || view.status.is_live())
                .collect();
            emit(json, &views, |views| render::sessions(views))
        }
        SessionCommand::Show { session } => {
            let id = choose_session(workspace, mode, session, Which::Any)?;
            let state = workspace.state();
            let session = state.session(id).expect("resolved sessions exist");
            let shown = view::session_view(
                state,
                &state.dependency_graph(),
                session,
                session_model(workspace)(id),
            );
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
            let follow = if args.no_follow {
                Follow::Once
            } else {
                Follow::UntilEnded
            };
            watch(
                workspace,
                id,
                follow,
                watch_format(args.raw),
                watch_verbosity(args.full),
            )
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
    let eligible: std::collections::HashSet<SessionId> = state
        .sessions()
        .filter(|session| match which {
            Which::LocalLive => session.is_live() && session.device() == workspace.device_key(),
            Which::Permission => permissions.iter().any(|held| held.session == session.id()),
            Which::Transcript => {
                session.device() != workspace.device_key()
                    || transcript_path(workspace, session.id()).is_file()
            }
            Which::Any => true,
        })
        .map(|session| session.id())
        .collect();
    let choices = view::session_views(state, session_model(workspace))
        .iter()
        .filter(|view| eligible.contains(&view.id))
        .map(|view| Choice {
            value: view.id.to_base32(),
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
    let step = match args.step {
        Some(step) => Some(step),
        None => pick_review_step(workspace, mode, item)?,
    };
    let session = workspace.start_session(agent.clone(), item, step)?;
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
        watch(
            workspace,
            session,
            Follow::UntilEnded,
            watch_format(args.raw),
            watch_verbosity(args.full),
        )?;
    }
    Ok(())
}

/// The step to review when `item` awaits a review with several steps open: the one picked at a
/// terminal. Otherwise the daemon decides, and tells the open steps when it cannot.
fn pick_review_step(
    workspace: &Workspace,
    mode: Mode,
    item: ItemId,
) -> anyhow::Result<Option<ReviewStepName>> {
    let Some((_, open)) = workspace.state().startable_reviews(item) else {
        return Ok(None);
    };
    if open.len() < 2 || mode == Mode::Scripted {
        return Ok(None);
    }
    let choices = open
        .iter()
        .map(|step| Choice {
            value: step.as_ref().map_or_else(String::new, ToString::to_string),
            label: step_names(std::slice::from_ref(step)),
        })
        .collect();
    let picked = interactive::pick("review step", choices)?;
    // The step without a name is picked as an empty value, and is the one no name stands for.
    Ok(ReviewStepName::new(picked).ok())
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
    workspace.data.transcripts(&workspace.workforce())
}

/// Where the transcript of `session` is read: the transcript itself when the session runs on this
/// device, and otherwise the copy the daemon keeps of it ([`Workspace::follow_transcript`]).
pub(crate) fn transcript_path(workspace: &Workspace, session: SessionId) -> std::path::PathBuf {
    let elsewhere = workspace
        .state()
        .session(session)
        .is_some_and(|session| session.device() != workspace.device_key());
    if elsewhere {
        workspace
            .data
            .transcript_copy(&workspace.workforce(), session)
    } else {
        workspace.data.transcript(&workspace.workforce(), session)
    }
}

/// The copy the daemon keeps of the transcript of a session of another device, and what it last
/// said of it.
pub(crate) struct FollowedCopy {
    session: SessionId,
    /// The name of the device of the session.
    device: String,
    now: TranscriptCopy,
    /// What [`FollowedCopy::news`] last said, so that a device that stays unreachable is said once.
    said: Option<TranscriptCopy>,
}

impl FollowedCopy {
    /// Starts following the transcript of `session` when it is a session of another device.
    pub(crate) fn of(workspace: &Workspace, session: SessionId) -> anyhow::Result<Option<Self>> {
        let state = workspace.state();
        let Some(device) = state
            .session(session)
            .map(|session| session.device())
            .filter(|device| *device != workspace.device_key())
        else {
            return Ok(None);
        };
        let mut copy = Self {
            session,
            device: view::device_name(state, device),
            now: TranscriptCopy::Fetching,
            said: None,
        };
        copy.ask(workspace)?;
        Ok(Some(copy))
    }

    /// Has the daemon follow the transcript for some seconds more, and learns what the copy came
    /// to.
    pub(crate) fn ask(&mut self, workspace: &Workspace) -> anyhow::Result<()> {
        self.now = workspace.follow_transcript(self.session)?.ok_or_else(|| {
            anyhow!(
                "the session runs on {}, and the daemon fetches its transcript; start it with \
                 `wf daemon start`",
                self.device
            )
        })?;
        Ok(())
    }

    pub(crate) fn now(&self) -> &TranscriptCopy {
        &self.now
    }

    /// Whether the device of the session answered since the copy was first followed: the copy
    /// has what the transcript had then, or the device cannot tell.
    fn answered(&self) -> bool {
        match self.now {
            TranscriptCopy::Fetching => false,
            TranscriptCopy::CaughtUp
            | TranscriptCopy::Complete
            | TranscriptCopy::Unreachable { .. }
            | TranscriptCopy::NotHere { .. } => true,
        }
    }

    /// Why the copy falls behind the transcript, when that is new since the last call.
    fn news(&mut self) -> Option<String> {
        if self.said.as_ref() == Some(&self.now) {
            return None;
        }
        self.said = Some(self.now.clone());
        copy_behind(&self.device, &self.now)
    }
}

/// Why the copy of a transcript of `device` falls behind it, as `now` says; `None` when it does
/// not.
pub(crate) fn copy_behind(device: &str, now: &TranscriptCopy) -> Option<String> {
    match now {
        TranscriptCopy::Unreachable { reason, .. } => Some(format!(
            "{device} cannot be reached ({reason}); this is what was fetched before"
        )),
        TranscriptCopy::NotHere { .. } => Some(format!(
            "{device} has no transcript of this session; this is what was fetched before"
        )),
        TranscriptCopy::Fetching | TranscriptCopy::CaughtUp | TranscriptCopy::Complete => None,
    }
}

/// The model of a session as the daemon of this machine recorded it beside the transcript.
fn session_model(workspace: &Workspace) -> impl Fn(SessionId) -> Option<String> {
    let transcripts = transcripts(workspace);
    move |session| wf_daemon::session_model(&transcripts, session)
}

/// Waits until the daemon has a copy of the transcript at `path`, or says why it has none.
fn wait_for_copy(
    workspace: &Workspace,
    copy: &mut FollowedCopy,
    path: &Path,
) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + COPY_TIMEOUT;
    while !path.exists() {
        if copy.answered() {
            let why = copy_behind(&copy.device, copy.now())
                .unwrap_or_else(|| format!("{} has not written it yet", copy.device));
            bail!("nothing of the transcript was fetched: {why}");
        }
        if std::time::Instant::now() >= deadline {
            bail!(
                "{} did not answer about the transcript in time",
                copy.device
            );
        }
        std::thread::sleep(FOLLOW_INTERVAL);
        copy.ask(workspace)?;
    }
    Ok(())
}

/// Whether `watch` waits for the transcript to grow.
#[derive(Clone, Copy)]
enum Follow {
    /// Print what is there and stop.
    Once,
    /// Wait for more until the session has ended.
    UntilEnded,
}

fn watch_format(raw: bool) -> Format {
    if raw { Format::Raw } else { Format::Styled }
}

fn watch_verbosity(full: bool) -> Verbosity {
    if full {
        Verbosity::Full
    } else {
        Verbosity::Abridged
    }
}

fn watch(
    workspace: &mut Workspace,
    session: SessionId,
    follow: Follow,
    format: Format,
    verbosity: Verbosity,
) -> anyhow::Result<()> {
    let mut copy = FollowedCopy::of(workspace, session)?;
    let path = transcript_path(workspace, session);
    if let Some(copy) = &mut copy {
        wait_for_copy(workspace, copy, &path)?;
    }
    let mut reader = transcript::Reader::open(&path)?;
    let agent = workspace.state().session(session).map_or_else(
        || "agent".to_owned(),
        |session| session.agent().name.to_string(),
    );
    let mut printer = Printer::new(
        &agent,
        transcript::terminal_width(),
        format,
        verbosity,
        jiff::tz::TimeZone::system(),
    );
    let session_ended = |workspace: &Workspace| {
        workspace
            .state()
            .session(session)
            .is_some_and(|session| !session.is_live())
    };
    // A copy is all there once the daemon says so, which can be after the session ended.
    let transcript_ended = |workspace: &Workspace, copy: &Option<FollowedCopy>| match copy {
        None => session_ended(workspace),
        Some(copy) => match copy.now() {
            TranscriptCopy::Complete => true,
            // What was fetched is all there is to show.
            TranscriptCopy::Unreachable { .. } | TranscriptCopy::NotHere { .. } => {
                session_ended(workspace)
            }
            TranscriptCopy::Fetching | TranscriptCopy::CaughtUp => false,
        },
    };
    let mut ended = transcript_ended(workspace, &copy);
    let mut seen = workspace.state().event_count() as u64;
    let mut last_refresh = std::time::Instant::now();
    loop {
        let written = reader.read()?;
        if let Some(error) = written.malformed {
            anstream::eprintln!("wf: {error:#}");
        }
        let entries = written.entries;
        if entries.is_empty() {
            let once_done =
                matches!(follow, Follow::Once) && copy.as_ref().is_none_or(FollowedCopy::answered);
            if once_done || ended {
                if let Some(news) = copy.as_mut().and_then(FollowedCopy::news) {
                    anstream::eprintln!("wf: {news}");
                }
                let session = if session_ended(workspace) {
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
                last_refresh = std::time::Instant::now();
                let new = workspace.events_since(seen)?;
                if !new.is_empty() {
                    seen += new.len() as u64;
                    workspace.refresh()?;
                }
                if let Some(copy) = &mut copy {
                    copy.ask(workspace)?;
                    if let Some(news) = copy.news() {
                        anstream::eprintln!("wf: {news}");
                    }
                }
                ended = transcript_ended(workspace, &copy);
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

//! The dashboard: `wf watch` at a terminal. It shows what waits on the principal, what runs and
//! what is stuck, and answers what waits.

mod act;
mod app;
mod draw;
mod snapshot;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event as TerminalEvent, KeyEventKind};
use wf_core::event::Event;
use wf_core::id::{ItemId, SessionId};

use self::app::{App, Effect};
use self::snapshot::{Detail, Snapshot, Sources};
use crate::workspace::{self, Workspace};
use crate::{render, transcript, view};

/// How often the daemon is asked for what is new.
const REFRESH_EVERY: Duration = Duration::from_millis(500);

/// How many of the latest events the Log tab starts with.
const BACKLOG: usize = 200;

pub fn run(workspace: &mut Workspace, only: Option<ItemId>) -> anyhow::Result<()> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, workspace, only);
    ratatui::restore();
    result
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    workspace: &mut Workspace,
    only: Option<ItemId>,
) -> anyhow::Result<()> {
    let mut app = App::new(Snapshot::default(), jiff::tz::TimeZone::system());
    app.resize(terminal.size()?.width);
    let mut feed = Feed::new(workspace, &mut app)?;
    if let Some(item) = only {
        app.open_item(item);
    }
    feed.read_transcripts(workspace, &mut app);
    let snapshot = feed.snapshot(workspace, &app);
    app.refresh(snapshot);
    let mut refreshed = Instant::now();
    loop {
        terminal.draw(|frame| draw::draw(frame, &app))?;
        let wait = REFRESH_EVERY.saturating_sub(refreshed.elapsed());
        if event::poll(wait)? {
            match event::read()? {
                TerminalEvent::Key(key) if key.kind == KeyEventKind::Press => match app.key(key) {
                    Some(Effect::Quit) => return Ok(()),
                    Some(effect) => {
                        match act::carry_out(effect, workspace, terminal) {
                            Ok(act::Outcome::Did(what)) => app.did(what),
                            Ok(act::Outcome::Choose(choice)) => app.choose(choice),
                            Err(error) => app.action_failed(&error),
                        }
                        // Show what the action changed right away.
                        if let Err(error) = feed.refresh(workspace, &mut app) {
                            app.refresh_failed(&error);
                        }
                    }
                    None => {}
                },
                TerminalEvent::Resize(width, _) => app.resize(width),
                _ => continue,
            }
            // What is on screen may have changed: show it without waiting for the daemon.
            feed.read_transcripts(workspace, &mut app);
            let snapshot = feed.snapshot(workspace, &app);
            app.refresh(snapshot);
            continue;
        }
        refreshed = Instant::now();
        match feed.refresh(workspace, &mut app) {
            Ok(()) => {
                feed.read_transcripts(workspace, &mut app);
                let snapshot = feed.snapshot(workspace, &app);
                app.refresh(snapshot);
            }
            Err(error) => app.refresh_failed(&error),
        }
    }
}

/// What the dashboard has read from the workforce so far.
struct Feed {
    events: Vec<Event>,
    /// The transcripts followed, by session.
    readers: HashMap<SessionId, transcript::Reader>,
}

impl Feed {
    /// Reads the log so far, and hands the app the lines of its latest events.
    fn new(workspace: &Workspace, app: &mut App) -> anyhow::Result<Self> {
        let events = workspace.events()?;
        let backlog = &events[events.len().saturating_sub(BACKLOG)..];
        app.logged(log_lines(workspace.state(), &events, backlog, app.width));
        Ok(Self {
            events,
            readers: HashMap::new(),
        })
    }

    /// Asks the daemon for the state and the new events, and hands the app their lines.
    fn refresh(&mut self, workspace: &mut Workspace, app: &mut App) -> anyhow::Result<()> {
        workspace.refresh()?;
        let new = workspace.events_since(self.events.len() as u64)?;
        let known = self.events.len();
        self.events.extend(new);
        let lines = log_lines(
            workspace.state(),
            &self.events,
            &self.events[known..],
            app.width,
        );
        app.logged(lines);
        Ok(())
    }

    /// Hands the app what was written to the transcripts of this machine's live sessions, and
    /// of the session on screen.
    fn read_transcripts(&mut self, workspace: &Workspace, app: &mut App) {
        let own = workspace.device_key();
        let mut sessions: Vec<SessionId> = workspace
            .state()
            .live_sessions()
            .filter(|session| session.device() == own)
            .map(|session| session.id())
            .collect();
        if let Some(Detail::Session(session)) = app.wanted().detail {
            sessions.push(session);
        }
        for session in sessions {
            let reader = match self.readers.entry(session) {
                std::collections::hash_map::Entry::Occupied(reader) => reader.into_mut(),
                std::collections::hash_map::Entry::Vacant(vacant) => {
                    let path = crate::session::transcript_path(workspace, session);
                    // The agent may not have written its transcript yet: try again next time.
                    let Ok(reader) = transcript::Reader::open(&path) else {
                        continue;
                    };
                    vacant.insert(reader)
                }
            };
            match reader.read() {
                Ok(entries) => app.transcribed(session, &entries),
                Err(error) => app.refresh_failed(&error),
            }
        }
    }

    fn snapshot(&self, workspace: &Workspace, app: &App) -> Snapshot {
        let held = workspace.held_permissions();
        let last_seen = workspace.last_seen();
        let sources = Sources {
            state: workspace.state(),
            events: &self.events,
            held: &held,
            own: workspace.device_key(),
            last_seen: &last_seen,
            now_ms: workspace::now_ms(),
            detail_width: app.detail_width(),
        };
        Snapshot::take(&sources, app.wanted())
    }
}

/// What the `new` events did, as `wf watch` prints it, read against all `events`, for a window
/// `width` wide.
fn log_lines(
    state: &wf_core::state::State,
    events: &[Event],
    new: &[Event],
    width: u16,
) -> Vec<String> {
    // Within the margin of the Log tab.
    let width = Some(usize::from(width.saturating_sub(1)).min(100));
    view::watch_lines(state, events, new)
        .iter()
        .map(|line| render::watch_line_for(line, width))
        .collect()
}

#[cfg(test)]
mod tests;

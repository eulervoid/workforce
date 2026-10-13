//! The dashboard: `wf watch` at a terminal. It shows what waits on the principal, what runs and
//! what is stuck, and answers what waits.

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
use self::snapshot::{Snapshot, Sources, Wanted};
use crate::transcript;
use crate::workspace::{self, Workspace};

/// How often the daemon is asked for what is new.
const REFRESH_EVERY: Duration = Duration::from_millis(500);

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
    let mut feed = Feed::new(workspace)?;
    let mut app = App::new(Snapshot::default(), jiff::tz::TimeZone::system());
    app.resize(terminal.size()?.width);
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
        match feed.refresh(workspace) {
            Ok(()) => {
                feed.read_transcripts(workspace, &mut app);
                let snapshot = feed.snapshot(workspace, &app);
                app.refresh(snapshot);
            }
            Err(error) => app.failed(&error),
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
    fn new(workspace: &Workspace) -> anyhow::Result<Self> {
        Ok(Self {
            events: workspace.events()?,
            readers: HashMap::new(),
        })
    }

    fn refresh(&mut self, workspace: &mut Workspace) -> anyhow::Result<()> {
        workspace.refresh()?;
        let new = workspace.events_since(self.events.len() as u64)?;
        self.events.extend(new);
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
        if let Some(Wanted::Session(session)) = app.wanted() {
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
                Err(error) => app.failed(&error),
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

#[cfg(test)]
mod tests;

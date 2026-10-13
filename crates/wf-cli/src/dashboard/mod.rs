//! The dashboard: `wf watch` at a terminal. It shows what waits on the principal, what runs and
//! what is stuck, and answers what waits.

mod app;
mod draw;
mod snapshot;

use std::time::{Duration, Instant};

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event as TerminalEvent, KeyEventKind};
use wf_core::event::Event;
use wf_core::id::ItemId;

use self::app::{App, Effect};
use self::snapshot::{Counts, Snapshot};
use crate::view;
use crate::workspace::Workspace;

/// How often the daemon is asked for what is new.
const REFRESH_EVERY: Duration = Duration::from_millis(500);

pub fn run(workspace: &mut Workspace, _only: Option<ItemId>) -> anyhow::Result<()> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, workspace);
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut DefaultTerminal, workspace: &mut Workspace) -> anyhow::Result<()> {
    let mut feed = Feed::new(workspace)?;
    let mut app = App::new(feed.snapshot(workspace));
    let mut refreshed = Instant::now();
    loop {
        terminal.draw(|frame| draw::draw(frame, &app))?;
        let wait = REFRESH_EVERY.saturating_sub(refreshed.elapsed());
        if event::poll(wait)? {
            if let TerminalEvent::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                match app.key(key) {
                    Some(Effect::Quit) => return Ok(()),
                    None => {}
                }
            }
            continue;
        }
        refreshed = Instant::now();
        match feed.refresh(workspace) {
            Ok(()) => app.refresh(feed.snapshot(workspace)),
            Err(error) => app.failed(&error),
        }
    }
}

/// What the dashboard has read from the workforce so far.
struct Feed {
    events: Vec<Event>,
}

impl Feed {
    fn new(workspace: &Workspace) -> anyhow::Result<Self> {
        Ok(Self {
            events: workspace.events()?,
        })
    }

    fn refresh(&mut self, workspace: &mut Workspace) -> anyhow::Result<()> {
        workspace.refresh()?;
        let new = workspace.events_since(self.events.len() as u64)?;
        self.events.extend(new);
        Ok(())
    }

    fn snapshot(&self, workspace: &Workspace) -> Snapshot {
        let state = workspace.state();
        let latest = self.events.last().and_then(|event| {
            view::watch_lines(state, &self.events, std::slice::from_ref(event))
                .last()
                .map(crate::render::watch_line)
        });
        Snapshot {
            name: state.name().unwrap_or("workforce").to_owned(),
            counts: Counts::of(state),
            waiting: view::inbox_views(state, &workspace.held_permissions()).len(),
            latest,
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent};

    use super::app::Tab;
    use super::*;

    pub(super) fn screen(app: &App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw::draw(frame, app)).unwrap();
        terminal.backend().to_string()
    }

    pub(super) fn snapshot() -> Snapshot {
        Snapshot {
            name: "workforce".to_owned(),
            counts: Counts {
                live_sessions: 3,
                open_items: 14,
                blocked_items: 1,
                devices: 2,
            },
            waiting: 3,
            latest: Some(
                "\u{1b}[2m07:04\u{1b}[0m  #ab4c  coder submitted 3e8a1c → in_review\n".to_owned(),
            ),
        }
    }

    #[test]
    fn the_frame_has_tabs_counts_and_the_latest_event() {
        let app = App::new(snapshot());
        insta::assert_snapshot!(screen(&app, 60, 8));
    }

    #[test]
    fn keys_switch_tabs_and_quit() {
        let mut app = App::new(snapshot());
        assert_eq!(app.key(KeyEvent::from(KeyCode::Char('3'))), None);
        assert_eq!(app.tab, Tab::Agents);
        app.key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(app.tab, Tab::Log);
        app.key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(app.tab, Tab::Now);
        app.key(KeyEvent::from(KeyCode::BackTab));
        assert_eq!(app.tab, Tab::Log);
        assert_eq!(
            app.key(KeyEvent::from(KeyCode::Char('q'))),
            Some(Effect::Quit)
        );
    }
}

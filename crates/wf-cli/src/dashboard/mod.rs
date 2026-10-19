//! The dashboard: plain `wf` at a terminal. It shows what waits on the principal, what runs and
//! what is stuck, and answers what waits.

mod act;
mod app;
mod diff;
mod draw;
mod draw_diff;
mod links;
mod snapshot;
mod transcript_lines;

use std::collections::HashMap;
use std::io::Write;
use std::time::{Duration, Instant};

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, Event as TerminalEvent, KeyEventKind, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::crossterm::execute;
use wf_core::event::Event;
use wf_core::id::{ItemId, SessionId};

use self::app::{App, Effect, Wheel};
use self::diff::{Changes, ItemWork};
use self::snapshot::{Detail, InView, Snapshot, Sources};
use crate::transcript::Format;
use crate::workspace::Workspace;
use crate::{commands, render, transcript, view};

/// How often the daemon is asked for what is new.
const REFRESH_EVERY: Duration = Duration::from_millis(500);

/// How many of the latest events the Log tab starts with.
const BACKLOG: usize = 200;

pub fn run(workspace: &mut Workspace, only: Option<ItemId>) -> anyhow::Result<()> {
    let mut terminal = ratatui::init();
    // A panic restores the terminal through ratatui's hook, which leaves the mouse and the focus
    // reported.
    let restore = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = stop_reports();
        restore(info);
    }));
    let result = start_reports()
        .map_err(anyhow::Error::from)
        .and_then(|()| event_loop(&mut terminal, workspace, only));
    let _ = stop_reports();
    ratatui::restore();
    result
}

/// Asks the terminal to report presses of the mouse and its wheel, so a click picks and the wheel
/// scrolls what is under the pointer, rather than the wheel sending arrow keys; and every move of
/// the pointer, so a link under it stands out. A move costs a frame only when it goes onto or off
/// a link or button. And whether the terminal pane is focused, so the accent dims while it is
/// not; tmux passes that on only with `set -g focus-events on`. And pasted text as one event, so
/// its line breaks are not taken for the Enter key.
pub(super) fn start_reports() -> std::io::Result<()> {
    let mut stdout = std::io::stdout();
    // Presses, releases and moves (1003), in the SGR encoding (1006).
    stdout.write_all(b"\x1b[?1003h\x1b[?1006h")?;
    execute!(stdout, EnableFocusChange, EnableBracketedPaste)
}

/// Stops the reports [`start_reports`] asked for.
pub(super) fn stop_reports() -> std::io::Result<()> {
    execute!(
        std::io::stdout(),
        DisableMouseCapture,
        DisableFocusChange,
        DisableBracketedPaste
    )
}

/// What an event from the terminal came to.
enum Handled {
    Quit,
    /// Something moved on screen: draw it again, and ask the workspace about what came into view.
    Moved,
    /// The workforce or the window changed: ask the workspace again, and draw.
    Changed,
    Ignored,
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    workspace: &mut Workspace,
    only: Option<ItemId>,
) -> anyhow::Result<()> {
    let mut app = App::new(Snapshot::default(), jiff::tz::TimeZone::system());
    let size = terminal.size()?;
    app.resize(size.width, size.height);
    let mut feed = Feed::new(workspace, &mut app)?;
    if let Some(item) = only {
        app.open_item(item);
    }
    feed.read_transcripts(workspace, &mut app);
    feed.read_diff(workspace, &mut app);
    let snapshot = feed.snapshot(workspace, &app)?;
    app.refresh(snapshot);
    let mut refreshed = Instant::now();
    let mut draw = true;
    loop {
        if draw {
            terminal.draw(|frame| draw::draw(frame, &mut app))?;
            draw = false;
        }
        // Wait for the terminal until the next refresh, then take in all it has queued before
        // drawing again, so a fast wheel does not queue up draws.
        let wanted = app.wanted();
        let mut changed = false;
        let mut wait = REFRESH_EVERY.saturating_sub(refreshed.elapsed());
        while event::poll(wait)? {
            wait = Duration::ZERO;
            match handle(event::read()?, &mut app, &mut feed, workspace, terminal) {
                Handled::Quit => return Ok(()),
                Handled::Moved => draw = true,
                Handled::Changed => changed = true,
                Handled::Ignored => {}
            }
        }
        if refreshed.elapsed() >= REFRESH_EVERY {
            refreshed = Instant::now();
            match feed.refresh(workspace, &mut app) {
                Ok(()) => app.refreshed(),
                Err(error) => {
                    app.refresh_failed(&error);
                    draw = true;
                    continue;
                }
            }
        } else if !changed {
            let now_wanted = app.wanted();
            if now_wanted == wanted {
                // Scrolling within what is on screen needs nothing new from the workspace.
                continue;
            }
            if now_wanted.items == wanted.items && now_wanted.agents == wanted.agents {
                // A move of the selection needs only the detail it comes to.
                draw = true;
                feed.read_transcripts(workspace, &mut app);
                feed.read_diff(workspace, &mut app);
                let in_view = feed.in_view(workspace, &app);
                app.refresh_detail(in_view);
                continue;
            }
        }
        draw = true;
        feed.read_transcripts(workspace, &mut app);
        feed.read_diff(workspace, &mut app);
        let snapshot = feed.snapshot(workspace, &app)?;
        app.refresh(snapshot);
    }
}

fn handle(
    event: TerminalEvent,
    app: &mut App,
    feed: &mut Feed,
    workspace: &mut Workspace,
    terminal: &mut DefaultTerminal,
) -> Handled {
    let effect = match event {
        TerminalEvent::Key(key) if key.kind == KeyEventKind::Press => app.key(key),
        TerminalEvent::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column,
            ..
        }) => {
            app.wheel(column, Wheel::Up);
            None
        }
        TerminalEvent::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column,
            ..
        }) => {
            app.wheel(column, Wheel::Down);
            None
        }
        TerminalEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            ..
        }) => app.click(column, row),
        TerminalEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column,
            row,
            ..
        }) => {
            app.drag(column, row);
            None
        }
        TerminalEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column,
            row,
            ..
        }) => {
            return if app.hover(column, row) {
                Handled::Moved
            } else {
                Handled::Ignored
            };
        }
        TerminalEvent::FocusGained | TerminalEvent::FocusLost => {
            let focused = matches!(event, TerminalEvent::FocusGained);
            return if app.terminal_focus(focused) {
                Handled::Moved
            } else {
                Handled::Ignored
            };
        }
        TerminalEvent::Paste(text) => {
            return if app.paste(&text) {
                Handled::Moved
            } else {
                Handled::Ignored
            };
        }
        TerminalEvent::Resize(width, height) => {
            app.resize(width, height);
            return Handled::Changed;
        }
        _ => return Handled::Ignored,
    };
    match effect {
        Some(Effect::Quit) => Handled::Quit,
        Some(effect) => {
            match act::carry_out(effect, workspace, terminal) {
                Ok(act::Outcome::Did(what)) => app.did(what, wf_store::now_ms()),
                Ok(act::Outcome::Choose(choice)) => app.choose(choice),
                Ok(act::Outcome::Drafted {
                    item,
                    comment,
                    replacing,
                }) => app.drafted(item, comment, replacing),
                Ok(act::Outcome::Reviewed { item, what }) => {
                    app.reviewed(item);
                    app.did(what, wf_store::now_ms());
                }
                Ok(act::Outcome::ReviewFailed {
                    item,
                    posted,
                    error,
                }) => {
                    app.drafts_posted(item, posted);
                    app.action_failed(&error, wf_store::now_ms());
                }
                Err(error) => app.action_failed(&error, wf_store::now_ms()),
            }
            // Show what the action changed right away.
            match feed.refresh(workspace, app) {
                Ok(()) => app.refreshed(),
                Err(error) => app.refresh_failed(&error),
            }
            Handled::Changed
        }
        None => Handled::Moved,
    }
}

/// What the dashboard has read from the workforce so far.
struct Feed {
    events: Vec<Event>,
    /// The transcripts followed, by session.
    readers: HashMap<SessionId, transcript::Reader>,
    /// The repository the diffs are read from, once one was.
    git: Option<wf_vcs::GitCli>,
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
            git: None,
        })
    }

    /// Asks the daemon for the state and the new events, and hands the app their lines and the
    /// moves back to `open` among them to warn of.
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
        app.warn(snapshot::moves_back(&self.events, known));
        Ok(())
    }

    /// Hands the app what was written to the transcripts of this machine's live sessions, and
    /// of the session on screen if it ran here; those of other sessions are let go.
    fn read_transcripts(&mut self, workspace: &Workspace, app: &mut App) {
        let own = workspace.device_key();
        let state = workspace.state();
        let mut sessions: Vec<SessionId> = state
            .live_sessions()
            .filter(|session| session.device() == own)
            .map(|session| session.id())
            .collect();
        if let Some(Detail::Session(session)) = app.wanted().detail
            && state
                .session(session)
                .is_some_and(|session| session.device() == own)
        {
            sessions.push(session);
        }
        self.readers.retain(|session, _| sessions.contains(session));
        app.keep_transcripts(&sessions);
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
                Ok(written) => {
                    app.transcribed(session, &written.entries);
                    if let Some(error) = written.malformed {
                        app.transcript_failed(&error);
                    }
                }
                Err(error) => app.transcript_failed(&error),
            }
        }
    }

    /// Hands the app the work of the item whose diff is on screen, when its branch moved since
    /// it was last read, or why it cannot be read.
    fn read_diff(&mut self, workspace: &Workspace, app: &mut App) {
        let Some(Detail::Diff(item)) = app.wanted().detail else {
            return;
        };
        let Some(pane) = app.diffs.get_mut(&item) else {
            return;
        };
        let mut read = || -> anyhow::Result<Option<ItemWork>> {
            let git = match &self.git {
                Some(git) => git,
                None => self.git.insert(commands::repository()?),
            };
            let work = commands::item_work(workspace, git, item)?;
            let commit = work.shown();
            if pane.commit() == Some(commit) {
                return Ok(None);
            }
            let changes = if git.has_commits(&work)? {
                Changes::Files(git.file_diffs(&work)?)
            } else {
                Changes::NoCommits
            };
            Ok(Some(ItemWork { commit, changes }))
        };
        match read() {
            Ok(None) => {}
            Ok(Some(work)) => pane.read(Ok(work)),
            Err(error) => pane.read(Err(format!("{error:#}"))),
        }
    }

    fn sources<'a>(&'a self, workspace: &'a Workspace, app: &App) -> Sources<'a> {
        Sources {
            state: workspace.state(),
            events: &self.events,
            own: workspace.device_key(),
            now_ms: wf_store::now_ms(),
            detail_width: app.detail_width(),
        }
    }

    fn snapshot(&self, workspace: &Workspace, app: &App) -> anyhow::Result<Snapshot> {
        let held = workspace.held_permissions()?;
        let machine_faults = workspace.machine_faults()?;
        let last_seen = workspace.last_seen()?;
        let sources = self.sources(workspace, app);
        Ok(Snapshot::take(
            &sources,
            &held,
            &machine_faults,
            &last_seen,
            app.wanted(),
        ))
    }

    /// What the detail on screen shows, read against what the snapshot on screen flags.
    fn in_view(&self, workspace: &Workspace, app: &App) -> Option<InView> {
        let detail = app.wanted().detail?;
        let sources = self.sources(workspace, app);
        InView::take(&sources, &app.snapshot.flagged, detail)
    }
}

/// What the `new` events did, as `wf log --follow` prints it, read against all `events`, for a window
/// `width` wide.
fn log_lines(
    state: &wf_core::state::State,
    events: &[Event],
    new: &[Event],
    width: u16,
) -> Vec<String> {
    // Within the margin of the Log tab.
    let width = Some(usize::from(width.saturating_sub(1)).min(100));
    view::event_lines(state, events, new)
        .iter()
        .map(|line| render::event_line_for(line, width, Format::Styled))
        .collect()
}

#[cfg(test)]
mod tests;

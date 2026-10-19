//! The dashboard: plain `wf` at a terminal. It shows what waits on the principal, what runs and
//! what is stuck, and answers what waits.

mod act;
mod app;
mod diff;
mod draw;
mod draw_diff;
mod hint;
mod links;
mod restart;
mod snapshot;
mod transcript_lines;

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::time::{Duration, Instant};

use anyhow::anyhow;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, Event as TerminalEvent, KeyEventKind, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::crossterm::execute;
use wf_core::event::Event;
use wf_core::id::{ItemId, SessionId};
use wf_daemon::proto::HeldPermission;

use self::app::{App, Wheel};
use self::diff::{Changes, ItemWork};
use self::restart::{Executable, Mismatch};
use self::snapshot::{Detail, InView, Snapshot, Sources, Wanted};
use crate::commands::{DeviceDiff, ItemDiff};
use crate::transcript::Format;
use crate::workspace::{self, Reconnect, Workspace};
use crate::{commands, render, transcript, view};

/// How often the daemon is asked for what is new.
const REFRESH_EVERY: Duration = Duration::from_millis(500);

/// How often the diff on screen is asked of the device of its session, which is a request over
/// the network rather than a read of a branch.
const ASK_DEVICE_EVERY: Duration = Duration::from_secs(3);

/// How often the worktree of the diff on screen is read again while the branch head stays.
const READ_WORKTREE_EVERY: Duration = Duration::from_secs(2);

/// How many of the latest events the Log tab starts with.
const BACKLOG: usize = 200;

pub fn run(workspace: &mut Workspace, only: Option<ItemId>) -> anyhow::Result<()> {
    let executable = Executable::start()?;
    let rebuilt = executable.path().to_path_buf();
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
        .and_then(|()| event_loop(&mut terminal, workspace, only, executable));
    let _ = stop_reports();
    ratatui::restore();
    match result? {
        Exit::Quit => Ok(()),
        Exit::RunRebuilt => {
            // Only returns when the executable could not be run.
            let error = std::process::Command::new(rebuilt)
                .args(std::env::args_os().skip(1))
                .exec();
            Err(anyhow::Error::from(error).context("running the rebuilt wf"))
        }
    }
}

/// How the dashboard ended.
enum Exit {
    Quit,
    /// The daemon is a newer build: the executable that replaced this one takes over.
    RunRebuilt,
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
    RunRebuilt,
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
    executable: Executable,
) -> anyhow::Result<Exit> {
    let mut app = App::new(Snapshot::default(), jiff::tz::TimeZone::system());
    let size = terminal.size()?;
    app.resize(size.width, size.height);
    let mut feed = Feed::new(workspace, &mut app, executable)?;
    if let Some(item) = only {
        app.open_item(item);
    }
    feed.read_transcripts(workspace, &mut app);
    feed.read_diff(workspace, &mut app);
    if let Some(snapshot) = feed.snapshot(workspace, &app)? {
        app.refresh(snapshot);
    }
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
                Handled::Quit => return Ok(Exit::Quit),
                Handled::RunRebuilt => return Ok(Exit::RunRebuilt),
                Handled::Moved => draw = true,
                Handled::Changed => changed = true,
                Handled::Ignored => {}
            }
        }
        if refreshed.elapsed() >= REFRESH_EVERY {
            refreshed = Instant::now();
            match feed.refresh_through_restart(workspace, &mut app) {
                Ok(Refreshed::Read) => app.refreshed(),
                Ok(Refreshed::RunRebuilt) => return Ok(Exit::RunRebuilt),
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
            if now_wanted.listing == wanted.listing {
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
        match feed.snapshot(workspace, &app)? {
            Some(snapshot) => app.refresh(snapshot),
            None => app.tick(wf_store::now_ms()),
        }
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
        TerminalEvent::Key(_) | TerminalEvent::Mouse(_) => return Handled::Ignored,
    };
    match effect {
        Some(effect) => {
            match act::carry_out(effect, workspace, terminal) {
                Ok(act::Outcome::Quit) => return Handled::Quit,
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
            match feed.refresh_through_restart(workspace, app) {
                Ok(Refreshed::Read) => app.refreshed(),
                Ok(Refreshed::RunRebuilt) => return Handled::RunRebuilt,
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
    /// What the snapshot on screen was taken from.
    taken: Option<Taken>,
    /// When each diff was last asked of the device of its session.
    asked_devices: Asks,
    /// When the worktree of each diff on screen was last read.
    read_worktrees: Asks,
    /// The executable as it was when the dashboard started.
    executable: Executable,
}

/// What came of a refresh.
enum Refreshed {
    Read,
    /// The daemon is a newer build, and the executable was rebuilt to speak to it.
    RunRebuilt,
}

/// When each item was last asked about, however that came out, so a device that cannot be reached
/// or a worktree is not read on every tick.
struct Asks {
    every: Duration,
    at: HashMap<ItemId, Instant>,
}

impl Asks {
    fn new(every: Duration) -> Self {
        Self {
            every,
            at: HashMap::new(),
        }
    }

    fn due(&self, item: ItemId) -> bool {
        self.at
            .get(&item)
            .is_none_or(|asked| asked.elapsed() >= self.every)
    }

    fn asked(&mut self, item: ItemId) {
        self.at.insert(item, Instant::now());
    }

    /// The item is asked about again, whatever is held back.
    fn forget(&mut self, item: ItemId) {
        self.at.remove(&item);
    }
}

impl Feed {
    /// Reads the log so far, and hands the app the lines of its latest events.
    fn new(workspace: &Workspace, app: &mut App, executable: Executable) -> anyhow::Result<Self> {
        let events = workspace.events()?;
        let backlog = &events[events.len().saturating_sub(BACKLOG)..];
        app.logged(log_lines(workspace.state(), &events, backlog, app.width));
        Ok(Self {
            events,
            readers: HashMap::new(),
            git: None,
            taken: None,
            asked_devices: Asks::new(ASK_DEVICE_EVERY),
            read_worktrees: Asks::new(READ_WORKTREE_EVERY),
            executable,
        })
    }

    /// [`Feed::refresh`], and when the connection to the daemon broke because it restarted,
    /// connects again, on every refresh until the daemon answers. A newer daemon than this build
    /// is met by the rebuilt executable if there is one, and otherwise says which side to restart.
    fn refresh_through_restart(
        &mut self,
        workspace: &mut Workspace,
        app: &mut App,
    ) -> anyhow::Result<Refreshed> {
        let error = match self.refresh(workspace, app) {
            Ok(()) => return Ok(Refreshed::Read),
            Err(error) => error,
        };
        if !workspace::connection_broke(&error) {
            return Err(error);
        }
        match workspace.reconnect() {
            Ok(()) => self.refresh(workspace, app).map(|()| Refreshed::Read),
            Err(Reconnect::NoDaemon) => Err(anyhow!("the daemon is restarting")),
            Err(Reconnect::Failed(error)) => Err(error),
            Err(Reconnect::Refused { daemon, client }) => {
                match restart::on_mismatch(daemon, client, self.executable.changed()) {
                    Mismatch::RunRebuilt => Ok(Refreshed::RunRebuilt),
                    Mismatch::RestartDaemon => Err(anyhow!(
                        "the daemon speaks protocol {daemon}, older than this build's {client}; \
                         restart it with `wf daemon restart`"
                    )),
                    Mismatch::RebuildAndStart => Err(anyhow!(
                        "the daemon speaks protocol {daemon}, newer than this build's {client}; \
                         rebuild wf and start it again"
                    )),
                }
            }
        }
    }

    /// Asks the daemon for the new events, and for the state when there are some, and hands the
    /// app their lines and the moves back to `open` among them to warn of.
    fn refresh(&mut self, workspace: &mut Workspace, app: &mut App) -> anyhow::Result<()> {
        let new = workspace.events_since(self.events.len() as u64)?;
        if new.is_empty() {
            return Ok(());
        }
        workspace.refresh()?;
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
        if !self.asked_devices.due(item) {
            return;
        }
        let mut read = || -> anyhow::Result<()> {
            let git = match &self.git {
                Some(git) => git,
                None => self.git.insert(workspace.repository()?),
            };
            let now_ms = wf_store::now_ms();
            match commands::item_diff(workspace, git, item, pane.commit())? {
                ItemDiff::Local(work) => {
                    self.asked_devices.forget(item);
                    let commit = work.shown();
                    let has_worktree = matches!(work, wf_vcs::Work::Working { .. });
                    if pane.commit() == Some(commit) && pane.device().is_none() {
                        // The head did not move: the worktree is read again every few seconds
                        // while there is one, and once more when it is gone.
                        let held_back = if has_worktree {
                            !self.read_worktrees.due(item)
                        } else {
                            !pane.has_uncommitted_part()
                        };
                        if held_back {
                            return Ok(());
                        }
                    }
                    let changes = if git.has_commits(&work)? {
                        Changes::Files(git.file_diffs(&work)?)
                    } else {
                        Changes::NoCommits
                    };
                    let uncommitted = git
                        .parseable_uncommitted_diff(&work)?
                        .map(|text| wf_vcs::diff::parse(&text));
                    if has_worktree {
                        self.read_worktrees.asked(item);
                    } else {
                        self.read_worktrees.forget(item);
                    }
                    pane.read(Ok(ItemWork {
                        commit,
                        changes,
                        uncommitted,
                    }));
                }
                ItemDiff::Device { device, diff } => {
                    self.asked_devices.asked(item);
                    match diff {
                        DeviceDiff::Work(work) => {
                            let changes = Changes::Files(wf_vcs::diff::parse(&work.committed));
                            let work = ItemWork {
                                commit: work.head,
                                changes,
                                uncommitted: None,
                            };
                            pane.read_from_device(device, work, now_ms);
                        }
                        DeviceDiff::Unchanged => pane.device_unchanged(device, now_ms),
                        DeviceDiff::Unreachable(reason) => {
                            pane.device_unreachable(device, reason);
                        }
                    }
                }
            }
            Ok(())
        };
        if let Err(error) = read() {
            pane.read(Err(format!("{error:#}")));
        }
    }

    /// Takes the snapshot, unless nothing it is taken from changed since the last one: the same
    /// events, permission requests, machine faults, devices heard from and wanted, or the minute. Then only the
    /// clock moves, and `None` says to keep the snapshot on screen.
    fn snapshot(&mut self, workspace: &Workspace, app: &App) -> anyhow::Result<Option<Snapshot>> {
        let now_ms = wf_store::now_ms();
        let taken = Taken {
            minute: now_ms / MINUTE_MS,
            events: self.events.len(),
            held: workspace.held_permissions()?,
            machine_faults: workspace.machine_faults()?,
            last_seen: workspace.last_seen()?,
            wanted: app.wanted(),
            detail_width: app.detail_width(),
        };
        if self.taken.as_ref() == Some(&taken) {
            return Ok(None);
        }
        let state = workspace.state();
        let graph = state.dependency_graph();
        let sources = Sources {
            state,
            graph: &graph,
            events: &self.events,
            event: &|id| workspace.event(&id).ok().flatten(),
            own: workspace.device_key(),
            now_ms,
            detail_width: taken.detail_width,
        };
        let snapshot = Snapshot::take(
            &sources,
            &taken.held,
            &taken.machine_faults,
            &taken.last_seen,
            taken.wanted.clone(),
        );
        self.taken = Some(taken);
        Ok(Some(snapshot))
    }

    /// What the detail on screen shows, read against what the snapshot on screen flags.
    fn in_view(&mut self, workspace: &Workspace, app: &App) -> Option<InView> {
        // The snapshot on screen is taken to show this detail from now on.
        if let Some(taken) = &mut self.taken {
            taken.wanted = app.wanted();
        }
        let detail = app.wanted().detail?;
        let state = workspace.state();
        let graph = state.dependency_graph();
        let sources = Sources {
            state,
            graph: &graph,
            events: &self.events,
            event: &|_| None,
            own: workspace.device_key(),
            now_ms: wf_store::now_ms(),
            detail_width: app.detail_width(),
        };
        InView::take(&sources, &app.snapshot.flagged, detail)
    }
}

const MINUTE_MS: u64 = 60_000;

/// What the last snapshot was taken from.
#[derive(PartialEq)]
struct Taken {
    /// The minute the snapshot was taken in: the ages and stats in it are worded in minutes at
    /// the finest, so they go stale when it turns.
    minute: u64,
    /// How many events the log had.
    events: usize,
    held: Vec<HeldPermission>,
    machine_faults: wf_daemon::MachineFaults,
    last_seen: crate::devices::LastSeen,
    wanted: Wanted,
    detail_width: u16,
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

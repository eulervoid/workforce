//! The state of the dashboard and what keys do to it. Pure: what a key asks of the workforce comes
//! back as an [`Effect`], which the loop carries out.

use std::collections::HashMap;

use jiff::tz::TimeZone;
use ratatui::crossterm::event::{Event as TerminalEvent, KeyCode, KeyEvent, KeyModifiers};
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;
use wf_core::id::{ItemId, SessionId};
use wf_harness::TranscriptEntry;

use super::snapshot::{Snapshot, Waiting, WaitingKey, Wanted};
use crate::transcript::Transcript;

/// From this width the detail sits right of the list, instead of in its place.
pub const WIDE: u16 = 120;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Now,
    Items,
    Agents,
    Log,
}

impl Tab {
    pub const ALL: [Tab; 4] = [Tab::Now, Tab::Items, Tab::Agents, Tab::Log];

    pub fn name(self) -> &'static str {
        match self {
            Tab::Now => "Now",
            Tab::Items => "Items",
            Tab::Agents => "Agents",
            Tab::Log => "Log",
        }
    }

    pub fn key(self) -> char {
        match self {
            Tab::Now => '1',
            Tab::Items => '2',
            Tab::Agents => '3',
            Tab::Log => '4',
        }
    }

    fn index(self) -> usize {
        match self {
            Tab::Now => 0,
            Tab::Items => 1,
            Tab::Agents => 2,
            Tab::Log => 3,
        }
    }

    fn next(self) -> Tab {
        Tab::ALL[(self.index() + 1) % Tab::ALL.len()]
    }

    fn previous(self) -> Tab {
        Tab::ALL[(self.index() + Tab::ALL.len() - 1) % Tab::ALL.len()]
    }
}

/// What a key asks of the workforce or the terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    Quit,
}

/// What a detail shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Detail {
    Item(ItemId),
    Session(SessionId),
}

impl Detail {
    fn wanted(self) -> Wanted {
        match self {
            Detail::Item(item) => Wanted::Item(item),
            Detail::Session(session) => Wanted::Session(session),
        }
    }

    /// Where a detail starts: a transcript at its end, which is what happens now.
    fn start(self) -> Scroll {
        match self {
            Detail::Item(_) => Scroll::FromTop(0),
            Detail::Session(_) => Scroll::FromEnd(0),
        }
    }
}

/// Which part of a detail is on screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scroll {
    /// This many lines below the top.
    FromTop(u16),
    /// This many lines above the end; at 0 it follows what is added.
    FromEnd(u16),
}

impl Scroll {
    fn down(self, lines: u16) -> Scroll {
        match self {
            Scroll::FromTop(offset) => Scroll::FromTop(offset.saturating_add(lines)),
            Scroll::FromEnd(offset) => Scroll::FromEnd(offset.saturating_sub(lines)),
        }
    }

    fn up(self, lines: u16) -> Scroll {
        match self {
            Scroll::FromTop(offset) => Scroll::FromTop(offset.saturating_sub(lines)),
            Scroll::FromEnd(offset) => Scroll::FromEnd(offset.saturating_add(lines)),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Opened {
    pub detail: Detail,
    pub scroll: Scroll,
}

/// A line of the Now tab that can be selected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Row {
    Waiting(WaitingKey),
    Session(SessionId),
    Flag(String),
}

/// A list whose selection holds what is selected, not where it is, so a refresh that adds or
/// removes rows keeps it on the same thing.
#[derive(Clone, Debug)]
pub struct Selection<Key> {
    selected: Option<Key>,
    /// Where the selection was, for when what it held is gone.
    index: usize,
}

impl<Key> Default for Selection<Key> {
    fn default() -> Self {
        Self {
            selected: None,
            index: 0,
        }
    }
}

impl<Key: Clone + PartialEq> Selection<Key> {
    pub fn get(&self) -> Option<&Key> {
        self.selected.as_ref()
    }

    /// Keeps the selection on its row of `rows`, or near where it was when the row is gone.
    fn update(&mut self, rows: &[Key]) {
        if let Some(index) = self
            .selected
            .as_ref()
            .and_then(|selected| rows.iter().position(|row| row == selected))
        {
            self.index = index;
            return;
        }
        self.index = self.index.min(rows.len().saturating_sub(1));
        self.selected = rows.get(self.index).cloned();
    }

    fn move_by(&mut self, rows: &[Key], step: isize) {
        if rows.is_empty() {
            return;
        }
        self.update(rows);
        let last = rows.len() - 1;
        self.index = self.index.saturating_add_signed(step).min(last);
        self.selected = rows.get(self.index).cloned();
    }

    fn first(&mut self, rows: &[Key]) {
        self.index = 0;
        self.selected = rows.first().cloned();
    }

    fn last(&mut self, rows: &[Key]) {
        self.index = rows.len().saturating_sub(1);
        self.selected = rows.last().cloned();
    }
}

/// A tab of a list and the details opened from it, the latest on top.
#[derive(Clone, Debug)]
pub struct ListTab<Key> {
    pub selection: Selection<Key>,
    pub stack: Vec<Opened>,
}

impl<Key> Default for ListTab<Key> {
    fn default() -> Self {
        Self {
            selection: Selection::default(),
            stack: Vec::new(),
        }
    }
}

/// How many lines of the event stream are kept.
const LOG_LINES: usize = 1000;

/// The event stream of `wf watch`.
#[derive(Clone, Debug)]
pub struct LogTab {
    /// One entry for each thing an event did, as `wf watch` prints it, the oldest first.
    pub lines: Vec<String>,
    pub scroll: Scroll,
    /// Only lines with this text are shown.
    pub filter: String,
}

/// A line the footer asks for.
#[derive(Clone, Debug)]
pub enum Prompt {
    /// Text the lines of the Log tab must have; it applies while it is typed.
    Filter(Input),
}

pub struct App {
    pub snapshot: Snapshot,
    /// The transcripts of the sessions this machine runs, as far as they are read.
    pub transcripts: HashMap<SessionId, Transcript>,
    pub tab: Tab,
    pub now: ListTab<Row>,
    pub log: LogTab,
    pub prompt: Option<Prompt>,
    pub width: u16,
    pub time_zone: TimeZone,
    /// Why the last refresh or action failed, until the next one works.
    pub error: Option<String>,
}

impl App {
    pub fn new(snapshot: Snapshot, time_zone: TimeZone) -> Self {
        let mut app = Self {
            snapshot: Snapshot::default(),
            transcripts: HashMap::new(),
            tab: Tab::Now,
            now: ListTab::default(),
            log: LogTab {
                lines: Vec::new(),
                scroll: Scroll::FromEnd(0),
                filter: String::new(),
            },
            prompt: None,
            width: 80,
            time_zone,
            error: None,
        };
        app.refresh(snapshot);
        app
    }

    /// Opens the dashboard on an item.
    pub fn open_item(&mut self, item: ItemId) {
        self.open(Detail::Item(item));
    }

    pub fn refresh(&mut self, snapshot: Snapshot) {
        self.snapshot = snapshot;
        self.error = None;
        let rows = self.now_rows();
        self.now.selection.update(&rows);
    }

    pub fn failed(&mut self, error: &anyhow::Error) {
        self.error = Some(format!("{error:#}"));
    }

    pub fn resize(&mut self, width: u16) {
        self.width = width;
    }

    pub fn is_wide(&self) -> bool {
        self.width >= WIDE
    }

    /// How many columns the list takes in a wide window.
    pub fn list_width(&self) -> u16 {
        (self.width * 2 / 5).min(64)
    }

    /// How many columns the text of a detail has: the window's, or what the list leaves of it,
    /// less the margin.
    pub fn detail_width(&self) -> u16 {
        let pane = if self.is_wide() {
            self.width.saturating_sub(self.list_width() + 1)
        } else {
            self.width
        };
        pane.saturating_sub(1)
    }

    /// Takes in what a session's agent wrote to its transcript since the last time.
    pub fn transcribed(&mut self, session: SessionId, entries: &[TranscriptEntry]) {
        let transcript = self.transcripts.entry(session).or_default();
        for entry in entries {
            transcript.push(entry);
        }
    }

    /// Takes in the lines of new events.
    pub fn logged(&mut self, lines: impl IntoIterator<Item = String>) {
        self.log.lines.extend(lines);
        let over = self.log.lines.len().saturating_sub(LOG_LINES);
        self.log.lines.drain(..over);
    }

    /// The lines of the Log tab that pass its filter.
    pub fn log_lines(&self) -> impl Iterator<Item = &String> {
        let filter = self.log.filter.to_lowercase();
        self.log.lines.iter().filter(move |line| {
            filter.is_empty()
                || anstream::adapter::strip_str(line)
                    .to_string()
                    .to_lowercase()
                    .contains(&filter)
        })
    }

    /// The rows of the Now tab, in the order they are shown.
    pub fn now_rows(&self) -> Vec<Row> {
        let snapshot = &self.snapshot;
        let waiting = snapshot
            .waiting
            .iter()
            .map(|entry| Row::Waiting(entry.key()));
        let working = snapshot
            .working
            .iter()
            .map(|session| Row::Session(session.id));
        let flagged = snapshot.flagged.iter().cloned().map(Row::Flag);
        waiting.chain(working).chain(flagged).collect()
    }

    /// The detail of a row of the Now tab, if it has one.
    pub fn detail_of(&self, row: &Row) -> Option<Detail> {
        match row {
            Row::Waiting(key) => {
                let entry = self
                    .snapshot
                    .waiting
                    .iter()
                    .find(|entry| &entry.key() == key)?;
                Some(match entry {
                    Waiting::Permission { session, .. } => Detail::Session(*session),
                    Waiting::Blocker { item, .. }
                    | Waiting::Review { item, .. }
                    | Waiting::Notice { item, .. } => Detail::Item(item.id),
                })
            }
            Row::Session(session) => Some(Detail::Session(*session)),
            Row::Flag(_) => None,
        }
    }

    /// The detail on screen: the one opened last, or in a wide window the selection's.
    pub fn in_view(&self) -> Option<Opened> {
        match self.tab {
            Tab::Now => self.now.stack.last().copied().or_else(|| {
                if !self.is_wide() {
                    return None;
                }
                let detail = self.detail_of(self.now.selection.get()?)?;
                Some(Opened {
                    detail,
                    scroll: detail.start(),
                })
            }),
            Tab::Items | Tab::Agents | Tab::Log => None,
        }
    }

    /// What the workspace is asked about for the detail on screen.
    pub fn wanted(&self) -> Option<Wanted> {
        self.in_view().map(|opened| opened.detail.wanted())
    }

    fn open(&mut self, detail: Detail) {
        self.tab = Tab::Now;
        self.now.stack.push(Opened {
            detail,
            scroll: detail.start(),
        });
    }

    pub fn key(&mut self, key: KeyEvent) -> Option<Effect> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Some(Effect::Quit);
        }
        if let Some(prompt) = self.prompt.take() {
            self.prompt = self.prompt_key(prompt, key);
            return None;
        }
        match key.code {
            KeyCode::Char('q') => return Some(Effect::Quit),
            KeyCode::Char('1') => self.tab = Tab::Now,
            KeyCode::Char('2') => self.tab = Tab::Items,
            KeyCode::Char('3') => self.tab = Tab::Agents,
            KeyCode::Char('4') => self.tab = Tab::Log,
            KeyCode::Tab => self.tab = self.tab.next(),
            KeyCode::BackTab => self.tab = self.tab.previous(),
            _ => match self.tab {
                Tab::Now => self.now_key(key),
                Tab::Log => self.log_key(key),
                Tab::Items | Tab::Agents => {}
            },
        }
        None
    }

    /// A key while the footer asks for a line; the prompt stays open unless the key closes it.
    fn prompt_key(&mut self, prompt: Prompt, key: KeyEvent) -> Option<Prompt> {
        match prompt {
            Prompt::Filter(mut input) => match key.code {
                KeyCode::Enter => None,
                KeyCode::Esc => {
                    self.log.filter.clear();
                    None
                }
                _ => {
                    input.handle_event(&TerminalEvent::Key(key));
                    self.log.filter = input.value().to_owned();
                    Some(Prompt::Filter(input))
                }
            },
        }
    }

    fn log_key(&mut self, key: KeyEvent) {
        let scroll = &mut self.log.scroll;
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => *scroll = scroll.down(1),
            KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.up(1),
            KeyCode::PageDown | KeyCode::Char(' ') => *scroll = scroll.down(20),
            KeyCode::PageUp => *scroll = scroll.up(20),
            KeyCode::Char('g') => *scroll = Scroll::FromTop(0),
            KeyCode::Char('G') => *scroll = Scroll::FromEnd(0),
            KeyCode::Char('/') => {
                let input = Input::new(self.log.filter.clone());
                self.prompt = Some(Prompt::Filter(input));
            }
            KeyCode::Esc => self.log.filter.clear(),
            _ => {}
        }
    }

    fn now_key(&mut self, key: KeyEvent) {
        if let Some(opened) = self.now.stack.last_mut() {
            match key.code {
                KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') => {
                    self.now.stack.pop();
                }
                KeyCode::Down | KeyCode::Char('j') => opened.scroll = opened.scroll.down(1),
                KeyCode::Up | KeyCode::Char('k') => opened.scroll = opened.scroll.up(1),
                KeyCode::PageDown | KeyCode::Char(' ') => opened.scroll = opened.scroll.down(20),
                KeyCode::PageUp => opened.scroll = opened.scroll.up(20),
                KeyCode::Char('g') => opened.scroll = Scroll::FromTop(0),
                KeyCode::Char('G') => opened.scroll = Scroll::FromEnd(0),
                _ => {}
            }
            return;
        }
        let rows = self.now_rows();
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.now.selection.move_by(&rows, 1),
            KeyCode::Up | KeyCode::Char('k') => self.now.selection.move_by(&rows, -1),
            KeyCode::Char('g') => self.now.selection.first(&rows),
            KeyCode::Char('G') => self.now.selection.last(&rows),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                if let Some(detail) = self.now.selection.get().and_then(|row| self.detail_of(row)) {
                    self.open(detail);
                }
            }
            _ => {}
        }
    }
}

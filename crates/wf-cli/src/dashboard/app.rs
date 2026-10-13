//! The state of the dashboard and what keys do to it. Pure: what a key asks of the workforce comes
//! back as an [`Effect`], which the loop carries out.

use std::collections::HashMap;

use jiff::tz::TimeZone;
use ratatui::crossterm::event::{Event as TerminalEvent, KeyCode, KeyEvent, KeyModifiers};
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;
use wf_core::graph::Readiness;
use wf_core::id::{CommentId, ItemId, SessionId};
use wf_core::identity::AgentId;
use wf_core::name::{AgentName, StateName};
use wf_core::policy::StateCategory;
use wf_harness::{Permission, TranscriptEntry};

use super::snapshot::{
    Detail, InView, ItemRow, SessionInfo, SessionStatus, Snapshot, Waiting, WaitingKey, Wanted,
};
use crate::cli::ItemGrouping;
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
    /// Answers the oldest permission request of a session.
    Permit {
        session: SessionId,
        answer: Permission,
    },
    /// A comment that resolves a blocker.
    Resolve {
        item: ItemId,
        blocker: CommentId,
        text: String,
    },
    /// Writes the comment that resolves a blocker in `$EDITOR`.
    ResolveInEditor {
        item: ItemId,
        blocker: CommentId,
    },
    Approve {
        item: ItemId,
    },
    /// Writes the change requests in `$EDITOR`, one per line, then requests the changes.
    RequestChanges {
        item: ItemId,
    },
    /// The work of an item through git, in its pager.
    ShowDiff {
        item: ItemId,
    },
    /// Asks which agent to start on an item.
    ChooseAgent {
        item: ItemId,
    },
    /// Asks which state to move an item to.
    ChooseState {
        item: ItemId,
    },
    StartSession {
        item: ItemId,
        agent: AgentName,
    },
    StopSession {
        session: SessionId,
    },
    Move {
        item: ItemId,
        state: StateName,
    },
    Comment {
        item: ItemId,
        text: String,
    },
    /// Edits an item in `$EDITOR`, as `wf item edit` does.
    Edit {
        item: ItemId,
    },
}

impl Detail {
    /// Where a detail starts: a transcript at its end, which is what happens now.
    fn start(&self) -> Scroll {
        match self {
            Detail::Item(_) | Detail::Agent(_) => Scroll::FromTop(0),
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

    /// What the keys of a scrolled text do to it; `None` for a key it does not take.
    fn key(self, key: KeyEvent) -> Option<Scroll> {
        Some(match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.down(1),
            KeyCode::Up | KeyCode::Char('k') => self.up(1),
            KeyCode::PageDown | KeyCode::Char(' ') => self.down(20),
            KeyCode::PageUp => self.up(20),
            KeyCode::Char('g') => Scroll::FromTop(0),
            KeyCode::Char('G') => Scroll::FromEnd(0),
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Opened {
    pub detail: Detail,
    pub scroll: Scroll,
}

/// A line of the Agents tab that can be selected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentRow {
    Agent(AgentId),
    Session(SessionId),
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

    /// What the keys of a list do to its selection; `false` for a key it does not take.
    fn key(&mut self, rows: &[Key], key: KeyEvent) -> bool {
        self.update(rows);
        let last = rows.len().saturating_sub(1);
        self.index = match key.code {
            KeyCode::Down | KeyCode::Char('j') => (self.index + 1).min(last),
            KeyCode::Up | KeyCode::Char('k') => self.index.saturating_sub(1),
            KeyCode::PageDown => (self.index + 20).min(last),
            KeyCode::PageUp => self.index.saturating_sub(20),
            KeyCode::Char('g') => 0,
            KeyCode::Char('G') => last,
            _ => return false,
        };
        self.selected = rows.get(self.index).cloned();
        true
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

/// Which items the Items tab lists, by their readiness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadinessFilter {
    All,
    Ready,
    Blocked,
    Stranded,
}

impl ReadinessFilter {
    fn next(self) -> Self {
        match self {
            ReadinessFilter::All => ReadinessFilter::Ready,
            ReadinessFilter::Ready => ReadinessFilter::Blocked,
            ReadinessFilter::Blocked => ReadinessFilter::Stranded,
            ReadinessFilter::Stranded => ReadinessFilter::All,
        }
    }

    fn accepts(self, readiness: Option<Readiness>) -> bool {
        match self {
            ReadinessFilter::All => true,
            ReadinessFilter::Ready => readiness == Some(Readiness::Ready),
            ReadinessFilter::Blocked => readiness == Some(Readiness::Blocked),
            ReadinessFilter::Stranded => readiness == Some(Readiness::Stranded),
        }
    }

    pub fn name(self) -> Option<&'static str> {
        match self {
            ReadinessFilter::All => None,
            ReadinessFilter::Ready => Some("ready"),
            ReadinessFilter::Blocked => Some("blocked"),
            ReadinessFilter::Stranded => Some("stranded"),
        }
    }
}

/// `wf item list --group-by`.
#[derive(Clone, Debug)]
pub struct ItemsTab {
    pub list: ListTab<ItemId>,
    pub grouping: ItemGrouping,
    /// Only items whose title has this text are listed.
    pub filter: String,
    pub readiness: ReadinessFilter,
}

/// The groupings `o` goes through, in order.
const GROUPINGS: [ItemGrouping; 4] = [
    ItemGrouping::State,
    ItemGrouping::Lane,
    ItemGrouping::Priority,
    ItemGrouping::Type,
];

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

/// One of a list of things to pick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Choice {
    /// The agent to start on an item.
    Agent {
        item: ItemId,
        agents: Vec<AgentName>,
        index: usize,
    },
    /// The state to move an item to.
    State {
        item: ItemId,
        states: Vec<(StateName, StateCategory)>,
        index: usize,
    },
}

impl Choice {
    pub fn len(&self) -> usize {
        match self {
            Choice::Agent { agents, .. } => agents.len(),
            Choice::State { states, .. } => states.len(),
        }
    }

    pub fn index(&self) -> usize {
        match self {
            Choice::Agent { index, .. } | Choice::State { index, .. } => *index,
        }
    }

    fn index_mut(&mut self) -> &mut usize {
        match self {
            Choice::Agent { index, .. } | Choice::State { index, .. } => index,
        }
    }

    /// What picking the selected option asks of the workforce.
    fn picked(self) -> Option<Effect> {
        match self {
            Choice::Agent {
                item,
                agents,
                index,
            } => Some(Effect::StartSession {
                item,
                agent: agents.into_iter().nth(index)?,
            }),
            Choice::State {
                item,
                states,
                index,
            } => Some(Effect::Move {
                item,
                state: states.into_iter().nth(index)?.0,
            }),
        }
    }
}

/// What the footer asks for.
#[derive(Clone, Debug)]
pub enum Prompt {
    /// Text the lines of the tab on screen must have; it applies while it is typed.
    Filter(Input),
    /// The answer to a blocker.
    Answer {
        item: ItemId,
        blocker: CommentId,
        input: Input,
    },
    /// A verdict on the work of an item: approve, request changes or look at the diff first.
    Verdict {
        item: ItemId,
    },
    Comment {
        item: ItemId,
        input: Input,
    },
    Choose(Choice),
    /// Whether to stop a session, which costs the agent what it has not written down.
    Stop {
        session: SessionId,
    },
    /// The keys, until any key is pressed.
    Keys,
}

/// What the footer says instead of the latest event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// What an action did, until the next key.
    Done(String),
    /// Why an action failed, until the next key.
    ActionFailed(String),
    /// Why the workforce could not be read, until it can again.
    RefreshFailed(String),
}

pub struct App {
    pub snapshot: Snapshot,
    /// The transcripts of the sessions this machine runs, as far as they are read.
    pub transcripts: HashMap<SessionId, Transcript>,
    pub tab: Tab,
    pub now: ListTab<Row>,
    pub items: ItemsTab,
    pub agents: ListTab<AgentRow>,
    pub log: LogTab,
    pub prompt: Option<Prompt>,
    /// Whether transcripts show everything, as `wf session watch --full` does.
    pub full_transcripts: bool,
    pub width: u16,
    pub time_zone: TimeZone,
    pub status: Option<Status>,
}

impl App {
    pub fn new(snapshot: Snapshot, time_zone: TimeZone) -> Self {
        let mut app = Self {
            snapshot: Snapshot::default(),
            transcripts: HashMap::new(),
            tab: Tab::Now,
            now: ListTab::default(),
            items: ItemsTab {
                list: ListTab::default(),
                grouping: ItemGrouping::State,
                filter: String::new(),
                readiness: ReadinessFilter::All,
            },
            agents: ListTab::default(),
            log: LogTab {
                lines: Vec::new(),
                scroll: Scroll::FromEnd(0),
                filter: String::new(),
            },
            prompt: None,
            full_transcripts: false,
            width: 80,
            time_zone,
            status: None,
        };
        app.refresh(snapshot);
        app
    }

    /// Opens the dashboard on an item.
    pub fn open_item(&mut self, item: ItemId) {
        self.tab = Tab::Now;
        self.open(Detail::Item(item));
    }

    pub fn refresh(&mut self, snapshot: Snapshot) {
        self.snapshot = snapshot;
        if matches!(self.status, Some(Status::RefreshFailed(_))) {
            self.status = None;
        }
        let rows = self.now_rows();
        self.now.selection.update(&rows);
        // The rows of these tabs are only known while they are on screen.
        match self.tab {
            Tab::Items => {
                let rows = self.item_rows();
                self.items.list.selection.update(&rows);
            }
            Tab::Agents => {
                let rows = self.agent_rows();
                self.agents.selection.update(&rows);
            }
            Tab::Now | Tab::Log => {}
        }
    }

    pub fn refresh_failed(&mut self, error: &anyhow::Error) {
        self.status = Some(Status::RefreshFailed(format!("{error:#}")));
    }

    pub fn action_failed(&mut self, error: &anyhow::Error) {
        self.status = Some(Status::ActionFailed(format!("{error:#}")));
    }

    /// Says what an action did.
    pub fn did(&mut self, what: String) {
        self.status = (!what.is_empty()).then_some(Status::Done(what));
    }

    /// Asks the principal to pick one of `choice`.
    pub fn choose(&mut self, choice: Choice) {
        self.prompt = Some(Prompt::Choose(choice));
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

    /// The groups of the Items tab with the items that pass its filters; empty groups are left
    /// out.
    pub fn item_groups(&self) -> Vec<(&str, Vec<&ItemRow>)> {
        let filter = self.items.filter.to_lowercase();
        self.snapshot
            .items
            .iter()
            .map(|group| {
                let items = group
                    .items
                    .iter()
                    .filter(|item| self.items.readiness.accepts(item.readiness))
                    .filter(|item| item.title.to_lowercase().contains(&filter))
                    .collect();
                (group.name.as_str(), items)
            })
            .filter(|(_, items): &(&str, Vec<&ItemRow>)| !items.is_empty())
            .collect()
    }

    fn item_rows(&self) -> Vec<ItemId> {
        self.item_groups()
            .into_iter()
            .flat_map(|(_, items)| items.into_iter().map(|item| item.id))
            .collect()
    }

    /// The rows of the Agents tab: each agent, then its sessions.
    pub fn agent_rows(&self) -> Vec<AgentRow> {
        self.snapshot
            .agents
            .iter()
            .flat_map(|agent| {
                std::iter::once(AgentRow::Agent(agent.id.clone())).chain(
                    agent
                        .sessions
                        .iter()
                        .map(|session| AgentRow::Session(session.id)),
                )
            })
            .collect()
    }

    /// The detail of a row of the Now tab, if it has one.
    pub fn detail_of(&self, row: &Row) -> Option<Detail> {
        match row {
            Row::Waiting(key) => Some(match self.waiting(key)? {
                Waiting::Permission { session, .. } => Detail::Session(*session),
                Waiting::Blocker { item, .. }
                | Waiting::Review { item, .. }
                | Waiting::Notice { item, .. } => Detail::Item(item.id),
            }),
            Row::Session(session) => Some(Detail::Session(*session)),
            Row::Flag(_) => None,
        }
    }

    fn waiting(&self, key: &WaitingKey) -> Option<&Waiting> {
        self.snapshot
            .waiting
            .iter()
            .find(|entry| &entry.key() == key)
    }

    /// The details opened on the tab on screen, the latest on top.
    pub fn stack(&self) -> Option<&Vec<Opened>> {
        match self.tab {
            Tab::Now => Some(&self.now.stack),
            Tab::Items => Some(&self.items.list.stack),
            Tab::Agents => Some(&self.agents.stack),
            Tab::Log => None,
        }
    }

    fn stack_mut(&mut self) -> Option<&mut Vec<Opened>> {
        match self.tab {
            Tab::Now => Some(&mut self.now.stack),
            Tab::Items => Some(&mut self.items.list.stack),
            Tab::Agents => Some(&mut self.agents.stack),
            Tab::Log => None,
        }
    }

    /// The detail of what is selected on the tab on screen.
    fn selected_detail(&self) -> Option<Detail> {
        match self.tab {
            Tab::Now => self.detail_of(self.now.selection.get()?),
            Tab::Items => self.items.list.selection.get().copied().map(Detail::Item),
            Tab::Agents => Some(match self.agents.selection.get()? {
                AgentRow::Agent(agent) => Detail::Agent(agent.clone()),
                AgentRow::Session(session) => Detail::Session(*session),
            }),
            Tab::Log => None,
        }
    }

    /// The detail the keys act on: the one opened last, else the selection's.
    fn focus(&self) -> Option<Detail> {
        match self.stack().and_then(|stack| stack.last()) {
            Some(opened) => Some(opened.detail.clone()),
            None => self.selected_detail(),
        }
    }

    /// The detail on screen: the one opened last, or in a wide window the selection's.
    pub fn in_view(&self) -> Option<Opened> {
        if let Some(opened) = self.stack().and_then(|stack| stack.last()) {
            return Some(opened.clone());
        }
        if !self.is_wide() {
            return None;
        }
        let detail = self.selected_detail()?;
        Some(Opened {
            scroll: detail.start(),
            detail,
        })
    }

    /// What the workspace is asked about, for what is on screen.
    pub fn wanted(&self) -> Wanted {
        Wanted {
            detail: self.in_view().map(|opened| opened.detail),
            items: (self.tab == Tab::Items).then_some(self.items.grouping),
            agents: self.tab == Tab::Agents,
        }
    }

    /// What waits that the keys on screen act on: the selected entry, the permission request of
    /// a selected or open session, or what waits about the selected or open item.
    pub fn subjects(&self) -> Vec<&Waiting> {
        let opened = self.stack().and_then(|stack| stack.last());
        if opened.is_none()
            && self.tab == Tab::Now
            && let Some(Row::Waiting(key)) = self.now.selection.get()
        {
            return self.waiting(key).into_iter().collect();
        }
        let waiting = self.snapshot.waiting.iter();
        match self.focus() {
            Some(Detail::Session(id)) => waiting
                .filter(
                    |entry| matches!(entry, Waiting::Permission { session, .. } if *session == id),
                )
                .collect(),
            Some(Detail::Item(id)) => waiting
                .filter(|entry| match entry {
                    Waiting::Blocker { item, .. } | Waiting::Review { item, .. } => item.id == id,
                    Waiting::Permission { .. } | Waiting::Notice { .. } => false,
                })
                .collect(),
            Some(Detail::Agent(_)) | None => Vec::new(),
        }
    }

    /// The item the item keys act on: of the open or selected detail.
    pub fn item_subject(&self) -> Option<ItemId> {
        match self.focus()? {
            Detail::Item(item) => Some(item),
            Detail::Session(session) => self.session_info(session).map(|info| info.item.id),
            Detail::Agent(_) => None,
        }
    }

    /// The session `x` stops: the one open, or selected, while it is live.
    pub fn session_subject(&self) -> Option<SessionId> {
        match self.focus()? {
            Detail::Session(session) => self
                .session_info(session)
                .filter(|info| match info.status {
                    SessionStatus::Live | SessionStatus::Parked => true,
                    SessionStatus::Ended { .. } => false,
                })
                .map(|info| info.id),
            Detail::Item(_) | Detail::Agent(_) => None,
        }
    }

    /// What the snapshot knows of a session: as one that works, one in view, or one of an agent.
    fn session_info(&self, session: SessionId) -> Option<&SessionInfo> {
        let in_view = match &self.snapshot.in_view {
            Some(InView::Session(info)) => Some(info),
            Some(InView::Item { .. } | InView::Agent { .. }) | None => None,
        };
        self.snapshot
            .working
            .iter()
            .chain(in_view)
            .chain(
                self.snapshot
                    .agents
                    .iter()
                    .flat_map(|agent| agent.sessions.iter()),
            )
            .find(|info| info.id == session)
    }

    /// The item whose work `D` shows: of a review waiting, or of the item keys.
    fn item_with_work(&self) -> Option<ItemId> {
        let review = self.subjects().into_iter().find_map(|entry| match entry {
            Waiting::Review { item, .. } => Some(item.id),
            Waiting::Permission { .. } | Waiting::Blocker { .. } | Waiting::Notice { .. } => None,
        });
        review.or_else(|| self.item_subject())
    }

    fn open(&mut self, detail: Detail) {
        let Some(stack) = self.stack_mut() else {
            return;
        };
        stack.push(Opened {
            scroll: detail.start(),
            detail,
        });
    }

    pub fn key(&mut self, key: KeyEvent) -> Option<Effect> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Some(Effect::Quit);
        }
        if matches!(self.status, Some(Status::Done(_) | Status::ActionFailed(_))) {
            self.status = None;
        }
        if let Some(prompt) = self.prompt.take() {
            let (prompt, effect) = self.prompt_key(prompt, key);
            self.prompt = prompt;
            return effect;
        }
        if let Some(effect) = self.act(key) {
            return Some(effect);
        }
        if self.prompt.is_some() {
            return None;
        }
        match key.code {
            KeyCode::Char('q') => return Some(Effect::Quit),
            KeyCode::Char('?') => self.prompt = Some(Prompt::Keys),
            KeyCode::Char('1') => self.tab = Tab::Now,
            KeyCode::Char('2') => self.tab = Tab::Items,
            KeyCode::Char('3') => self.tab = Tab::Agents,
            KeyCode::Char('4') => self.tab = Tab::Log,
            KeyCode::Tab => self.tab = self.tab.next(),
            KeyCode::BackTab => self.tab = self.tab.previous(),
            _ => {
                if !self.detail_key(key) {
                    match self.tab {
                        Tab::Now => self.now_key(key),
                        Tab::Items => self.items_key(key),
                        Tab::Log => self.log_key(key),
                        Tab::Agents => self.agents_key(key),
                    }
                }
            }
        }
        None
    }

    /// The keys that act on what waits, on items and on sessions, for whatever is on screen.
    fn act(&mut self, key: KeyEvent) -> Option<Effect> {
        let subjects = self.subjects();
        let permission = subjects.iter().find_map(|entry| match entry {
            Waiting::Permission { session, .. } => Some(*session),
            Waiting::Blocker { .. } | Waiting::Review { .. } | Waiting::Notice { .. } => None,
        });
        let blocker = subjects.iter().find_map(|entry| match entry {
            Waiting::Blocker { item, comment, .. } => Some((item.id, *comment)),
            Waiting::Permission { .. } | Waiting::Review { .. } | Waiting::Notice { .. } => None,
        });
        let review = subjects.iter().find_map(|entry| match entry {
            Waiting::Review { item, .. } => Some(item.id),
            Waiting::Permission { .. } | Waiting::Blocker { .. } | Waiting::Notice { .. } => None,
        });
        match key.code {
            KeyCode::Char('a') => permission.map(|session| Effect::Permit {
                session,
                answer: Permission::Allow,
            }),
            KeyCode::Char('d') => permission.map(|session| Effect::Permit {
                session,
                answer: Permission::Deny,
            }),
            KeyCode::Char('r') => {
                let (item, blocker) = blocker?;
                self.prompt = Some(Prompt::Answer {
                    item,
                    blocker,
                    input: Input::default(),
                });
                None
            }
            KeyCode::Char('R') => {
                let (item, blocker) = blocker?;
                Some(Effect::ResolveInEditor { item, blocker })
            }
            KeyCode::Char('v') => {
                self.prompt = Some(Prompt::Verdict { item: review? });
                None
            }
            KeyCode::Char('D') => self.item_with_work().map(|item| Effect::ShowDiff { item }),
            KeyCode::Char('s') => self.item_subject().map(|item| Effect::ChooseAgent { item }),
            KeyCode::Char('m') => self.item_subject().map(|item| Effect::ChooseState { item }),
            KeyCode::Char('e') => self.item_subject().map(|item| Effect::Edit { item }),
            KeyCode::Char('c') => {
                self.prompt = Some(Prompt::Comment {
                    item: self.item_subject()?,
                    input: Input::default(),
                });
                None
            }
            KeyCode::Char('x') => {
                self.prompt = Some(Prompt::Stop {
                    session: self.session_subject()?,
                });
                None
            }
            _ => None,
        }
    }

    /// A key while the footer asks for something: the prompt it leaves open, and what it asks of
    /// the workforce.
    fn prompt_key(&mut self, prompt: Prompt, key: KeyEvent) -> (Option<Prompt>, Option<Effect>) {
        match prompt {
            Prompt::Filter(mut input) => {
                let filter = match self.tab {
                    Tab::Items => &mut self.items.filter,
                    Tab::Now | Tab::Agents | Tab::Log => &mut self.log.filter,
                };
                match key.code {
                    KeyCode::Enter => (None, None),
                    KeyCode::Esc => {
                        filter.clear();
                        (None, None)
                    }
                    _ => {
                        input.handle_event(&TerminalEvent::Key(key));
                        *filter = input.value().to_owned();
                        (Some(Prompt::Filter(input)), None)
                    }
                }
            }
            Prompt::Answer {
                item,
                blocker,
                mut input,
            } => match key.code {
                KeyCode::Esc => (None, None),
                KeyCode::Enter if input.value().trim().is_empty() => (None, None),
                KeyCode::Enter => {
                    let text = input.value().trim().to_owned();
                    (
                        None,
                        Some(Effect::Resolve {
                            item,
                            blocker,
                            text,
                        }),
                    )
                }
                _ => {
                    input.handle_event(&TerminalEvent::Key(key));
                    (
                        Some(Prompt::Answer {
                            item,
                            blocker,
                            input,
                        }),
                        None,
                    )
                }
            },
            Prompt::Comment { item, mut input } => match key.code {
                KeyCode::Esc => (None, None),
                KeyCode::Enter if input.value().trim().is_empty() => (None, None),
                KeyCode::Enter => {
                    let text = input.value().trim().to_owned();
                    (None, Some(Effect::Comment { item, text }))
                }
                _ => {
                    input.handle_event(&TerminalEvent::Key(key));
                    (Some(Prompt::Comment { item, input }), None)
                }
            },
            Prompt::Verdict { item } => match key.code {
                KeyCode::Char('a') => (None, Some(Effect::Approve { item })),
                KeyCode::Char('r') => (None, Some(Effect::RequestChanges { item })),
                // The diff is looked at before deciding, so the question stays.
                KeyCode::Char('D') => (
                    Some(Prompt::Verdict { item }),
                    Some(Effect::ShowDiff { item }),
                ),
                KeyCode::Esc => (None, None),
                _ => (Some(Prompt::Verdict { item }), None),
            },
            Prompt::Choose(mut choice) => {
                let last = choice.len().saturating_sub(1);
                let index = choice.index_mut();
                match key.code {
                    KeyCode::Down | KeyCode::Char('j') => *index = (*index + 1).min(last),
                    KeyCode::Up | KeyCode::Char('k') => *index = index.saturating_sub(1),
                    KeyCode::Enter => return (None, choice.picked()),
                    KeyCode::Esc => return (None, None),
                    _ => {}
                }
                (Some(Prompt::Choose(choice)), None)
            }
            Prompt::Stop { session } => match key.code {
                KeyCode::Char('y') => (None, Some(Effect::StopSession { session })),
                KeyCode::Char('n') | KeyCode::Esc => (None, None),
                _ => (Some(Prompt::Stop { session }), None),
            },
            Prompt::Keys => (None, None),
        }
    }

    /// The keys of the open detail; `false` when no detail is open or it does not take the key.
    fn detail_key(&mut self, key: KeyEvent) -> bool {
        let item_session = match &self.snapshot.in_view {
            Some(InView::Item { session, .. }) => *session,
            Some(InView::Session(_) | InView::Agent { .. }) | None => None,
        };
        let item_of_session = self.item_subject();
        let Some(stack) = self.stack_mut() else {
            return false;
        };
        let Some(opened) = stack.last_mut() else {
            return false;
        };
        if let Some(scroll) = opened.scroll.key(key) {
            opened.scroll = scroll;
            return true;
        }
        match (key.code, &opened.detail) {
            (KeyCode::Esc | KeyCode::Left | KeyCode::Char('h'), _) => {
                stack.pop();
            }
            (KeyCode::Enter | KeyCode::Right | KeyCode::Char('l'), Detail::Item(_)) => {
                let Some(session) = item_session else {
                    return true;
                };
                self.open(Detail::Session(session));
            }
            (KeyCode::Char('i'), Detail::Session(_)) => {
                let Some(item) = item_of_session else {
                    return true;
                };
                self.open(Detail::Item(item));
            }
            (KeyCode::Char('f'), Detail::Session(_)) => {
                self.full_transcripts = !self.full_transcripts;
            }
            _ => return false,
        }
        true
    }

    fn now_key(&mut self, key: KeyEvent) {
        let rows = self.now_rows();
        if self.now.selection.key(&rows, key) {
            return;
        }
        if matches!(
            key.code,
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l')
        ) && let Some(detail) = self.selected_detail()
        {
            self.open(detail);
        }
    }

    fn agents_key(&mut self, key: KeyEvent) {
        let rows = self.agent_rows();
        if self.agents.selection.key(&rows, key) {
            return;
        }
        if matches!(
            key.code,
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l')
        ) && let Some(detail) = self.selected_detail()
        {
            self.open(detail);
        }
    }

    fn items_key(&mut self, key: KeyEvent) {
        let rows = self.item_rows();
        if self.items.list.selection.key(&rows, key) {
            return;
        }
        match key.code {
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                if let Some(detail) = self.selected_detail() {
                    self.open(detail);
                }
            }
            KeyCode::Char('o') => {
                let index = GROUPINGS
                    .iter()
                    .position(|grouping| *grouping == self.items.grouping)
                    .map_or(0, |index| (index + 1) % GROUPINGS.len());
                self.items.grouping = GROUPINGS[index];
            }
            KeyCode::Char('F') => self.items.readiness = self.items.readiness.next(),
            KeyCode::Char('/') => {
                self.prompt = Some(Prompt::Filter(Input::new(self.items.filter.clone())));
            }
            KeyCode::Esc => self.items.filter.clear(),
            _ => {}
        }
    }

    fn log_key(&mut self, key: KeyEvent) {
        if let Some(scroll) = self.log.scroll.key(key) {
            self.log.scroll = scroll;
            return;
        }
        match key.code {
            KeyCode::Char('/') => {
                self.prompt = Some(Prompt::Filter(Input::new(self.log.filter.clone())));
            }
            KeyCode::Esc => self.log.filter.clear(),
            _ => {}
        }
    }
}

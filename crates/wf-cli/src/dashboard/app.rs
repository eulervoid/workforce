//! The state of the dashboard and what keys do to it. Pure: what a key asks of the workforce comes
//! back as an [`Effect`], which the loop carries out.

use std::cell::RefCell;
use std::collections::HashMap;

use jiff::tz::TimeZone;
use ratatui::crossterm::event::{Event as TerminalEvent, KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Position, Rect};
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;
use wf_core::graph::Readiness;
use wf_core::id::{CommentId, ItemId, SessionId};
use wf_core::identity::AgentId;
use wf_core::item::ItemState;
use wf_core::name::AgentName;
use wf_harness::{Permission, TranscriptEntry};

use super::snapshot::{
    Detail, InView, ItemRow, MovedBack, SessionInfo, SessionStatus, Snapshot, Waiting, WaitingKey,
    Wanted,
};
use super::transcript_lines::TranscriptLines;
use crate::cli::ItemGrouping;
use crate::transcript::{Transcript, Verbosity};

/// From this width the detail sits right of the list, instead of in its place.
pub const WIDE: u16 = 120;

/// How many columns a pane folded away keeps at its edge, in a wide window.
pub const STRIP: u16 = 2;

/// How many lines a text moves for a notch of the mouse wheel.
const WHEEL_LINES: usize = 3;

/// Which way the mouse wheel turned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wheel {
    Up,
    Down,
}

impl Wheel {
    fn scroll(self, scroll: Scroll) -> Scroll {
        match self {
            Wheel::Up => scroll.up(WHEEL_LINES),
            Wheel::Down => scroll.down(WHEEL_LINES),
        }
    }
}

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
    /// Writes the comment that resolves a blocker in `$EDITOR`, starting from `text`.
    ResolveInEditor {
        item: ItemId,
        blocker: CommentId,
        text: String,
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
    /// Asks who starts work on an item: an agent, or the dispatcher for a draft.
    ChooseStart {
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
        state: ItemState,
    },
    Comment {
        item: ItemId,
        text: String,
    },
    /// Writes a comment in `$EDITOR`, starting from `text`, then posts it.
    CommentInEditor {
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
            Detail::Item(_) | Detail::Agent(_) | Detail::Flag(_) => Scroll::At(0),
            Detail::Session(_) => Scroll::END,
        }
    }
}

/// Which part of a text is on screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scroll {
    /// From this line down; it stays there as the text grows.
    At(usize),
    /// The end, following what is added to it. `top` is the line the end was last drawn from,
    /// where scrolling up starts.
    End { top: usize },
}

impl Scroll {
    /// The end of a text not drawn yet.
    pub const END: Scroll = Scroll::End { top: usize::MAX };

    fn down(self, lines: usize) -> Scroll {
        match self {
            Scroll::At(top) => Scroll::At(top.saturating_add(lines)),
            Scroll::End { .. } => self,
        }
    }

    fn up(self, lines: usize) -> Scroll {
        match self {
            Scroll::At(top) | Scroll::End { top } => Scroll::At(top.saturating_sub(lines)),
        }
    }

    /// The scroll kept to a text whose top can go no further down than `last_top`. Reaching the
    /// end follows it again.
    pub fn within(self, last_top: usize) -> Scroll {
        match self {
            Scroll::At(top) if top < last_top => self,
            Scroll::At(_) | Scroll::End { .. } => Scroll::End { top: last_top },
        }
    }

    /// The line drawn at the top.
    pub fn top(self) -> usize {
        match self {
            Scroll::At(top) | Scroll::End { top } => top,
        }
    }

    /// What the keys of a scrolled text do to it, a page being `page` lines; `None` for a key it
    /// does not take.
    fn key(self, key: KeyEvent, page: usize) -> Option<Scroll> {
        Some(match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.down(1),
            KeyCode::Up | KeyCode::Char('k') => self.up(1),
            KeyCode::PageDown | KeyCode::Char(' ') => self.down(page),
            KeyCode::PageUp => self.up(page),
            KeyCode::Char('g') => Scroll::At(0),
            KeyCode::Char('G') => Scroll::END,
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
    Flag(crate::view::Flag),
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

    /// What the keys of a list do to its selection, a page being `page` rows; `false` for a key
    /// it does not take.
    fn key(&mut self, rows: &[Key], key: KeyEvent, page: usize) -> bool {
        self.update(rows);
        let last = rows.len().saturating_sub(1);
        self.index = match key.code {
            KeyCode::Down | KeyCode::Char('j') => (self.index + 1).min(last),
            KeyCode::Up | KeyCode::Char('k') => self.index.saturating_sub(1),
            KeyCode::PageDown => (self.index + page).min(last),
            KeyCode::PageUp => self.index.saturating_sub(page),
            KeyCode::Char('g') => 0,
            KeyCode::Char('G') => last,
            _ => return false,
        };
        self.selected = rows.get(self.index).cloned();
        true
    }

    /// Selects the row at `index` of `rows`; `None` when there is none.
    fn pick(&mut self, rows: &[Key], index: usize) -> Option<Picked> {
        let row = rows.get(index)?;
        let picked = if self.selected.as_ref() == Some(row) {
            Picked::Again
        } else {
            Picked::Moved
        };
        self.selected = Some(row.clone());
        self.index = index;
        Some(picked)
    }
}

/// One of the two panes of a tab: its list, and the detail opened from it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pane {
    List,
    Detail,
}

/// Whether a wide window shows both panes side by side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Split {
    Both,
    /// Only the pane with the keys, across the window as in a narrow one; the other is folded
    /// away to a strip at its edge.
    One,
}

/// What a tab of a list shows besides its selection.
#[derive(Clone, Debug)]
pub struct Panes {
    /// The details opened from the list, the latest on top.
    pub stack: Vec<Opened>,
    /// In a wide window, which of the list and the detail opened beside it has the keys.
    pub focus: Pane,
    pub list_top: ListTop,
}

/// The first line of a list on screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListTop {
    /// Kept while the selection moves within the list, moved only as far as keeps it on screen.
    FollowsSelection(usize),
    /// Where the wheel scrolled the list, wherever the selection is.
    Scrolled(usize),
}

impl ListTop {
    fn line(self) -> usize {
        match self {
            ListTop::FollowsSelection(line) | ListTop::Scrolled(line) => line,
        }
    }

    /// The same way of keeping the list, from `line`.
    fn at(self, line: usize) -> Self {
        match self {
            ListTop::FollowsSelection(_) => ListTop::FollowsSelection(line),
            ListTop::Scrolled(_) => ListTop::Scrolled(line),
        }
    }

    /// The list from where it is on screen, kept to the selection again.
    fn follow_selection(self) -> Self {
        ListTop::FollowsSelection(self.line())
    }

    fn scroll(self, wheel: Wheel) -> Self {
        let line = self.line();
        ListTop::Scrolled(match wheel {
            Wheel::Up => line.saturating_sub(WHEEL_LINES),
            Wheel::Down => line + WHEEL_LINES,
        })
    }
}

/// A tab of a list and the details opened from it.
#[derive(Clone, Debug)]
pub struct ListTab<Key> {
    pub selection: Selection<Key>,
    pub panes: Panes,
}

impl<Key: Clone + PartialEq> ListTab<Key> {
    /// Keeps the selection on its row of `rows`; when that moved it to another row, the list
    /// follows it on screen again.
    fn update(&mut self, rows: &[Key]) {
        let before = self.selection.get().cloned();
        self.selection.update(rows);
        if self.selection.get() != before.as_ref() {
            self.panes.list_top = self.panes.list_top.follow_selection();
        }
    }
}

impl<Key> Default for ListTab<Key> {
    fn default() -> Self {
        Self {
            selection: Selection::default(),
            panes: Panes {
                stack: Vec::new(),
                focus: Pane::Detail,
                list_top: ListTop::FollowsSelection(0),
            },
        }
    }
}

/// What drawing showed: where the text with the keys and the list ended up, and what a click can
/// land on.
#[derive(Clone, Debug, Default)]
pub struct Drawn {
    pub text: Option<Scroll>,
    pub list_top: Option<usize>,
    pub clickable: Clickable,
    /// What is under the pointer.
    pub hovered: Option<Hovered>,
}

/// What a click can land on that shows the pointer is over it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Hovered {
    /// A link or a button: underlined, with its tip.
    Spot(Spot),
    /// The label of a tab: stronger than the other inactive tabs.
    Tab(Tab),
}

/// Where drawing put what a click can land on.
#[derive(Clone, Debug, Default)]
pub struct Clickable {
    /// The label of each tab in the header.
    pub tabs: Vec<(Tab, Rect)>,
    pub list: Option<ClickableList>,
    /// The detail beside the list, in a wide window.
    pub detail: Option<Rect>,
    /// The strip of the pane folded away, in a wide window that shows one.
    pub strip: Option<Rect>,
    /// The links in the text, and the buttons.
    pub spots: Vec<Spot>,
}

impl Clickable {
    /// The link or button at `position`.
    pub fn spot(&self, position: Position) -> Option<&Spot> {
        self.spots.iter().find(|spot| spot.area.contains(position))
    }

    /// The link, button or tab at `position`.
    pub fn hovered(&self, position: Position) -> Option<Hovered> {
        if let Some(spot) = self.spot(position) {
            return Some(Hovered::Spot(spot.clone()));
        }
        self.tabs
            .iter()
            .find(|(_, label)| label.contains(position))
            .map(|(tab, _)| Hovered::Tab(*tab))
    }
}

/// A link or a button, where drawing put it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spot {
    pub target: Target,
    pub area: Rect,
}

/// What a click on a link or a button does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// Opens a detail; `name` is how it was written: `#k3f9ab`, `@ada/grace`.
    Open { detail: Detail, name: String },
    /// Shows the work of an item, as `D` does, from one of its commits.
    Diff { item: ItemId, commit: String },
    /// A key hint of the footer: does what its key does.
    Key {
        code: KeyCode,
        key: &'static str,
        what: &'static str,
    },
    /// A part of the breadcrumb: goes back to the first `depth` details opened.
    Back { depth: usize, name: String },
    /// Allows a permission request on a second click, so that a click that slipped allows
    /// nothing.
    Allow(Request),
    /// Denies a permission request at once.
    Deny(Request),
}

/// A permission request, as a click answers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request {
    pub session: SessionId,
    pub since_ms: u64,
}

impl Waiting {
    /// The permission request this is, if it is one.
    pub fn request(&self) -> Option<Request> {
        match self {
            Waiting::Permission {
                session, since_ms, ..
            } => Some(Request {
                session: *session,
                since_ms: *since_ms,
            }),
            Waiting::Blocker { .. } | Waiting::Review { .. } | Waiting::Notice { .. } => None,
        }
    }
}

/// How long a first click on allow waits for the second that confirms it.
pub const CONFIRM_LASTS_MS: u64 = 3_000;

/// A first click on allow, at `at_ms`, until a second confirms it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnconfirmedAllow {
    pub request: Request,
    pub at_ms: u64,
}

impl Target {
    /// What a click does, for the latest-event line while the pointer is over it.
    pub fn tip(&self) -> String {
        match self {
            Target::Open {
                detail: Detail::Session(_),
                name,
            } => format!("open the transcript of {name}"),
            Target::Open {
                detail: Detail::Item(_) | Detail::Agent(_) | Detail::Flag(_),
                name,
            } => format!("open {name}"),
            Target::Diff { commit, .. } => {
                let short: String = commit.chars().take(8).collect();
                format!("open the diff of {short}")
            }
            Target::Key { key, what, .. } => format!("{what} ({key})"),
            Target::Back { name, .. } => format!("back to {name}"),
            Target::Allow(_) => "allow the request: click twice".to_owned(),
            Target::Deny(_) => "deny the request".to_owned(),
        }
    }
}

/// A list as it is on screen.
#[derive(Clone, Debug)]
pub struct ClickableList {
    pub area: Rect,
    /// For each line on screen, from the top, the entry it shows by its place among the entries
    /// of the list; `None` for a line that is no entry, such as a heading.
    pub entries: Vec<Option<usize>>,
}

/// What a click on an entry of a list did to its selection.
enum Picked {
    /// The entry was selected already.
    Again,
    Moved,
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

/// The event stream, as `wf log --follow` prints it.
#[derive(Clone, Debug)]
pub struct LogTab {
    /// One entry for each thing an event did, as `wf log --follow` prints it, the oldest first.
    pub lines: Vec<String>,
    pub scroll: Scroll,
    /// Only lines with this text are shown.
    pub filter: String,
    /// The details opened from links in the log, the latest on top, in its place.
    pub stack: Vec<Opened>,
}

/// Who starts work on an item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Starter {
    /// Release a draft: open it, and the dispatcher starts whoever is free.
    Dispatcher,
    Agent(AgentName),
}

/// One of a list of things to pick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Choice {
    /// Who starts work on an item.
    Start {
        item: ItemId,
        starters: Vec<Starter>,
        index: usize,
    },
    /// The state to move an item to.
    State {
        item: ItemId,
        states: Vec<ItemState>,
        index: usize,
    },
}

impl Choice {
    pub fn len(&self) -> usize {
        match self {
            Choice::Start { starters, .. } => starters.len(),
            Choice::State { states, .. } => states.len(),
        }
    }

    pub fn index(&self) -> usize {
        match self {
            Choice::Start { index, .. } | Choice::State { index, .. } => *index,
        }
    }

    fn index_mut(&mut self) -> &mut usize {
        match self {
            Choice::Start { index, .. } | Choice::State { index, .. } => index,
        }
    }

    /// What picking the selected option asks of the workforce.
    fn picked(self) -> Option<Effect> {
        match self {
            Choice::Start {
                item,
                starters,
                index,
            } => Some(match starters.into_iter().nth(index)? {
                Starter::Dispatcher => Effect::Move {
                    item,
                    state: ItemState::Open,
                },
                Starter::Agent(agent) => Effect::StartSession { item, agent },
            }),
            Choice::State {
                item,
                states,
                index,
            } => Some(Effect::Move {
                item,
                state: states.into_iter().nth(index)?,
            }),
        }
    }
}

/// What the footer asks for.
#[derive(Clone, Debug)]
pub enum Prompt {
    /// Text the lines of a list must have; it applies while it is typed.
    Filter {
        list: Filtered,
        input: Input,
    },
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

/// A list that can be filtered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filtered {
    Items,
    Log,
}

/// How long the footer says what an action did or why it failed, unless a key is pressed first.
pub const STATUS_LASTS_MS: u64 = 5_000;

/// What the footer says instead of the latest event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// What an action did, said at `at_ms`, until the next key or [`STATUS_LASTS_MS`] later.
    Done { what: String, at_ms: u64 },
    /// Why an action failed, said at `at_ms`, until the next key or [`STATUS_LASTS_MS`] later.
    ActionFailed { error: String, at_ms: u64 },
    /// Why the workforce could not be read, until it can again.
    RefreshFailed(String),
}

pub struct App {
    pub snapshot: Snapshot,
    /// The transcripts of the sessions this machine runs, as far as they are read.
    pub transcripts: HashMap<SessionId, Transcript>,
    /// The lines of the transcripts as last drawn, kept so that drawing lays out only what was
    /// added since.
    pub transcript_lines: RefCell<HashMap<SessionId, TranscriptLines>>,
    pub tab: Tab,
    pub now: ListTab<Row>,
    pub items: ItemsTab,
    pub agents: ListTab<AgentRow>,
    pub log: LogTab,
    pub prompt: Option<Prompt>,
    /// How much of a transcript is shown: everything, as `wf session watch --full` shows it, or
    /// cut short.
    pub verbosity: Verbosity,
    /// For every tab with a list, alike.
    pub split: Split,
    pub width: u16,
    pub height: u16,
    pub time_zone: TimeZone,
    pub status: Option<Status>,
    /// The moves back to `open` the daemon made since the dashboard started, the latest last, the
    /// footer warns of until a key or until their item is opened.
    pub moved_back: Vec<MovedBack>,
    /// What a click can land on, as last drawn.
    pub clickable: Clickable,
    /// Where the pointer was last reported.
    pub pointer: Option<Position>,
    /// The link, button or tab under the pointer, as last drawn.
    pub hovered: Option<Hovered>,
    pub unconfirmed_allow: Option<UnconfirmedAllow>,
    /// Whether the terminal pane the dashboard runs in is focused, as far as the terminal reports
    /// it; a terminal that does not report it counts as focused.
    pub terminal_focused: bool,
}

impl App {
    pub fn new(snapshot: Snapshot, time_zone: TimeZone) -> Self {
        let mut app = Self {
            snapshot: Snapshot::default(),
            transcripts: HashMap::new(),
            transcript_lines: RefCell::default(),
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
                scroll: Scroll::END,
                filter: String::new(),
                stack: Vec::new(),
            },
            prompt: None,
            verbosity: Verbosity::Abridged,
            split: Split::Both,
            width: 80,
            height: 24,
            time_zone,
            status: None,
            moved_back: Vec::new(),
            clickable: Clickable::default(),
            pointer: None,
            hovered: None,
            unconfirmed_allow: None,
            terminal_focused: true,
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
        match self.status {
            Some(Status::RefreshFailed(_)) => self.status = None,
            Some(Status::Done { at_ms, .. } | Status::ActionFailed { at_ms, .. })
                if self.snapshot.now_ms >= at_ms + STATUS_LASTS_MS =>
            {
                self.status = None;
            }
            Some(Status::Done { .. } | Status::ActionFailed { .. }) | None => {}
        }
        // A first click on allow lapses after its time, or with its request.
        if let Some(unconfirmed) = self.unconfirmed_allow
            && !self.confirms(unconfirmed, unconfirmed.request)
        {
            self.unconfirmed_allow = None;
        }
        let rows = self.now_rows();
        self.now.update(&rows);
        // The rows of these tabs are only known while they are on screen.
        match self.tab {
            Tab::Items => {
                let rows = self.item_rows();
                self.items.list.update(&rows);
            }
            Tab::Agents => {
                let rows = self.agent_rows();
                self.agents.update(&rows);
            }
            Tab::Now | Tab::Log => {}
        }
    }

    /// Takes in what the detail on screen shows, when only the detail changed.
    pub fn refresh_detail(&mut self, in_view: Option<InView>) {
        self.snapshot.in_view = in_view;
    }

    pub fn refresh_failed(&mut self, error: &anyhow::Error) {
        self.status = Some(Status::RefreshFailed(format!("{error:#}")));
    }

    /// Says why an action failed, at `now_ms`.
    pub fn action_failed(&mut self, error: &anyhow::Error, now_ms: u64) {
        self.status = Some(Status::ActionFailed {
            error: format!("{error:#}"),
            at_ms: now_ms,
        });
    }

    /// Says what an action did, at `now_ms`.
    pub fn did(&mut self, what: String, now_ms: u64) {
        self.status = (!what.is_empty()).then_some(Status::Done {
            what,
            at_ms: now_ms,
        });
    }

    /// Warns of `moves` back to `open`, besides those it warns of already.
    pub fn warn(&mut self, moves: impl IntoIterator<Item = MovedBack>) {
        self.moved_back.extend(moves);
    }

    /// Asks the principal to pick one of `choice`.
    pub fn choose(&mut self, choice: Choice) {
        self.prompt = Some(Prompt::Choose(choice));
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
    }

    /// How many lines or rows a page moves: about what a detail shows.
    fn page(&self) -> usize {
        // Less the header, the counts, the rules, the latest event, the keys and the breadcrumb,
        // and a line kept from the page before.
        usize::from(self.height.saturating_sub(8).max(1))
    }

    pub fn is_wide(&self) -> bool {
        self.width >= WIDE
    }

    /// Whether the list and the detail are side by side: in a wide window, unless one is folded
    /// away.
    pub fn shows_both(&self) -> bool {
        self.is_wide() && self.split == Split::Both
    }

    /// How many columns the list takes beside the detail.
    pub fn list_width(&self) -> u16 {
        (self.width * 2 / 5).min(64)
    }

    /// How many columns the text of a detail has: what the list or the strip leave of the
    /// window, less the margin on the left and the padding column on the right.
    pub fn detail_width(&self) -> u16 {
        let pane = match (self.is_wide(), self.split) {
            (false, _) => self.width,
            (true, Split::Both) => self.width.saturating_sub(self.list_width() + 1),
            // The list folded away keeps one column: the detail's margin is the strip's other.
            (true, Split::One) => self.width.saturating_sub(STRIP - 1),
        };
        pane.saturating_sub(2)
    }

    /// Takes in what a session's agent wrote to its transcript since the last time.
    pub fn transcribed(&mut self, session: SessionId, entries: &[TranscriptEntry]) {
        let transcript = self.transcripts.entry(session).or_default();
        for entry in entries {
            transcript.push(entry);
        }
    }

    /// Lets go of the transcripts of sessions other than `sessions`.
    pub fn keep_transcripts(&mut self, sessions: &[SessionId]) {
        self.transcripts
            .retain(|session, _| sessions.contains(session));
        self.transcript_lines
            .get_mut()
            .retain(|session, _| sessions.contains(session));
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
            // A flag about an item shows in the item's notice box.
            Row::Flag(flag) => Some(
                flag.item()
                    .map_or_else(|| Detail::Flag(flag.clone()), Detail::Item),
            ),
        }
    }

    fn waiting(&self, key: &WaitingKey) -> Option<&Waiting> {
        self.snapshot
            .waiting
            .iter()
            .find(|entry| &entry.key() == key)
    }

    /// The details opened on the tab on screen, the latest on top.
    pub fn stack(&self) -> &Vec<Opened> {
        match self.tab {
            Tab::Now => &self.now.panes.stack,
            Tab::Items => &self.items.list.panes.stack,
            Tab::Agents => &self.agents.panes.stack,
            Tab::Log => &self.log.stack,
        }
    }

    fn stack_mut(&mut self) -> &mut Vec<Opened> {
        match self.tab {
            Tab::Now => &mut self.now.panes.stack,
            Tab::Items => &mut self.items.list.panes.stack,
            Tab::Agents => &mut self.agents.panes.stack,
            Tab::Log => &mut self.log.stack,
        }
    }

    pub fn panes(&self) -> Option<&Panes> {
        match self.tab {
            Tab::Now => Some(&self.now.panes),
            Tab::Items => Some(&self.items.list.panes),
            Tab::Agents => Some(&self.agents.panes),
            Tab::Log => None,
        }
    }

    fn panes_mut(&mut self) -> Option<&mut Panes> {
        match self.tab {
            Tab::Now => Some(&mut self.now.panes),
            Tab::Items => Some(&mut self.items.list.panes),
            Tab::Agents => Some(&mut self.agents.panes),
            Tab::Log => None,
        }
    }

    /// Keeps the scroll of the text on screen and the top of the list to what drawing showed of
    /// them, so that the next key moves them from there rather than from past their end; and
    /// where a click lands.
    pub fn shown(&mut self, drawn: Drawn) {
        self.clickable = drawn.clickable;
        self.hovered = drawn.hovered;
        if let Some(list_top) = drawn.list_top
            && let Some(panes) = self.panes_mut()
        {
            panes.list_top = panes.list_top.at(list_top);
        }
        let Some(scroll) = drawn.text else {
            return;
        };
        let is_log = self.tab == Tab::Log;
        match self.stack_mut().last_mut() {
            Some(opened) => opened.scroll = scroll,
            None if is_log => self.log.scroll = scroll,
            None => {}
        }
    }

    /// Whether the keys go to the list of the tab on screen, rather than to a detail opened from
    /// it: when none is, or when the list has taken them back. The log is the list of its tab.
    pub fn list_has_keys(&self) -> bool {
        match self.panes() {
            Some(panes) => panes.stack.is_empty() || panes.focus == Pane::List,
            None => self.log.stack.is_empty(),
        }
    }

    /// The details on the way from the list to what is on screen: those opened, or in a wide
    /// window the selection's. None with only the list on screen, even with details kept.
    pub fn path(&self) -> Vec<Detail> {
        if !self.shows_both() && self.list_has_keys() {
            return Vec::new();
        }
        match self.stack() {
            stack if !stack.is_empty() => {
                stack.iter().map(|opened| opened.detail.clone()).collect()
            }
            _ => self
                .in_view()
                .map(|opened| opened.detail)
                .into_iter()
                .collect(),
        }
    }

    /// The title of an item, as far as the snapshot knows it.
    pub fn item_title(&self, item: ItemId) -> Option<&str> {
        let snapshot = &self.snapshot;
        let sessions = snapshot
            .working
            .iter()
            .chain(
                snapshot
                    .agents
                    .iter()
                    .flat_map(|agent| agent.sessions.iter()),
            )
            .map(|session| &session.item);
        let waiting = snapshot.waiting.iter().filter_map(|entry| match entry {
            Waiting::Permission { item, .. } => item.as_ref(),
            Waiting::Blocker { item, .. }
            | Waiting::Review { item, .. }
            | Waiting::Notice { item, .. } => Some(item),
        });
        let listed = snapshot
            .items
            .iter()
            .flat_map(|group| group.items.iter())
            .find(|row| row.id == item)
            .map(|row| row.title.as_str());
        listed.or_else(|| {
            sessions
                .chain(waiting)
                .find(|known| known.id == item)
                .map(|known| known.title.as_str())
        })
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
        match self.stack().last() {
            Some(opened) => Some(opened.detail.clone()),
            None => self.selected_detail(),
        }
    }

    /// The detail on screen: the one opened last, or beside the list the selection's. With one
    /// pane on screen, none while the list has the keys.
    pub fn in_view(&self) -> Option<Opened> {
        if !self.shows_both() && self.list_has_keys() {
            return None;
        }
        if let Some(opened) = self.stack().last() {
            return Some(opened.clone());
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
        if self.list_has_keys()
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
            Some(Detail::Agent(_) | Detail::Flag(_)) | None => Vec::new(),
        }
    }

    /// The item the item keys act on: of the open or selected detail.
    pub fn item_subject(&self) -> Option<ItemId> {
        match self.focus()? {
            Detail::Item(item) => Some(item),
            Detail::Session(session) => self.session_info(session).map(|info| info.item.id),
            Detail::Agent(_) | Detail::Flag(_) => None,
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
            Detail::Item(_) | Detail::Agent(_) | Detail::Flag(_) => None,
        }
    }

    /// What the snapshot knows of a session: as one that works, one in view, or one of an agent.
    pub fn session_info(&self, session: SessionId) -> Option<&SessionInfo> {
        let in_view = match &self.snapshot.in_view {
            Some(InView::Session(info)) => Some(info),
            Some(InView::Item { .. } | InView::Agent { .. } | InView::Flag { .. }) | None => None,
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

    /// Opens `detail`: on top of the one with the keys, or from the list in place of those
    /// opened before. Those opened from the list on `detail` before are given the keys back as
    /// they were left.
    fn open(&mut self, detail: Detail) {
        if let Detail::Item(item) = detail {
            self.moved_back.retain(|moved| moved.item != item);
        }
        let from_list = self.list_has_keys();
        if let Some(panes) = self.panes_mut() {
            panes.focus = Pane::Detail;
        }
        let stack = self.stack_mut();
        if from_list {
            if stack.first().is_some_and(|opened| opened.detail == detail) {
                return;
            }
            stack.clear();
        }
        stack.push(Opened {
            scroll: detail.start(),
            detail,
        });
    }

    pub fn key(&mut self, key: KeyEvent) -> Option<Effect> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Some(Effect::Quit);
        }
        self.unconfirmed_allow = None;
        self.moved_back.clear();
        if matches!(
            self.status,
            Some(Status::Done { .. } | Status::ActionFailed { .. })
        ) {
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
            KeyCode::Char('H') => self.step_split(Pane::List),
            KeyCode::Char('L') => self.step_split(Pane::Detail),
            KeyCode::Left if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.step_split(Pane::List);
            }
            KeyCode::Right if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.step_split(Pane::Detail);
            }
            // A detail covers the list behind it, so the list takes no keys while one is open.
            _ if !self.list_has_keys() => self.detail_key(key),
            _ if self.tab == Tab::Log => self.log_key(key),
            _ if self.move_selection(key) => {}
            _ => self.list_key(key),
        }
        None
    }

    /// `H` or `L`: one step of a wide window from the split towards the whole window for
    /// `towards`, or back. `list only ←H— split —L→ detail only`
    fn step_split(&mut self, towards: Pane) {
        if !self.is_wide() || self.panes().is_none() {
            return;
        }
        match self.split {
            Split::Both => {
                match towards {
                    Pane::List => self.focus_list(),
                    Pane::Detail => self.focus_detail(),
                }
                // Without a detail to give the keys to, both stay.
                if self.pane_with_keys() == towards {
                    self.split = Split::One;
                }
            }
            Split::One if self.pane_with_keys() != towards => self.split = Split::Both,
            Split::One => {}
        }
    }

    fn pane_with_keys(&self) -> Pane {
        if self.list_has_keys() {
            Pane::List
        } else {
            Pane::Detail
        }
    }

    /// The mouse wheel turned over `column`. It scrolls what is under the pointer: the list,
    /// leaving its selection and the keys where they are, or the detail beside it, which takes
    /// the keys; with one pane on screen, that one.
    pub fn wheel(&mut self, column: u16, wheel: Wheel) {
        if !self.mouse_passes() {
            return;
        }
        if self.tab == Tab::Log {
            match self.log.stack.last_mut() {
                Some(opened) => opened.scroll = wheel.scroll(opened.scroll),
                None => self.log.scroll = wheel.scroll(self.log.scroll),
            }
            return;
        }
        let over_list = if self.shows_both() {
            column < self.list_width()
        } else {
            self.list_has_keys()
        };
        if over_list {
            // Drawing keeps the list short of its end.
            if let Some(panes) = self.panes_mut() {
                panes.list_top = panes.list_top.scroll(wheel);
            }
            return;
        }
        self.focus_detail();
        // Without a detail to give the keys to, there is nothing to scroll.
        if let Some(opened) = self.stack_mut().last_mut() {
            opened.scroll = wheel.scroll(opened.scroll);
        }
    }

    /// The left button pressed at `column`, `row`. On a link or a button it does what that does;
    /// on a tab it switches to it; on an entry of a list it selects it and gives the list the
    /// keys, and opens it when it was selected already; on the detail beside a list it gives the
    /// detail the keys; on the strip of a pane folded away it brings both back.
    pub fn click(&mut self, column: u16, row: u16) -> Option<Effect> {
        if !self.mouse_passes() {
            return None;
        }
        let position = Position::new(column, row);
        // The pointer is where it clicked, even without a report of its moves.
        self.pointer = Some(position);
        // Only the very next click confirms an allow.
        let unconfirmed = self.unconfirmed_allow.take();
        let clickable = &self.clickable;
        let in_detail = clickable
            .detail
            .is_some_and(|detail| detail.contains(position));
        if let Some(spot) = clickable.spot(position) {
            let target = spot.target.clone();
            // The filter of the log, the only prompt a click goes through, is taken as typed.
            self.prompt = None;
            return self.follow(target, in_detail, unconfirmed);
        }
        if let Some((tab, _)) = clickable
            .tabs
            .iter()
            .find(|(_, label)| label.contains(position))
        {
            self.tab = *tab;
            self.prompt = None;
            return None;
        }
        if clickable
            .strip
            .is_some_and(|strip| strip.contains(position))
        {
            self.split = Split::Both;
            return None;
        }
        if let Some(list) = &clickable.list
            && list.area.contains(position)
        {
            let line = usize::from(row - list.area.y);
            if let Some(Some(entry)) = list.entries.get(line).copied() {
                self.click_entry(entry);
            }
            return None;
        }
        if in_detail {
            self.focus_detail();
        }
        None
    }

    /// Does what a click on a link or a button does; `in_detail` when it is in the detail beside
    /// the list, `unconfirmed` the click on allow before it.
    fn follow(
        &mut self,
        target: Target,
        in_detail: bool,
        unconfirmed: Option<UnconfirmedAllow>,
    ) -> Option<Effect> {
        match target {
            Target::Open { detail, .. } => {
                // On top of the detail it is in, so esc goes back to it.
                if in_detail {
                    self.focus_detail();
                }
                self.open(detail);
                None
            }
            Target::Diff { item, .. } => Some(Effect::ShowDiff { item }),
            Target::Key { code, .. } => self.key(KeyEvent::from(code)),
            Target::Back { depth, .. } => {
                self.stack_mut().truncate(depth);
                if depth > 0
                    && let Some(panes) = self.panes_mut()
                {
                    panes.focus = Pane::Detail;
                }
                None
            }
            Target::Allow(request) => {
                if unconfirmed.is_some_and(|unconfirmed| self.confirms(unconfirmed, request)) {
                    return Some(Effect::Permit {
                        session: request.session,
                        answer: Permission::Allow,
                    });
                }
                self.unconfirmed_allow = Some(UnconfirmedAllow {
                    request,
                    at_ms: self.snapshot.now_ms,
                });
                None
            }
            Target::Deny(request) => Some(Effect::Permit {
                session: request.session,
                answer: Permission::Deny,
            }),
        }
    }

    /// Whether a click on allow for `request` confirms `unconfirmed`: the first click was on
    /// the same request, which still waits, not longer than [`CONFIRM_LASTS_MS`] ago.
    fn confirms(&self, unconfirmed: UnconfirmedAllow, request: Request) -> bool {
        unconfirmed.request == request
            && self.snapshot.now_ms < unconfirmed.at_ms + CONFIRM_LASTS_MS
            && self
                .snapshot
                .waiting
                .iter()
                .any(|entry| entry.request() == Some(request))
    }

    /// What a click on `target` does, for the latest-event line while the pointer is over it.
    pub fn tip(&self, target: &Target) -> String {
        match target {
            Target::Allow(request)
                if self
                    .unconfirmed_allow
                    .is_some_and(|unconfirmed| unconfirmed.request == *request) =>
            {
                "allow: click again to confirm".to_owned()
            }
            Target::Open { .. }
            | Target::Diff { .. }
            | Target::Key { .. }
            | Target::Back { .. }
            | Target::Allow(_)
            | Target::Deny(_) => target.tip(),
        }
    }

    /// The pointer moved to `column`, `row`. Returns whether that took it onto or off a link, a
    /// button or a tab, which is drawn again only then.
    pub fn hover(&mut self, column: u16, row: u16) -> bool {
        let position = Position::new(column, row);
        self.pointer = Some(position);
        let under = self.hovered_at(position);
        let changed = under != self.hovered;
        self.hovered = under;
        changed
    }

    /// The terminal pane was focused, or left. Returns whether that changed anything, which is
    /// drawn again only then. Leaving it lets go of what is under the pointer.
    pub fn terminal_focus(&mut self, focused: bool) -> bool {
        if self.terminal_focused == focused {
            return false;
        }
        self.terminal_focused = focused;
        if !focused {
            self.pointer = None;
            self.hovered = None;
        }
        true
    }

    /// The link, button or tab at `position` as last drawn, while the mouse acts.
    fn hovered_at(&self, position: Position) -> Option<Hovered> {
        if !self.mouse_passes() {
            return None;
        }
        self.clickable.hovered(position)
    }

    /// Selects the entry at `index` of the list on screen and gives the list the keys; opens it
    /// when it was selected already and the list had the keys, as enter would.
    fn click_entry(&mut self, index: usize) {
        let picked = match self.tab {
            Tab::Now => {
                let rows = self.now_rows();
                self.now.selection.pick(&rows, index)
            }
            Tab::Items => {
                let rows = self.item_rows();
                self.items.list.selection.pick(&rows, index)
            }
            Tab::Agents => {
                let rows = self.agent_rows();
                self.agents.selection.pick(&rows, index)
            }
            Tab::Log => None,
        };
        let had_keys = self.list_has_keys();
        let Some(panes) = self.panes_mut() else {
            return;
        };
        match picked {
            None => {}
            Some(Picked::Again) if had_keys => {
                if let Some(detail) = self.selected_detail() {
                    self.open(detail);
                }
            }
            // A click on the selection only takes the keys from the detail.
            Some(Picked::Again) => panes.focus = Pane::List,
            // The details opened before give way to the selection's, which the list keeps on
            // screen from now on.
            Some(Picked::Moved) => {
                panes.stack.clear();
                panes.list_top = panes.list_top.follow_selection();
            }
        }
    }

    /// Gives the keys to the list, leaving the details opened from it as they are.
    fn focus_list(&mut self) {
        if let Some(panes) = self.panes_mut() {
            panes.focus = Pane::List;
        }
    }

    /// Gives the keys to the detail on screen beside the list: the one opened, else the
    /// selection's, if it has one.
    fn focus_detail(&mut self) {
        if !self.list_has_keys() {
            return;
        }
        let Some(panes) = self.panes_mut() else {
            return;
        };
        if !panes.stack.is_empty() {
            panes.focus = Pane::Detail;
            return;
        }
        // The detail of the selection is on screen beside the list already: opening it only
        // gives it the keys.
        if let Some(detail) = self.selected_detail() {
            self.open(detail);
        }
    }

    /// Whether the mouse acts while the footer asks for something: only while the log is
    /// filtered, so the wheel goes through what matches.
    pub fn mouse_passes(&self) -> bool {
        match &self.prompt {
            None
            | Some(Prompt::Filter {
                list: Filtered::Log,
                ..
            }) => true,
            Some(
                Prompt::Filter {
                    list: Filtered::Items,
                    ..
                }
                | Prompt::Answer { .. }
                | Prompt::Verdict { .. }
                | Prompt::Comment { .. }
                | Prompt::Choose(_)
                | Prompt::Stop { .. }
                | Prompt::Keys,
            ) => false,
        }
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
                Some(Effect::ResolveInEditor {
                    item,
                    blocker,
                    text: String::new(),
                })
            }
            KeyCode::Char('v') => {
                self.prompt = Some(Prompt::Verdict { item: review? });
                None
            }
            KeyCode::Char('D') => self.item_with_work().map(|item| Effect::ShowDiff { item }),
            KeyCode::Char('s') => self.item_subject().map(|item| Effect::ChooseStart { item }),
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
            Prompt::Filter { list, mut input } => {
                let filter = match list {
                    Filtered::Items => &mut self.items.filter,
                    Filtered::Log => {
                        // A line of the lines that matched is no place in those that match now.
                        self.log.scroll = Scroll::END;
                        &mut self.log.filter
                    }
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
                        (Some(Prompt::Filter { list, input }), None)
                    }
                }
            }
            Prompt::Answer {
                item,
                blocker,
                mut input,
            } => match key.code {
                KeyCode::Esc => (None, None),
                KeyCode::Char('g') if key.modifiers.contains(KeyModifiers::CONTROL) => (
                    None,
                    Some(Effect::ResolveInEditor {
                        item,
                        blocker,
                        text: input.value().trim().to_owned(),
                    }),
                ),
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
                KeyCode::Char('g') if key.modifiers.contains(KeyModifiers::CONTROL) => (
                    None,
                    Some(Effect::CommentInEditor {
                        item,
                        text: input.value().trim().to_owned(),
                    }),
                ),
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

    /// The keys of the open detail.
    fn detail_key(&mut self, key: KeyEvent) {
        let item_session = match &self.snapshot.in_view {
            Some(InView::Item { session, .. }) => *session,
            Some(InView::Session(_) | InView::Agent { .. } | InView::Flag { .. }) | None => None,
        };
        let item_of_session = self.item_subject();
        let page = self.page();
        if matches!(key.code, KeyCode::Esc | KeyCode::Left | KeyCode::Char('h')) {
            match self.panes_mut() {
                // The detail opened from the list stays as it is, to come back to with `l`.
                Some(panes) if panes.stack.len() == 1 => panes.focus = Pane::List,
                Some(_) | None => {
                    self.stack_mut().pop();
                }
            }
            return;
        }
        let Some(opened) = self.stack_mut().last_mut() else {
            return;
        };
        if let Some(scroll) = opened.scroll.key(key, page) {
            opened.scroll = scroll;
            return;
        }
        match (key.code, &opened.detail) {
            (KeyCode::Enter | KeyCode::Right | KeyCode::Char('l'), Detail::Item(_)) => {
                if let Some(session) = item_session {
                    self.open(Detail::Session(session));
                }
            }
            (KeyCode::Char('i'), Detail::Session(_)) => {
                if let Some(item) = item_of_session {
                    self.open(Detail::Item(item));
                }
            }
            (KeyCode::Char('f'), Detail::Session(_)) => {
                // A line of one transcript is no place in the other: start again at the end.
                opened.scroll = Scroll::END;
                self.verbosity = match self.verbosity {
                    Verbosity::Abridged => Verbosity::Full,
                    Verbosity::Full => Verbosity::Abridged,
                };
            }
            _ => {}
        }
    }

    /// Moves the selection of the list on screen as `key` asks; `false` for a key that does not
    /// move it. The details opened before give way to the selection's.
    fn move_selection(&mut self, key: KeyEvent) -> bool {
        let page = self.page();
        let moved = match self.tab {
            Tab::Now => {
                let rows = self.now_rows();
                self.now.selection.key(&rows, key, page)
            }
            Tab::Items => {
                let rows = self.item_rows();
                self.items.list.selection.key(&rows, key, page)
            }
            Tab::Agents => {
                let rows = self.agent_rows();
                self.agents.selection.key(&rows, key, page)
            }
            Tab::Log => false,
        };
        if moved {
            self.stack_mut().clear();
            // A list scrolled away from the selection comes back to show it.
            if let Some(panes) = self.panes_mut() {
                panes.list_top = panes.list_top.follow_selection();
            }
        }
        moved
    }

    /// The keys of the list on screen, besides those that move its selection.
    fn list_key(&mut self, key: KeyEvent) {
        match (self.tab, key.code) {
            (
                Tab::Now | Tab::Items | Tab::Agents,
                KeyCode::Enter | KeyCode::Right | KeyCode::Char('l'),
            ) => {
                if let Some(detail) = self.selected_detail() {
                    self.open(detail);
                }
            }
            (Tab::Items, KeyCode::Char('o')) => {
                let index = GROUPINGS
                    .iter()
                    .position(|grouping| *grouping == self.items.grouping)
                    .map_or(0, |index| (index + 1) % GROUPINGS.len());
                self.items.grouping = GROUPINGS[index];
            }
            (Tab::Items, KeyCode::Char('F')) => self.items.readiness = self.items.readiness.next(),
            (Tab::Items, KeyCode::Char('/')) => {
                self.prompt = Some(Prompt::Filter {
                    list: Filtered::Items,
                    input: Input::new(self.items.filter.clone()),
                });
            }
            (Tab::Items, KeyCode::Esc) => self.items.filter.clear(),
            (Tab::Now | Tab::Items | Tab::Agents | Tab::Log, _) => {}
        }
    }

    fn log_key(&mut self, key: KeyEvent) {
        if let Some(scroll) = self.log.scroll.key(key, self.page()) {
            self.log.scroll = scroll;
            return;
        }
        match key.code {
            KeyCode::Char('/') => {
                self.prompt = Some(Prompt::Filter {
                    list: Filtered::Log,
                    input: Input::new(self.log.filter.clone()),
                });
            }
            KeyCode::Esc => {
                self.log.filter.clear();
                self.log.scroll = Scroll::END;
            }
            _ => {}
        }
    }
}

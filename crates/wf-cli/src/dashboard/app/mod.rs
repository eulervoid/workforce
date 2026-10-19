//! The state of the dashboard and what keys and the mouse do to it. Pure: what a key asks of the
//! workforce comes back as an [`Effect`], which the loop carries out.

use std::collections::HashMap;

use jiff::tz::TimeZone;
use ratatui::layout::Position;
use ratatui::style::Style;
use ratatui::text::Line;
use wf_core::graph::Readiness;
use wf_core::id::{HashId, ItemId, SessionId};
use wf_core::session::CommitId;
use wf_harness::TranscriptEntry;

mod clickable;
mod effect;
mod keys;
mod mouse;
mod nav;
mod prompt;

pub use self::clickable::*;
pub use self::effect::*;
pub use self::nav::*;
pub use self::prompt::*;

use crate::cli::{ItemGrouping, ItemOrder};
use crate::dashboard::diff::{Changes, DiffPane, LineComment};
use crate::dashboard::draw;
use crate::dashboard::snapshot::{
    Detail, InView, InboxEntry, InboxKey, ItemListing, Listing, MovedBack, Place, SessionInfo,
    Snapshot, Wanted,
};
use crate::dashboard::transcript_lines::TranscriptLines;
use crate::theme;
use crate::transcript::{self, Format, Transcript, Verbosity};
use crate::view::{ItemSummary, SessionStatus};

/// From this width the detail sits right of the list, instead of in its place.
pub const WIDE: u16 = 120;

/// How many columns a pane folded away keeps at its edge, in a wide window.
pub const STRIP: u16 = 2;

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

/// `wf item list --group-by --sort`.
#[derive(Clone, Debug)]
pub struct ItemsTab {
    pub list: ListTab<ItemId>,
    pub grouping: ItemGrouping,
    pub order: ItemOrder,
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

/// The orders `O` goes through, in order.
const ORDERS: [ItemOrder; 3] = [ItemOrder::Priority, ItemOrder::Created, ItemOrder::Merged];

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
    /// Counts the lines taken in, so that what was made of them is known to be current.
    revision: u64,
    /// The lines that pass the filter as drawn, kept until they or the filter change.
    shown: ShownLog,
}

/// The lines of the Log tab that pass its filter, parsed for drawing.
#[derive(Clone, Debug, Default)]
pub struct ShownLog {
    /// The revision of the lines and the filter these were made for; `None` before the first.
    made_for: Option<(u64, String)>,
    lines: Vec<Line<'static>>,
    /// The width of the widest of them.
    widest: usize,
}

impl ShownLog {
    pub fn lines(&self) -> &[Line<'static>] {
        &self.lines
    }

    /// Whether none of the lines is wider than `width` columns, so that none wraps.
    pub fn fits(&self, width: usize) -> bool {
        self.widest <= width
    }
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
    /// Why the workforce could not be read, until a refresh succeeds.
    RefreshFailed(String),
    /// Why a transcript could not be read, said at `at_ms`, until the next key or
    /// [`STATUS_LASTS_MS`] later: a transcript is read once, so nothing says when it is fine again.
    TranscriptFailed { error: String, at_ms: u64 },
}

pub struct App {
    pub snapshot: Snapshot,
    /// The transcripts of the sessions this machine runs, as far as they are read.
    pub transcripts: HashMap<SessionId, Transcript>,
    /// The lines of the transcripts as last drawn, kept so that drawing lays out only what was
    /// added since.
    pub transcript_lines: HashMap<SessionId, TranscriptLines>,
    /// The diffs opened, by item, kept with where their cursor is and the comments of the review
    /// being written while other details are on screen.
    pub diffs: HashMap<ItemId, DiffPane>,
    pub tab: Tab,
    pub now: ListTab<NowRow>,
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
    /// What every cell has under what is drawn on it: the theme's text, on its background.
    pub base: Style,
}

impl App {
    pub fn new(snapshot: Snapshot, time_zone: TimeZone) -> Self {
        let mut app = Self {
            snapshot: Snapshot::default(),
            transcripts: HashMap::new(),
            transcript_lines: HashMap::new(),
            diffs: HashMap::new(),
            tab: Tab::Now,
            now: ListTab::default(),
            items: ItemsTab {
                list: ListTab::default(),
                grouping: ItemGrouping::State,
                order: ItemOrder::Priority,
                filter: String::new(),
                readiness: ReadinessFilter::All,
            },
            agents: ListTab::default(),
            log: LogTab {
                lines: Vec::new(),
                scroll: Scroll::END,
                filter: String::new(),
                stack: Vec::new(),
                revision: 0,
                shown: ShownLog::default(),
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
            base: draw::base(theme::text(), theme::background()),
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
        self.settle();
    }

    /// Moves the clock of the snapshot on screen when nothing else changed.
    pub fn tick(&mut self, now_ms: u64) {
        self.snapshot.now_ms = now_ms;
        self.settle();
    }

    /// Lets what the app keeps follow the snapshot on screen.
    fn settle(&mut self) {
        self.take_posted();
        match self.status {
            Some(
                Status::Done { at_ms, .. }
                | Status::ActionFailed { at_ms, .. }
                | Status::TranscriptFailed { at_ms, .. },
            ) if self.snapshot.now_ms >= at_ms + STATUS_LASTS_MS => {
                self.status = None;
            }
            Some(
                Status::Done { .. }
                | Status::ActionFailed { .. }
                | Status::RefreshFailed(_)
                | Status::TranscriptFailed { .. },
            )
            | None => {}
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
        self.keep_diffs();
    }

    /// Lets go of the diffs that are not open in any tab, but for those with a review being
    /// written: its comments are not to be lost to a closed diff.
    fn keep_diffs(&mut self) {
        let stacks = [
            &self.now.panes.stack,
            &self.items.list.panes.stack,
            &self.agents.panes.stack,
            &self.log.stack,
        ];
        self.diffs.retain(|item, pane| {
            !pane.drafts().is_empty()
                || stacks.iter().any(|stack| {
                    stack
                        .iter()
                        .any(|opened| opened.detail == Detail::Diff(*item))
                })
        });
    }

    /// Takes in what the detail on screen shows, when only the detail changed.
    pub fn refresh_detail(&mut self, in_view: Option<InView>) {
        self.snapshot.in_view = in_view;
        self.take_posted();
    }

    /// Hands the diff in view the comments on its lines in the log.
    fn take_posted(&mut self) {
        if let Some(InView::Diff { id, comments }) = &self.snapshot.in_view
            && let Some(pane) = self.diffs.get_mut(id)
        {
            pane.set_posted(comments);
        }
    }

    /// Says why the workforce could not be read, unless an action's failure is still unseen.
    pub fn refresh_failed(&mut self, error: &anyhow::Error) {
        self.fail_quietly(Status::RefreshFailed(format!("{error:#}")));
    }

    /// Says why a transcript could not be read, unless an action's failure is still unseen.
    pub fn transcript_failed(&mut self, error: &anyhow::Error) {
        self.fail_quietly(Status::TranscriptFailed {
            error: format!("{error:#}"),
            at_ms: self.snapshot.now_ms,
        });
    }

    fn fail_quietly(&mut self, failure: Status) {
        if !matches!(self.status, Some(Status::ActionFailed { .. })) {
            self.status = Some(failure);
        }
    }

    /// The workforce was read: what failed to read it is no longer true.
    pub fn refreshed(&mut self) {
        if matches!(self.status, Some(Status::RefreshFailed(_))) {
            self.status = None;
        }
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
            .retain(|session, _| sessions.contains(session));
    }

    /// Lays out the lines of the transcript of the session on screen that drawing has not seen
    /// yet, for the width of its text.
    pub fn lay_out_transcript(&mut self) {
        let Some(Opened {
            detail: Detail::Session(session),
            ..
        }) = self.on_screen()
        else {
            return;
        };
        let Some(InView::Session(info)) = self.in_view_of(&Detail::Session(session)) else {
            return;
        };
        let (Place::Here, Some(transcript)) = (&info.place, self.transcripts.get(&session)) else {
            return;
        };
        let layout = transcript::Layout::new(
            info.agent_name.as_str(),
            usize::from(self.detail_width()).max(20),
            Format::Styled,
            self.verbosity,
            self.time_zone.clone(),
        );
        self.transcript_lines
            .entry(session)
            .or_insert_with(|| TranscriptLines::new(layout.clone()))
            .update(layout, transcript);
    }

    /// Takes in the lines of new events.
    pub fn logged(&mut self, lines: impl IntoIterator<Item = String>) {
        self.log.lines.extend(lines);
        let over = self.log.lines.len().saturating_sub(LOG_LINES);
        self.log.lines.drain(..over);
        self.log.revision += 1;
    }

    /// Parses the lines of the Log tab that pass its filter, unless that was done for these lines
    /// and this filter already.
    pub fn lay_out_log(&mut self) {
        let made_for = (self.log.revision, self.log.filter.clone());
        if self.log.shown.made_for.as_ref() == Some(&made_for) {
            return;
        }
        let text: String = self.log_lines().map(String::as_str).collect();
        let lines = draw::ansi_text(&text).lines;
        self.log.shown = ShownLog {
            made_for: Some(made_for),
            widest: lines.iter().map(Line::width).max().unwrap_or(0),
            lines,
        };
    }

    /// The lines of the Log tab that pass its filter, as [`App::lay_out_log`] parsed them.
    pub fn shown_log(&self) -> &ShownLog {
        &self.log.shown
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
    pub fn now_rows(&self) -> Vec<NowRow> {
        let snapshot = &self.snapshot;
        let waiting = snapshot
            .waiting
            .iter()
            .map(|entry| NowRow::Waiting(entry.key()));
        let working = snapshot
            .working
            .iter()
            .map(|session| NowRow::Session(session.id));
        let flagged = snapshot.flagged.iter().cloned().map(NowRow::Flag);
        waiting.chain(working).chain(flagged).collect()
    }

    /// The groups of the Items tab with the items that pass its filters; empty groups are left
    /// out. The text filter passes an item whose title contains it or whose id starts with it,
    /// as `wf item` commands resolve an id, with or without a leading `#`.
    pub fn item_groups(&self) -> Vec<(&str, Vec<&ItemSummary>)> {
        let filter = self.items.filter.to_lowercase();
        let id_prefix = filter.strip_prefix('#').unwrap_or(&filter);
        self.snapshot
            .items
            .iter()
            .map(|group| {
                let items = group
                    .items
                    .iter()
                    .filter(|item| self.items.readiness.accepts(item.readiness))
                    .filter(|item| {
                        item.title.to_lowercase().contains(&filter)
                            || item.id.to_base32().starts_with(id_prefix)
                    })
                    .collect();
                (group.group.as_str(), items)
            })
            .filter(|(_, items): &(&str, Vec<&ItemSummary>)| !items.is_empty())
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
    pub fn detail_of(&self, row: &NowRow) -> Option<Detail> {
        match row {
            NowRow::Waiting(key) => match self.waiting(key)? {
                InboxEntry::Permission { session, .. } => Some(Detail::Session(*session)),
                InboxEntry::Blocker { item, .. }
                | InboxEntry::Review { item, .. }
                | InboxEntry::Notice { item, .. } => Some(Detail::Item(item.id)),
                // The row says all there is to say.
                InboxEntry::MachineFault { .. } => None,
            },
            NowRow::Session(session) => Some(Detail::Session(*session)),
            // A flag about an item shows in the item's notice box.
            NowRow::Flag(flag) => Some(
                flag.item()
                    .map_or_else(|| Detail::Flag(flag.clone()), Detail::Item),
            ),
        }
    }

    fn waiting(&self, key: &InboxKey) -> Option<&InboxEntry> {
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
        for pane in self.diffs.values_mut() {
            pane.area = None;
        }
        match drawn.detail {
            Some(DrawnDetail::Diff { area, top, cursor }) => {
                if let Some(pane) = self.diff_on_top_mut() {
                    pane.shown(area, top, cursor);
                }
            }
            Some(DrawnDetail::Text(scroll)) => {
                let is_log = self.tab == Tab::Log;
                match self.stack_mut().last_mut() {
                    Some(opened) => opened.scroll = scroll,
                    None if is_log => self.log.scroll = scroll,
                    None => {}
                }
            }
            None => {}
        }
    }

    /// Whether the keys go to the list of the tab on screen, rather than to a detail opened from
    /// it: when none is, or when the list has taken them back. The log is the list of its tab.
    pub fn list_has_keys(&self) -> bool {
        match self.panes() {
            Some(panes) => panes.stack.is_empty() || panes.keys == Pane::List,
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
                .on_screen()
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
            .map(|session| (session.item, session.title.as_str()));
        let waiting = snapshot
            .waiting
            .iter()
            .filter_map(|entry| match entry {
                InboxEntry::Permission { item, .. } => item.as_ref(),
                InboxEntry::Blocker { item, .. }
                | InboxEntry::Review { item, .. }
                | InboxEntry::Notice { item, .. } => Some(item),
                InboxEntry::MachineFault { .. } => None,
            })
            .map(|item| (item.id, item.title.as_str()));
        let listed = snapshot
            .items
            .iter()
            .flat_map(|group| group.items.iter())
            .find(|row| row.id == item)
            .map(|row| row.title.as_str());
        listed.or_else(|| {
            sessions
                .chain(waiting)
                .find(|(known, _)| *known == item)
                .map(|(_, title)| title)
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
    fn detail_for_keys(&self) -> Option<Detail> {
        match self.stack().last() {
            Some(opened) => Some(opened.detail.clone()),
            None => self.selected_detail(),
        }
    }

    /// The detail on screen: the one opened last, or beside the list the selection's. With one
    /// pane on screen, none while the list has the keys.
    pub fn on_screen(&self) -> Option<Opened> {
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
            detail: self.on_screen().map(|opened| opened.detail),
            listing: match self.tab {
                Tab::Items => Some(Listing::Items(ItemListing {
                    grouping: self.items.grouping,
                    order: self.items.order,
                })),
                Tab::Agents => Some(Listing::Agents),
                Tab::Now | Tab::Log => None,
            },
        }
    }

    /// What waits that the keys on screen act on: the selected entry, the permission request of
    /// a selected or open session, or what waits about the selected or open item.
    pub fn subjects(&self) -> Vec<&InboxEntry> {
        if self.list_has_keys()
            && self.tab == Tab::Now
            && let Some(NowRow::Waiting(key)) = self.now.selection.get()
        {
            return self.waiting(key).into_iter().collect();
        }
        let waiting = self.snapshot.waiting.iter();
        match self.detail_for_keys() {
            Some(Detail::Session(id)) => waiting
                .filter(
                    |entry| matches!(entry, InboxEntry::Permission { session, .. } if *session == id),
                )
                .collect(),
            Some(Detail::Item(id)) => waiting
                .filter(|entry| match entry {
                    InboxEntry::Blocker { item, .. } | InboxEntry::Review { item, .. } => item.id == id,
                    InboxEntry::Permission { .. }
                    | InboxEntry::Notice { .. }
                    | InboxEntry::MachineFault { .. } => false,
                })
                .collect(),
            // A diff is looked at to review it.
            Some(Detail::Diff(id)) => waiting
                .filter(|entry| match entry {
                    InboxEntry::Review { item, .. } => item.id == id,
                    InboxEntry::Permission { .. }
                    | InboxEntry::Blocker { .. }
                    | InboxEntry::Notice { .. }
                    | InboxEntry::MachineFault { .. } => false,
                })
                .collect(),
            Some(Detail::Agent(_) | Detail::Flag(_)) | None => Vec::new(),
        }
    }

    /// The item the item keys act on: of the open or selected detail.
    pub fn item_subject(&self) -> Option<ItemId> {
        match self.detail_for_keys()? {
            Detail::Item(item) | Detail::Diff(item) => Some(item),
            Detail::Session(session) => self.session_info(session).map(|info| info.item),
            Detail::Agent(_) | Detail::Flag(_) => None,
        }
    }

    /// The session `x` stops: the one open, or selected, while it is live.
    pub fn session_subject(&self) -> Option<SessionId> {
        match self.detail_for_keys()? {
            Detail::Session(session) => self
                .session_info(session)
                .filter(|info| match info.status {
                    SessionStatus::Live | SessionStatus::Parked => true,
                    SessionStatus::Ended { .. } => false,
                })
                .map(|info| info.id),
            Detail::Item(_) | Detail::Agent(_) | Detail::Flag(_) | Detail::Diff(_) => None,
        }
    }

    /// What the snapshot knows of a session: as one that works, one in view, or one of an agent.
    pub fn session_info(&self, session: SessionId) -> Option<&SessionInfo> {
        let in_view = match &self.snapshot.in_view {
            Some(InView::Session(info)) => Some(info),
            Some(
                InView::Item { .. }
                | InView::Agent { .. }
                | InView::Flag { .. }
                | InView::Diff { .. },
            )
            | None => None,
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
        let review = self
            .subjects()
            .into_iter()
            .find_map(|entry| entry.review().map(|(item, _)| item));
        review.or_else(|| self.item_subject())
    }

    /// Opens `detail`: on top of the one with the keys, or from the list in place of those
    /// opened before. Those opened from the list on `detail` before are given the keys back as
    /// they were left.
    fn open(&mut self, detail: Detail) {
        if let Detail::Item(item) = detail {
            self.moved_back.retain(|moved| moved.item != item);
        }
        if let Detail::Diff(item) = detail {
            self.diffs.entry(item).or_default();
        }
        let from_list = self.list_has_keys();
        if let Some(panes) = self.panes_mut() {
            panes.keys = Pane::Detail;
        }
        let stack = self.stack_mut();
        if from_list {
            if stack.first().is_some_and(|opened| opened.detail == detail) {
                return;
            }
            stack.clear();
        } else if stack.last().is_some_and(|opened| opened.detail == detail) {
            return;
        }
        stack.push(Opened {
            scroll: detail.start(),
            detail,
        });
    }

    /// `i` in a diff or session of `item`: the item stands left of it, so back to the nearest
    /// `item` lower on the stack (as `h` would, keeping its scroll), else the item in place of
    /// the diff or session.
    fn open_item_of_top(&mut self, item: ItemId) {
        let stack = self.stack_mut();
        let lower = stack.len().saturating_sub(1);
        if let Some(index) = stack[..lower]
            .iter()
            .rposition(|opened| opened.detail == Detail::Item(item))
        {
            stack.truncate(index + 1);
            return;
        }
        stack.pop();
        self.open(Detail::Item(item));
    }

    /// What the snapshot has read for `detail`; nothing while it still shows another detail.
    pub fn in_view_of(&self, detail: &Detail) -> Option<&InView> {
        self.snapshot
            .in_view
            .as_ref()
            .filter(|in_view| in_view.is_of(detail))
    }

    /// The diff open on top, on screen or kept beside the list.
    pub fn diff_on_top(&self) -> Option<&DiffPane> {
        let Some(Opened {
            detail: Detail::Diff(item),
            ..
        }) = self.stack().last()
        else {
            return None;
        };
        self.diffs.get(item)
    }

    /// The diff open on top, on screen or kept beside the list.
    pub fn diff_on_top_mut(&mut self) -> Option<&mut DiffPane> {
        let Some(Opened {
            detail: Detail::Diff(item),
            ..
        }) = self.stack().last()
        else {
            return None;
        };
        let item = *item;
        self.diffs.get_mut(&item)
    }

    /// Whether the work of `item` waits for the principal's review.
    fn review_waits(&self, item: ItemId) -> bool {
        self.snapshot
            .waiting
            .iter()
            .any(|entry| entry.review_of(item).is_some())
    }

    /// The commit of `item` the principal looks at when giving a verdict: the one the diff on
    /// screen shows (none for work without commits), or without a diff read, the one the review
    /// waiting in the inbox is of.
    fn reviewed_commit(&self, item: ItemId) -> Option<CommitId> {
        let shown = self.diffs.get(&item).and_then(|pane| match pane.work() {
            Some(Ok(work)) => Some(match work.changes {
                Changes::NoCommits => None,
                Changes::Files(_) => Some(work.commit),
            }),
            Some(Err(_)) | None => None,
        });
        shown.unwrap_or_else(|| {
            self.snapshot
                .waiting
                .iter()
                .find_map(|entry| entry.review_of(item))
                .flatten()
        })
    }

    /// The comments on lines of the review of `item` being written.
    pub fn drafts(&self, item: ItemId) -> &[LineComment] {
        self.diffs.get(&item).map_or(&[], |pane| pane.drafts())
    }

    /// Takes in a comment on lines written in `$EDITOR`: a draft of the review of `item`, in
    /// place of `replacing`.
    pub fn drafted(&mut self, item: ItemId, comment: LineComment, replacing: Option<usize>) {
        if let Some(pane) = self.diffs.get_mut(&item) {
            pane.drafted(comment, replacing);
        }
    }

    /// The review of `item` was sent, with its drafts.
    pub fn reviewed(&mut self, item: ItemId) {
        if let Some(pane) = self.diffs.get_mut(&item) {
            pane.clear_drafts();
        }
    }

    /// The first `posted` drafts of the review of `item` were posted, but the verdict was not
    /// given: they are not drafts any more, and the rest still are.
    pub fn drafts_posted(&mut self, item: ItemId, posted: usize) {
        if let Some(pane) = self.diffs.get_mut(&item) {
            pane.drop_first_drafts(posted);
        }
    }

    /// Whether the keys go to a diff.
    pub fn diff_has_keys(&self) -> bool {
        !self.list_has_keys()
            && matches!(
                self.stack().last(),
                Some(Opened {
                    detail: Detail::Diff(_),
                    ..
                })
            )
    }
}

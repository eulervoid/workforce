//! Where the principal is in a tab: what is scrolled, what is selected, which pane has the keys.

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use wf_core::id::SessionId;
use wf_core::identity::AgentId;

use crate::dashboard::snapshot::{Detail, WaitingKey};

/// How many lines a text moves for a notch of the mouse wheel.
const WHEEL_LINES: usize = 3;

/// Which way the mouse wheel turned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wheel {
    Up,
    Down,
}

impl Wheel {
    pub(super) fn scroll(self, scroll: Scroll) -> Scroll {
        match self {
            Wheel::Up => scroll.up(WHEEL_LINES),
            Wheel::Down => scroll.down(WHEEL_LINES),
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

    pub(super) fn down(self, lines: usize) -> Scroll {
        match self {
            Scroll::At(top) => Scroll::At(top.saturating_add(lines)),
            Scroll::End { .. } => self,
        }
    }

    pub(super) fn up(self, lines: usize) -> Scroll {
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
    pub(super) fn key(self, key: KeyEvent, page: usize) -> Option<Scroll> {
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
    pub(super) fn update(&mut self, rows: &[Key]) {
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

    /// What the keys of a list do to its selection, a page being `page` rows.
    pub(super) fn key(&mut self, rows: &[Key], key: KeyEvent, page: usize) -> Navigated {
        self.update(rows);
        let before = self.index;
        let last = rows.len().saturating_sub(1);
        self.index = match key.code {
            KeyCode::Down | KeyCode::Char('j') => (self.index + 1).min(last),
            KeyCode::Up | KeyCode::Char('k') => self.index.saturating_sub(1),
            KeyCode::PageDown => (self.index + page).min(last),
            KeyCode::PageUp => self.index.saturating_sub(page),
            KeyCode::Char('g') => 0,
            KeyCode::Char('G') => last,
            _ => return Navigated::Unused,
        };
        self.selected = rows.get(self.index).cloned();
        if self.index == before {
            Navigated::Stayed
        } else {
            Navigated::Moved
        }
    }

    /// Selects the row at `index` of `rows`; `None` when there is none.
    pub(super) fn pick(&mut self, rows: &[Key], index: usize) -> Option<Picked> {
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

/// What a key did to the selection of a list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Navigated {
    /// The key is not one of navigation.
    Unused,
    /// A navigation key that left the selection where it was.
    Stayed,
    /// A navigation key that changed the selection.
    Moved,
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
    /// The first line of the list on screen, and whether it follows the selection.
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
    pub fn line(self) -> usize {
        match self {
            ListTop::FollowsSelection(line) | ListTop::Scrolled(line) => line,
        }
    }

    /// The same way of keeping the list, from `line`.
    pub fn at(self, line: usize) -> Self {
        match self {
            ListTop::FollowsSelection(_) => ListTop::FollowsSelection(line),
            ListTop::Scrolled(_) => ListTop::Scrolled(line),
        }
    }

    /// The list from where it is on screen, kept to the selection again.
    pub fn follow_selection(self) -> Self {
        ListTop::FollowsSelection(self.line())
    }

    pub fn scroll(self, wheel: Wheel) -> Self {
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
    pub(super) fn update(&mut self, rows: &[Key]) {
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

/// What a click on an entry of a list did to its selection.
pub(super) enum Picked {
    /// The entry was selected already.
    Again,
    Moved,
}

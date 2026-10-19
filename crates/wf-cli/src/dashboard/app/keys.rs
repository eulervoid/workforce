//! What keys do, outside a prompt.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tui_input::Input;
use wf_core::event::CommentKind;
use wf_core::id::ItemId;
use wf_harness::Permission;

use super::effect::Effect;
use super::nav::{Navigated, Opened, Pane, Scroll};
use super::prompt::{Filtered, Prompt, Written};
use super::{App, GROUPINGS, ORDERS, Split, Status, Tab};
use crate::dashboard::snapshot::{Detail, InView};
use crate::transcript::Verbosity;

/// What the keys that act on what waits made of a key.
pub(super) enum Acted {
    /// It asks this of the workforce.
    Effect(Effect),
    /// It did something to the dashboard itself: opened a prompt or a detail.
    Handled,
    /// It is not one of those keys, or there is nothing for it to act on: the keys of the
    /// screen are tried next.
    Unhandled,
}

impl From<Option<Effect>> for Acted {
    fn from(effect: Option<Effect>) -> Self {
        effect.map_or(Acted::Unhandled, Acted::Effect)
    }
}

impl App {
    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "crossterm's `KeyCode` has keys no binding uses"
    )]
    pub fn key(&mut self, key: KeyEvent) -> Option<Effect> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Some(Effect::Quit);
        }
        self.unconfirmed_allow = None;
        self.moved_back.clear();
        if matches!(
            self.status,
            Some(
                Status::Done { .. } | Status::ActionFailed { .. } | Status::TranscriptFailed { .. }
            )
        ) {
            self.status = None;
        }
        if let Some(prompt) = self.prompt.take() {
            let (prompt, effect) = self.prompt_key(prompt, key);
            self.prompt = prompt;
            return effect;
        }
        // In a diff, `v` selects lines, as in vim, and `R` asks for the verdict; `c` and `r`
        // write about the lines; `e` and `d` edit and drop a draft of the review.
        if matches!(key.code, KeyCode::Char('v' | 'c' | 'r' | 'e' | 'd')) && self.diff_has_keys() {
            return self.detail_key(key);
        }
        match self.act(key) {
            Acted::Effect(effect) => return Some(effect),
            Acted::Handled => return None,
            Acted::Unhandled => {}
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
            _ if !self.list_has_keys() => return self.detail_key(key),
            _ if self.tab == Tab::Log => self.log_key(key),
            _ if self.move_selection(key) => {}
            _ => self.list_key(key),
        }
        None
    }

    /// `H` or `L`: one step of a wide window from the split towards the whole window for
    /// `towards`, or back. `list only ←H— split —L→ detail only`
    pub(super) fn step_split(&mut self, towards: Pane) {
        if !self.is_wide() || self.panes().is_none() {
            return;
        }
        match self.split {
            Split::Both => {
                match towards {
                    Pane::List => self.keys_to_list(),
                    Pane::Detail => self.keys_to_detail(),
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

    pub(super) fn pane_with_keys(&self) -> Pane {
        if self.list_has_keys() {
            Pane::List
        } else {
            Pane::Detail
        }
    }

    /// The keys that act on what waits, on items and on sessions, for whatever is on screen.
    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "crossterm's `KeyCode` has keys no binding uses"
    )]
    pub(super) fn act(&mut self, key: KeyEvent) -> Acted {
        let subjects = self.subjects();
        let permission = subjects.iter().find_map(|entry| entry.request());
        let blocker = subjects.iter().find_map(|entry| entry.blocker());
        let review = subjects
            .iter()
            .find_map(|entry| entry.review().map(|(item, _)| item));
        match key.code {
            KeyCode::Char('a') => Acted::from(permission.map(|request| Effect::Permit {
                request,
                answer: Permission::Allow,
            })),
            KeyCode::Char('d') => Acted::from(permission.map(|request| Effect::Permit {
                request,
                answer: Permission::Deny,
            })),
            KeyCode::Char('r') => {
                let Some((item, blocker)) = blocker else {
                    return Acted::Unhandled;
                };
                self.prompt = Some(Prompt::Answer {
                    item,
                    blocker,
                    input: Input::default(),
                });
                Acted::Handled
            }
            KeyCode::Char('R') => {
                Acted::from(blocker.map(|(item, blocker)| Effect::ResolveInEditor {
                    item,
                    blocker,
                    text: String::new(),
                }))
            }
            KeyCode::Char('v') => {
                let Some(item) = review else {
                    return Acted::Unhandled;
                };
                self.prompt = Some(Prompt::Verdict { item });
                Acted::Handled
            }
            KeyCode::Char('D') => {
                let Some(item) = self.item_with_work() else {
                    return Acted::Unhandled;
                };
                self.open(Detail::Diff(item));
                Acted::Handled
            }
            KeyCode::Char('s') => {
                Acted::from(self.item_subject().map(|item| Effect::ChooseStart { item }))
            }
            KeyCode::Char('m') => {
                Acted::from(self.item_subject().map(|item| Effect::ChooseState { item }))
            }
            KeyCode::Char('e') => {
                Acted::from(self.item_subject().map(|item| Effect::Edit { item }))
            }
            KeyCode::Char('c') => {
                let Some(item) = self.item_subject() else {
                    return Acted::Unhandled;
                };
                self.prompt = Some(Prompt::Comment {
                    item,
                    input: Input::default(),
                });
                Acted::Handled
            }
            KeyCode::Char('x') => {
                let Some(session) = self.session_subject() else {
                    return Acted::Unhandled;
                };
                self.prompt = Some(Prompt::Stop { session });
                Acted::Handled
            }
            _ => Acted::Unhandled,
        }
    }

    /// The keys of the open detail.
    pub(super) fn detail_key(&mut self, key: KeyEvent) -> Option<Effect> {
        let item_session = match self
            .stack()
            .last()
            .and_then(|opened| self.in_view_of(&opened.detail))
        {
            Some(InView::Item { session, .. }) => *session,
            Some(
                InView::Session(_)
                | InView::Agent { .. }
                | InView::Flag { .. }
                | InView::Diff { .. },
            )
            | None => None,
        };
        let item_of_session = self.item_subject();
        let page = self.page();
        // A selection in a diff ends before esc goes back.
        if key.code == KeyCode::Esc
            && let Some(pane) = self.diff_on_top_mut()
            && pane.selection.is_some()
        {
            pane.selection = None;
            return None;
        }
        if matches!(key.code, KeyCode::Esc | KeyCode::Left | KeyCode::Char('h')) {
            match self.panes_mut() {
                // The detail opened from the list stays as it is, to come back to with `l`.
                Some(panes) if panes.stack.len() == 1 => panes.keys = Pane::List,
                Some(_) | None => {
                    self.stack_mut().pop();
                }
            }
            return None;
        }
        if let Some(Opened {
            detail: Detail::Diff(item),
            ..
        }) = self.stack().last()
        {
            let item = *item;
            return self.diff_key(item, key);
        }
        let opened = self.stack_mut().last_mut()?;
        if let Some(scroll) = opened.scroll.key(key, page) {
            opened.scroll = scroll;
            return None;
        }
        match (key.code, &opened.detail) {
            // Item, transcript, diff: each further right.
            (KeyCode::Enter | KeyCode::Right | KeyCode::Char('l'), Detail::Item(item)) => {
                let item = *item;
                match item_session {
                    Some(session) => self.open(Detail::Session(session)),
                    None => self.open(Detail::Diff(item)),
                }
            }
            (KeyCode::Enter | KeyCode::Right | KeyCode::Char('l'), Detail::Session(_)) => {
                if let Some(item) = item_of_session {
                    self.open(Detail::Diff(item));
                }
            }
            (KeyCode::Char('i'), Detail::Session(_)) => {
                if let Some(item) = item_of_session {
                    self.open_item_of_top(item);
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
        None
    }

    /// The keys of the diff of `item`, open on top.
    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "crossterm's `KeyCode` has keys no binding uses"
    )]
    pub(super) fn diff_key(&mut self, item: ItemId, key: KeyEvent) -> Option<Effect> {
        let page = self.page();
        match key.code {
            KeyCode::Char('i') => self.open_item_of_top(item),
            KeyCode::Char('R') => {
                if self.review_waits(item) {
                    self.prompt = Some(Prompt::Verdict { item });
                }
            }
            KeyCode::Char('p') => return Some(Effect::PageDiff { item }),
            KeyCode::Char(key @ ('c' | 'r')) => {
                let kind = if key == 'c' {
                    CommentKind::Plain
                } else {
                    CommentKind::ChangeRequest
                };
                // While a review waits, a comment is a draft of it; else it is posted at once.
                let then = if self.review_waits(item) {
                    Written::Draft { replacing: None }
                } else {
                    Written::Post
                };
                let lines = self.diffs.get(&item)?.selected_lines()?;
                self.prompt = Some(Prompt::LineComment {
                    item,
                    kind,
                    lines,
                    then,
                    input: Input::default(),
                });
            }
            KeyCode::Char('e') => {
                let pane = self.diffs.get(&item)?;
                let index = pane.draft_at_cursor()?;
                let draft = pane.drafts()[index].clone();
                self.prompt = Some(Prompt::LineComment {
                    item,
                    kind: draft.kind,
                    lines: draft.lines,
                    then: Written::Draft {
                        replacing: Some(index),
                    },
                    input: Input::new(draft.text),
                });
            }
            KeyCode::Char('d') => {
                let pane = self.diffs.get_mut(&item)?;
                let index = pane.draft_at_cursor()?;
                pane.drop_draft(index);
            }
            _ => {
                if let Some(pane) = self.diffs.get_mut(&item) {
                    pane.key(key, page);
                }
            }
        }
        None
    }

    /// Moves the selection of the list on screen as `key` asks; `false` for a key that is not
    /// one of navigation. The details opened before give way to the selection's, but only when it
    /// moved.
    pub(super) fn move_selection(&mut self, key: KeyEvent) -> bool {
        let page = self.page();
        let navigated = match self.tab {
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
            Tab::Log => Navigated::Unused,
        };
        if navigated == Navigated::Moved {
            self.stack_mut().clear();
            // A list scrolled away from the selection comes back to show it.
            if let Some(panes) = self.panes_mut() {
                panes.list_top = panes.list_top.follow();
            }
        }
        navigated != Navigated::Unused
    }

    /// The keys of the list on screen, besides those that move its selection.
    pub(super) fn list_key(&mut self, key: KeyEvent) {
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
            (Tab::Items, KeyCode::Char('O')) => {
                let index = ORDERS
                    .iter()
                    .position(|order| *order == self.items.order)
                    .map_or(0, |index| (index + 1) % ORDERS.len());
                self.items.order = ORDERS[index];
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

    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "crossterm's `KeyCode` has keys no binding uses"
    )]
    pub(super) fn log_key(&mut self, key: KeyEvent) {
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

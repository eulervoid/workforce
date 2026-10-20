//! What the mouse does: the wheel, clicks, moves and drags.

use ratatui::crossterm::event::KeyEvent;
use ratatui::layout::Position;
use wf_harness::Permission;

use super::clickable::{CONFIRM_LASTS_MS, Hovered, Request, Target, UnconfirmedAllow};
use super::effect::Effect;
use super::nav::{Opened, Pane, Picked, Wheel};
use super::prompt::{Filtered, Prompt};
use super::{App, Split, Tab};
use crate::dashboard::snapshot::Detail;

impl App {
    /// The mouse wheel turned over `column`. It scrolls what is under the pointer, the list or
    /// the detail beside it, and leaves the selection and the keys where they are; with one pane
    /// on screen, that one.
    pub fn wheel(&mut self, column: u16, wheel: Wheel) {
        if !self.mouse_passes() {
            return;
        }
        if self.tab == Tab::Log {
            match self.log.stack.last_mut() {
                Some(Opened {
                    detail: Detail::Diff(item),
                    ..
                }) => {
                    if let Some(pane) = self.diffs.get_mut(item) {
                        pane.scroll(pane.top.scroll(wheel));
                    }
                }
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
        // The selection's detail beside the list keeps a scroll only once opened; opening it
        // leaves the keys with the list.
        if self.stack().is_empty()
            && let Some(detail) = self.selected_detail()
        {
            self.open(detail);
            self.keys_to_list();
        }
        // Without a detail beside the list, there is nothing to scroll.
        if let Some(pane) = self.diff_on_top_mut() {
            pane.scroll(pane.top.scroll(wheel));
        } else if let Some(opened) = self.stack_mut().last_mut() {
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
        if let Some(pane) = self.diff_on_top_mut()
            && pane.press(position)
        {
            if in_detail {
                self.keys_to_detail();
            }
            return None;
        }
        let clickable = &self.clickable;
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
            self.keys_to_detail();
        }
        None
    }

    /// Does what a click on a link or a button does; `in_detail` when it is in the detail beside
    /// the list, `unconfirmed` the click on allow before it.
    pub(super) fn follow(
        &mut self,
        target: Target,
        in_detail: bool,
        unconfirmed: Option<UnconfirmedAllow>,
    ) -> Option<Effect> {
        match target {
            Target::Open { detail, .. } => {
                // In place of the detail it is in, so ctrl+o goes back to it.
                if in_detail {
                    self.keys_to_detail();
                }
                self.open_link(detail);
                None
            }
            Target::Diff { item, .. } => {
                if in_detail {
                    self.keys_to_detail();
                }
                self.open_link(Detail::Diff(item));
                None
            }
            Target::Key { code, .. } => {
                // A key drawn in the detail beside the list is the detail's key.
                if in_detail {
                    self.keys_to_detail();
                }
                self.key(KeyEvent::from(code))
            }
            Target::Back { depth, .. } => {
                self.stack_mut().truncate(depth);
                if depth > 0
                    && let Some(panes) = self.panes_mut()
                {
                    panes.keys = Pane::Detail;
                }
                None
            }
            Target::Allow(request) => {
                if unconfirmed.is_some_and(|unconfirmed| self.confirms(unconfirmed, request)) {
                    return Some(Effect::Permit {
                        request,
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
                request,
                answer: Permission::Deny,
            }),
        }
    }

    /// Whether a click on allow for `request` confirms `unconfirmed`: the first click was on
    /// the same request, which still waits, not longer than [`CONFIRM_LASTS_MS`] ago.
    pub(super) fn confirms(&self, unconfirmed: UnconfirmedAllow, request: Request) -> bool {
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
    pub(super) fn hovered_at(&self, position: Position) -> Option<Hovered> {
        if !self.mouse_passes() {
            return None;
        }
        self.clickable.hovered(position)
    }

    /// Selects the entry at `index` of the list on screen and gives the list the keys; opens it
    /// when it was selected already and the list had the keys, as enter would.
    pub(super) fn click_entry(&mut self, index: usize) {
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
            Some(Picked::Again) => panes.keys = Pane::List,
            // The details opened before give way to the selection's, which the list keeps on
            // screen from now on.
            Some(Picked::Moved) => {
                panes.stack.clear();
                panes.jumped_from.clear();
                panes.list_top = panes.list_top.follow();
            }
        }
    }

    /// The pointer dragged to `column`, `row` with the left button down: in a diff, it selects
    /// lines from where it was pressed.
    pub fn drag(&mut self, column: u16, row: u16) {
        if !self.mouse_passes() {
            return;
        }
        if let Some(pane) = self.diff_on_top_mut() {
            pane.drag(Position::new(column, row));
        }
    }

    /// Gives the keys to the list, leaving the details opened from it as they are.
    pub(super) fn keys_to_list(&mut self) {
        if let Some(panes) = self.panes_mut() {
            panes.keys = Pane::List;
        }
    }

    /// Gives the keys to the detail on screen beside the list: the one opened, else the
    /// selection's, if it has one.
    pub(super) fn keys_to_detail(&mut self) {
        if !self.list_has_keys() {
            return;
        }
        let Some(panes) = self.panes_mut() else {
            return;
        };
        if !panes.stack.is_empty() {
            panes.keys = Pane::Detail;
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
                | Prompt::LineComment { .. }
                | Prompt::Choose(_)
                | Prompt::Stop { .. }
                | Prompt::Keys,
            ) => false,
        }
    }
}

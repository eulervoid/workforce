//! The keys as the dashboard tells them: in the footer, in the keys popup, and as the buttons a
//! click presses.

use ratatui::crossterm::event::KeyCode;

use super::app::{Request, Target};

/// A key and what it does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hint {
    /// The key as written: `esc`, `j/k`.
    pub label: &'static str,
    /// The key a click presses; `None` for a hint of more than one key, or of a mouse action.
    pub code: Option<KeyCode>,
    pub what: &'static str,
}

impl Hint {
    const fn key(label: &'static str, code: KeyCode, what: &'static str) -> Self {
        Self {
            label,
            code: Some(code),
            what,
        }
    }

    /// A hint of keys together, which no click presses.
    const fn keys(label: &'static str, what: &'static str) -> Self {
        Self {
            label,
            code: None,
            what,
        }
    }

    /// The hint as the footer says it.
    pub fn text(&self) -> String {
        format!("{} {}", self.label, self.what)
    }

    /// What a click on the hint does: what the key does, but for allow and deny, which answer
    /// `request`, the permission request the keys answer; allow asks for a second click. `None`
    /// for a hint no click presses.
    pub fn target(&self, request: Option<Request>) -> Option<Target> {
        if *self == ALLOW {
            return request.map(Target::Allow);
        }
        if *self == DENY {
            return request.map(Target::Deny);
        }
        Some(Target::Key {
            code: self.code?,
            key: self.label,
            what: self.what,
        })
    }
}

pub const ALLOW: Hint = Hint::key("a", KeyCode::Char('a'), "allow");
pub const DENY: Hint = Hint::key("d", KeyCode::Char('d'), "deny");
pub const ANSWER: Hint = Hint::key("r", KeyCode::Char('r'), "answer");
pub const ANSWER_IN_EDITOR: Hint = Hint::key("R", KeyCode::Char('R'), "in editor");
pub const REVIEW: Hint = Hint::key("v", KeyCode::Char('v'), "review");
pub const REVIEW_IN_DIFF: Hint = Hint::key("R", KeyCode::Char('R'), "review");
pub const DIFF: Hint = Hint::key("D", KeyCode::Char('D'), "diff");

pub const BACK: Hint = Hint::key("esc", KeyCode::Esc, "back");
pub const JUMP_BACK: Hint = Hint::keys("ctrl+o", "back from link");
pub const UNSELECT: Hint = Hint::key("esc", KeyCode::Esc, "unselect");
pub const CLEAR_FILTER: Hint = Hint::key("esc", KeyCode::Esc, "clear");
pub const OPEN: Hint = Hint::key("enter", KeyCode::Enter, "open");
pub const OPEN_SESSION: Hint = Hint::key("enter", KeyCode::Enter, "session");
pub const FOLD: Hint = Hint::key("enter", KeyCode::Enter, "fold");
pub const SCROLL: Hint = Hint::keys("j/k", "scroll");
pub const MOVE: Hint = Hint::keys("j/k", "move");
pub const EXTEND: Hint = Hint::keys("j/k", "extend");
pub const FOLLOW: Hint = Hint::key("G", KeyCode::Char('G'), "follow");
pub const ITEM: Hint = Hint::key("i", KeyCode::Char('i'), "item");
pub const NEXT_SESSION: Hint = Hint::key("n", KeyCode::Char('n'), "next session");
pub const FULL: Hint = Hint::key("f", KeyCode::Char('f'), "full");
pub const OPEN_DIFF: Hint = Hint::key("l", KeyCode::Char('l'), "diff");
pub const SELECT: Hint = Hint::key("v", KeyCode::Char('v'), "select");
pub const COMMENT: Hint = Hint::key("c", KeyCode::Char('c'), "comment");
pub const CHANGE: Hint = Hint::key("r", KeyCode::Char('r'), "change");
pub const EDIT: Hint = Hint::key("e", KeyCode::Char('e'), "edit");
pub const DROP: Hint = Hint::key("d", KeyCode::Char('d'), "drop");
pub const FILE: Hint = Hint::keys("]/[", "file");
pub const PAGER: Hint = Hint::key("p", KeyCode::Char('p'), "pager");
pub const GROUP: Hint = Hint::key("o", KeyCode::Char('o'), "group");
pub const FILTER: Hint = Hint::key("/", KeyCode::Char('/'), "filter");
pub const CHANGE_FILTER: Hint = Hint::key("/", KeyCode::Char('/'), "change filter");
pub const READINESS: Hint = Hint::key("F", KeyCode::Char('F'), "ready");
pub const KEYS: Hint = Hint::key("?", KeyCode::Char('?'), "keys");

/// Where keys work, as the keys popup puts them together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    Everywhere,
    /// What waits on the principal: a permission request, a blocker, a review.
    Waiting,
    /// The list of the Items tab.
    Items,
    /// An item, open or selected.
    Item,
    EditMenu,
    Transcript,
    Diff,
}

/// The keys of a place, under its title.
pub struct Section {
    pub place: Place,
    pub title: &'static str,
    /// A row without a label goes on with the one before.
    pub hints: &'static [Hint],
}

/// Every key, by where it works, for the keys popup.
pub const HELP: &[Section] = &[
    Section {
        place: Place::Everywhere,
        title: "everywhere",
        hints: &[
            Hint::keys("1-4 tab", "switch tabs, also shift-tab"),
            Hint::keys("j/k g/G", "move, top, end (and follow)"),
            Hint::keys("pgup/pgdn", "a page up, down"),
            Hint::keys("space", "a page down in a text"),
            Hint::keys("enter esc", "open, go back"),
            Hint::keys("H L", "one pane or both, also shift ←/→"),
            Hint::keys("wheel", "move what is under the pointer"),
            Hint::keys("click", "pick a tab or an entry, again to open"),
            Hint::keys("", "follow a link, a key hint, a crumb"),
            Hint::keys("shift-drag", "select text, as without the dashboard"),
            Hint::keys("ctrl+o", "back to where a link was followed"),
            Hint::keys("q", "quit"),
            Hint::keys("tmux", "focus-events on: dims when unfocused"),
        ],
    },
    Section {
        place: Place::Waiting,
        title: "what waits",
        hints: &[
            Hint::keys("a d", "allow, deny a permission request"),
            Hint::keys("", "a click on allow asks for a second"),
            Hint::keys("r R", "answer a blocker here, in $EDITOR"),
            Hint::keys("ctrl+g", "carry a comment or answer to $EDITOR"),
            Hint::keys("v", "review the work"),
        ],
    },
    Section {
        place: Place::Items,
        title: "the items list",
        hints: &[
            Hint::keys("o O", "grouping, order"),
            Hint::keys("/ F", "filter by title or id, by readiness"),
            Hint::keys("esc", "clear the filter"),
        ],
    },
    Section {
        place: Place::Item,
        title: "an item",
        hints: &[
            Hint::keys("s x", "start a session, stop one"),
            Hint::keys("c", "comment"),
            Hint::keys("e", "title and body in $EDITOR"),
            Hint::keys("m p", "move, priority"),
            Hint::keys("E", "the edit menu: every field"),
            Hint::keys("D", "the diff"),
        ],
    },
    Section {
        place: Place::EditMenu,
        title: "the edit menu",
        hints: &[
            Hint::keys("j/k enter", "pick a field"),
            Hint::keys("1-8 click", "pick it straight"),
            Hint::keys("esc", "close"),
            Hint::keys("labels", "typing filters, ↑/↓ move, space marks"),
            Hint::keys("", "enter adds what is typed, else applies"),
        ],
    },
    Section {
        place: Place::Transcript,
        title: "a transcript",
        hints: &[
            Hint::keys("enter f", "open or close one entry, all"),
            Hint::keys("i l", "item, diff"),
            Hint::keys("n", "an ended session: its item's next"),
        ],
    },
    Section {
        place: Place::Diff,
        title: "a diff",
        hints: &[
            Hint::keys("] [ enter", "next, previous file, fold one"),
            Hint::keys("v drag", "select lines, as vim's visual line"),
            Hint::keys("c r", "comment, request a change on them"),
            Hint::keys("e d", "edit, drop a comment of the review"),
            Hint::keys("R", "review, as v does elsewhere"),
            Hint::keys("p", "in git's pager, as `wf item diff`"),
            Hint::keys("i", "the item"),
        ],
    },
];

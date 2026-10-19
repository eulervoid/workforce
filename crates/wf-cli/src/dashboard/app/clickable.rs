//! What drawing shows of where a click can land, kept for the clicks and moves that follow.

use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Position, Rect};
use wf_core::id::{ItemId, SessionId};

use super::Tab;
use super::nav::Scroll;
use crate::dashboard::snapshot::Detail;

/// What drawing showed: where the text with the keys and the list ended up, and what a click can
/// land on.
#[derive(Clone, Debug, Default)]
pub struct Drawn {
    pub text: Option<Scroll>,
    /// Where the rows of a diff were drawn, the first row on screen and the row of its cursor,
    /// where a diff is.
    pub diff: Option<(Rect, usize, usize)>,
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
    /// Opens the diff of an item, as `D` does, from one of its commits.
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
                detail: Detail::Diff(_),
                name,
            } => format!("open the diff of {name}"),
            Target::Open {
                detail: Detail::Item(_) | Detail::Agent(_) | Detail::Flag(_),
                name,
            } => format!("open {name}"),
            Target::Diff { commit, .. } => {
                let short = wf_core::session::short_commit(commit);
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

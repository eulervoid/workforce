//! The styles of the CLI and the dashboard, by what they mean: the one place that says how
//! workforce looks. The dashboard converts them to ratatui styles.

use anstyle::{AnsiColor, Color, RgbColor, Style};
use wf_core::graph::Readiness;
use wf_core::item::{ItemState, Priority};

use crate::view::ActorKind;

const fn fg(color: AnsiColor) -> Style {
    Style::new().fg_color(Some(Color::Ansi(color)))
}

pub const BOLD: Style = Style::new().bold();
pub const DIM: Style = Style::new().dimmed();
/// Rules and pane separators, and the frames of popups.
pub const DIVIDER: Style = Style::new().dimmed();
/// What the pointer is over, in the dashboard.
pub const LINK: Style = Style::new().underline();
/// The label of the tab on screen.
pub const TAB_ACTIVE: Style = Style::new().bold().invert();
pub const ID: Style = fg(AnsiColor::Cyan);
pub const PRINCIPAL: Style = fg(AnsiColor::Magenta);
pub const AGENT: Style = fg(AnsiColor::BrightBlue);
pub const DAEMON: Style = fg(AnsiColor::Green);
/// Something has gone wrong and needs the principal.
pub const WARNING: Style = fg(AnsiColor::Red).bold();
/// An action or a refresh that failed.
pub const ERROR: Style = fg(AnsiColor::Red);
/// An action that went through.
pub const SUCCESS: Style = fg(AnsiColor::Green);
/// Something that waits on the principal, or soon will.
pub const ATTENTION: Style = fg(AnsiColor::Yellow);
/// A key to press.
pub const KEY: Style = fg(AnsiColor::Yellow);
/// A live session, and the load of an agent while it has some.
pub const LIVE: Style = fg(AnsiColor::Blue);
/// Inline code: blue, as Claude Code shows it, so it looks familiar.
pub const CODE: Style = fg(AnsiColor::Blue);
/// Lines a diff adds, in transcripts.
pub const DIFF_ADDED: Style = fg(AnsiColor::Green);
/// Lines a diff removes, in transcripts.
pub const DIFF_REMOVED: Style = fg(AnsiColor::Red);

/// Marine blue: marks the one place that has the keys.
pub const ACCENT: Color = Color::Rgb(RgbColor(30, 80, 160));
/// The accent, dimmed: marks a place that had the keys but has not now, or every place while the
/// terminal pane is not focused.
pub const ACCENT_DIMMED: Color = Color::Rgb(RgbColor(30, 45, 75));
/// Text on the full accent.
pub const ON_ACCENT: Style = fg(AnsiColor::BrightWhite);

pub fn actor(kind: ActorKind) -> Style {
    match kind {
        ActorKind::Principal => PRINCIPAL,
        ActorKind::Agent => AGENT,
        ActorKind::Daemon => DAEMON,
    }
}

pub fn state(state: ItemState) -> Style {
    match state {
        ItemState::Open => fg(AnsiColor::Green),
        ItemState::Active => fg(AnsiColor::Blue),
        ItemState::Review => fg(AnsiColor::Yellow),
        ItemState::Done => fg(AnsiColor::Magenta),
        ItemState::Cancelled => fg(AnsiColor::BrightBlack),
        ItemState::Draft => DIM,
    }
}

/// How an item stands among the others: ready and claimed look like the states they lead to.
pub fn readiness(readiness: Readiness) -> Style {
    match readiness {
        Readiness::Ready => state(ItemState::Open),
        Readiness::Claimed => state(ItemState::Active),
        Readiness::Blocked => fg(AnsiColor::Red),
        Readiness::Stranded => WARNING,
    }
}

pub fn priority(priority: Priority) -> Style {
    match priority {
        Priority::P0 => WARNING,
        Priority::P1 => fg(AnsiColor::Yellow),
        Priority::P2 => Style::new(),
        Priority::P3 => DIM,
    }
}

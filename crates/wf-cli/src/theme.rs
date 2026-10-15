//! The styles of the CLI and the dashboard, by what they mean: the one place that says how
//! workforce looks. The dashboard converts them to ratatui styles.
//!
//! The theme is set once at startup with [`set`]; until then, and in tests, it is the built-in
//! one.

use std::sync::OnceLock;

use anstyle::{AnsiColor, Color, RgbColor, Style};
use wf_core::graph::Readiness;
use wf_core::item::{ItemState, Priority};

use crate::view::ActorKind;

/// Every style wf draws with.
#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    text_dimmed: Style,
    text_bold: Style,
    divider: Style,
    link: Style,
    accent: Color,
    accent_dimmed: Color,
    on_accent: Style,
    tab_active: Style,
    key: Style,
    id: Style,
    principal: Style,
    agent: Style,
    daemon: Style,
    success: Style,
    error: Style,
    warning: Style,
    attention: Style,
    live: Style,
    code: Style,
    diff_added: Style,
    diff_removed: Style,
    state_draft: Style,
    state_open: Style,
    state_active: Style,
    state_review: Style,
    state_done: Style,
    state_cancelled: Style,
    readiness_blocked: Style,
    readiness_stranded: Style,
    priority_p0: Style,
    priority_p1: Style,
    priority_p2: Style,
    priority_p3: Style,
}

const fn fg(color: AnsiColor) -> Style {
    Style::new().fg_color(Some(Color::Ansi(color)))
}

impl Default for Theme {
    fn default() -> Self {
        let warning = fg(AnsiColor::Red).bold();
        Self {
            text_dimmed: Style::new().dimmed(),
            text_bold: Style::new().bold(),
            divider: Style::new().dimmed(),
            link: Style::new().underline(),
            accent: Color::Rgb(RgbColor(30, 80, 160)),
            accent_dimmed: Color::Rgb(RgbColor(30, 45, 75)),
            on_accent: fg(AnsiColor::BrightWhite),
            tab_active: Style::new().bold().invert(),
            key: fg(AnsiColor::Yellow),
            id: fg(AnsiColor::Cyan),
            principal: fg(AnsiColor::Magenta),
            agent: fg(AnsiColor::BrightBlue),
            daemon: fg(AnsiColor::Green),
            success: fg(AnsiColor::Green),
            error: fg(AnsiColor::Red),
            warning,
            attention: fg(AnsiColor::Yellow),
            live: fg(AnsiColor::Blue),
            code: fg(AnsiColor::Blue),
            diff_added: fg(AnsiColor::Green),
            diff_removed: fg(AnsiColor::Red),
            state_draft: Style::new().dimmed(),
            state_open: fg(AnsiColor::Green),
            state_active: fg(AnsiColor::Blue),
            state_review: fg(AnsiColor::Yellow),
            state_done: fg(AnsiColor::Magenta),
            state_cancelled: fg(AnsiColor::BrightBlack),
            readiness_blocked: fg(AnsiColor::Red),
            readiness_stranded: warning,
            priority_p0: warning,
            priority_p1: fg(AnsiColor::Yellow),
            priority_p2: Style::new(),
            priority_p3: Style::new().dimmed(),
        }
    }
}

static THEME: OnceLock<Theme> = OnceLock::new();

/// Sets the theme for the rest of the run; only the first call counts.
pub fn set(theme: Theme) {
    let _ = THEME.set(theme);
}

fn get() -> &'static Theme {
    THEME.get_or_init(Theme::default)
}

/// Secondary text: types, counts, hints, timestamps.
pub fn text_dimmed() -> Style {
    get().text_dimmed
}

/// Names, titles and headings.
pub fn text_bold() -> Style {
    get().text_bold
}

/// Rules and pane separators, and the frames of popups.
pub fn divider() -> Style {
    get().divider
}

/// What the pointer is over, in the dashboard.
pub fn link() -> Style {
    get().link
}

/// Marks the one place that has the keys.
pub fn accent() -> Color {
    get().accent
}

/// The accent, dimmed: marks a place that had the keys but has not now, or every place while the
/// terminal pane is not focused.
pub fn accent_dimmed() -> Color {
    get().accent_dimmed
}

/// Text on the full accent.
pub fn on_accent() -> Style {
    get().on_accent
}

/// The label of the tab on screen.
pub fn tab_active() -> Style {
    get().tab_active
}

/// A key to press.
pub fn key() -> Style {
    get().key
}

pub fn id() -> Style {
    get().id
}

pub fn principal() -> Style {
    get().principal
}

pub fn agent() -> Style {
    get().agent
}

pub fn daemon() -> Style {
    get().daemon
}

/// An action that went through.
pub fn success() -> Style {
    get().success
}

/// An action or a refresh that failed.
pub fn error() -> Style {
    get().error
}

/// Something has gone wrong and needs the principal.
pub fn warning() -> Style {
    get().warning
}

/// Something that waits on the principal, or soon will.
pub fn attention() -> Style {
    get().attention
}

/// A live session, and the load of an agent while it has some.
pub fn live() -> Style {
    get().live
}

/// Inline code: blue by default, as Claude Code shows it, so it looks familiar.
pub fn code() -> Style {
    get().code
}

/// Lines a diff adds, in transcripts.
pub fn diff_added() -> Style {
    get().diff_added
}

/// Lines a diff removes, in transcripts.
pub fn diff_removed() -> Style {
    get().diff_removed
}

pub fn actor(kind: ActorKind) -> Style {
    match kind {
        ActorKind::Principal => principal(),
        ActorKind::Agent => agent(),
        ActorKind::Daemon => daemon(),
    }
}

pub fn state(state: ItemState) -> Style {
    let theme = get();
    match state {
        ItemState::Draft => theme.state_draft,
        ItemState::Open => theme.state_open,
        ItemState::Active => theme.state_active,
        ItemState::Review => theme.state_review,
        ItemState::Done => theme.state_done,
        ItemState::Cancelled => theme.state_cancelled,
    }
}

/// How an item stands among the others: ready and claimed look like the states they lead to.
pub fn readiness(readiness: Readiness) -> Style {
    match readiness {
        Readiness::Ready => state(ItemState::Open),
        Readiness::Claimed => state(ItemState::Active),
        Readiness::Blocked => get().readiness_blocked,
        Readiness::Stranded => get().readiness_stranded,
    }
}

pub fn priority(priority: Priority) -> Style {
    let theme = get();
    match priority {
        Priority::P0 => theme.priority_p0,
        Priority::P1 => theme.priority_p1,
        Priority::P2 => theme.priority_p2,
        Priority::P3 => theme.priority_p3,
    }
}

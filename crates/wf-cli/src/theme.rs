//! The styles of the CLI and the dashboard, by what they mean: the one place that says how
//! workforce looks. The dashboard converts them to ratatui styles.
//!
//! The theme is set once at startup with [`set`]; until then, and in tests, it is the built-in
//! one.

use std::sync::OnceLock;

use anstyle::{AnsiColor, Color, RgbColor, Style};
use wf_core::graph::Readiness;
use wf_core::item::{ItemState, Priority};

use wf_daemon::machine_config::{Background, MachineConfig, ThemeConfig};

use crate::view::ActorKind;

/// Every style wf draws with.
#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    background: Background,
    text: Style,
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
            background: Background::Terminal,
            text: Style::new(),
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

impl Theme {
    /// The built-in theme, with what `config` sets in place of its styles.
    pub fn new(config: &ThemeConfig) -> Self {
        let default = Self::default();
        Self {
            background: config.background,
            text: config.text.unwrap_or(default.text),
            text_dimmed: config.text_dimmed.unwrap_or(default.text_dimmed),
            text_bold: config.text_bold.unwrap_or(default.text_bold),
            divider: config.divider.unwrap_or(default.divider),
            link: config.link.unwrap_or(default.link),
            accent: config.accent.unwrap_or(default.accent),
            accent_dimmed: config.accent_dimmed.unwrap_or(default.accent_dimmed),
            on_accent: config.on_accent.unwrap_or(default.on_accent),
            tab_active: config.tab_active.unwrap_or(default.tab_active),
            key: config.key.unwrap_or(default.key),
            id: config.id.unwrap_or(default.id),
            principal: config.principal.unwrap_or(default.principal),
            agent: config.agent.unwrap_or(default.agent),
            daemon: config.daemon.unwrap_or(default.daemon),
            success: config.success.unwrap_or(default.success),
            error: config.error.unwrap_or(default.error),
            warning: config.warning.unwrap_or(default.warning),
            attention: config.attention.unwrap_or(default.attention),
            live: config.live.unwrap_or(default.live),
            code: config.code.unwrap_or(default.code),
            diff_added: config.diff_added.unwrap_or(default.diff_added),
            diff_removed: config.diff_removed.unwrap_or(default.diff_removed),
            state_draft: config.state.draft.unwrap_or(default.state_draft),
            state_open: config.state.open.unwrap_or(default.state_open),
            state_active: config.state.active.unwrap_or(default.state_active),
            state_review: config.state.review.unwrap_or(default.state_review),
            state_done: config.state.done.unwrap_or(default.state_done),
            state_cancelled: config.state.cancelled.unwrap_or(default.state_cancelled),
            readiness_blocked: config
                .readiness
                .blocked
                .unwrap_or(default.readiness_blocked),
            readiness_stranded: config
                .readiness
                .stranded
                .unwrap_or(default.readiness_stranded),
            priority_p0: config.priority.p0.unwrap_or(default.priority_p0),
            priority_p1: config.priority.p1.unwrap_or(default.priority_p1),
            priority_p2: config.priority.p2.unwrap_or(default.priority_p2),
            priority_p3: config.priority.p3.unwrap_or(default.priority_p3),
        }
    }
}

static THEME: OnceLock<Theme> = OnceLock::new();

/// Sets the theme from `[theme]` of the machine config. A broken one must not keep wf from
/// running: it warns and leaves the built-in theme.
pub fn load() {
    let Some(path) = MachineConfig::path_from_env() else {
        return;
    };
    match ThemeConfig::load(&path) {
        Ok(config) => set(Theme::new(&config)),
        Err(error) => eprintln!("warning: {error}\nwarning: using the built-in theme"),
    }
}

/// Sets the theme for the rest of the run; only the first call counts.
pub fn set(theme: Theme) {
    let _ = THEME.set(theme);
}

fn get() -> &'static Theme {
    THEME.get_or_init(Theme::default)
}

/// Behind every cell of the dashboard.
pub fn background() -> Background {
    get().background
}

/// Text with no other style, in the dashboard.
pub fn text() -> Style {
    get().text
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

#[cfg(test)]
mod tests {
    use wf_daemon::machine_config::{PriorityThemeConfig, ReadinessThemeConfig, StateThemeConfig};

    use super::*;

    #[test]
    fn what_the_config_sets_replaces_the_built_in_style_and_the_rest_stays() {
        let background = Color::Rgb(RgbColor(0x1b, 0x1d, 0x23));
        let accent = Color::Rgb(RgbColor(0x1e, 0x50, 0xa0));
        let config = ThemeConfig {
            background: Background::Color(background),
            text_dimmed: Some(fg(AnsiColor::BrightBlack)),
            accent: Some(accent),
            state: StateThemeConfig {
                review: Some(fg(AnsiColor::Yellow).bold()),
                ..StateThemeConfig::default()
            },
            readiness: ReadinessThemeConfig {
                blocked: Some(fg(AnsiColor::Magenta)),
                ..ReadinessThemeConfig::default()
            },
            priority: PriorityThemeConfig {
                p1: Some(fg(AnsiColor::Yellow).bold()),
                ..PriorityThemeConfig::default()
            },
            ..ThemeConfig::default()
        };
        let theme = Theme::new(&config);
        let default = Theme::default();
        assert_eq!(theme.background, Background::Color(background));
        assert_eq!(theme.text_dimmed, fg(AnsiColor::BrightBlack));
        assert_eq!(theme.accent, accent);
        assert_eq!(theme.state_review, fg(AnsiColor::Yellow).bold());
        assert_eq!(theme.readiness_blocked, fg(AnsiColor::Magenta));
        assert_eq!(theme.priority_p1, fg(AnsiColor::Yellow).bold());
        // Everything else keeps the built-in style, the neighbours of each key set included.
        assert_eq!(
            Theme {
                background: default.background,
                text_dimmed: default.text_dimmed,
                accent: default.accent,
                state_review: default.state_review,
                readiness_blocked: default.readiness_blocked,
                priority_p1: default.priority_p1,
                ..theme
            },
            default
        );
    }

    #[test]
    fn an_empty_config_is_the_built_in_theme() {
        assert_eq!(Theme::new(&ThemeConfig::default()), Theme::default());
    }
}

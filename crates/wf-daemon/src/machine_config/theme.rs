//! `[theme]`: the colours of the CLI and the dashboard on this machine. The daemon never reads
//! it; the CLI builds its theme from it.
//!
//! Values use git's color syntax, such as `"bold red"`, `"#1e50a0"` or `"brightblue dim"`.

use anstyle::{Color, Style};
use serde::{Deserialize, Deserializer};

/// What `[theme]` sets; what it leaves out keeps the built-in style.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThemeConfig {
    /// Behind every cell of the dashboard.
    #[serde(default)]
    pub background: Background,
    /// Text with no other style, in the dashboard.
    #[serde(default, deserialize_with = "optional_style")]
    pub text: Option<Style>,
    #[serde(default, deserialize_with = "optional_style")]
    pub text_dimmed: Option<Style>,
    #[serde(default, deserialize_with = "optional_style")]
    pub text_bold: Option<Style>,
    #[serde(default, deserialize_with = "optional_style")]
    pub divider: Option<Style>,
    #[serde(default, deserialize_with = "optional_style")]
    pub link: Option<Style>,
    #[serde(default, deserialize_with = "optional_color")]
    pub accent: Option<Color>,
    #[serde(default, deserialize_with = "optional_color")]
    pub accent_dimmed: Option<Color>,
    #[serde(default, deserialize_with = "optional_style")]
    pub on_accent: Option<Style>,
    #[serde(default, deserialize_with = "optional_style")]
    pub tab_active: Option<Style>,
    #[serde(default, deserialize_with = "optional_style")]
    pub key: Option<Style>,
    #[serde(default, deserialize_with = "optional_style")]
    pub id: Option<Style>,
    #[serde(default, deserialize_with = "optional_style")]
    pub principal: Option<Style>,
    #[serde(default, deserialize_with = "optional_style")]
    pub agent: Option<Style>,
    #[serde(default, deserialize_with = "optional_style")]
    pub daemon: Option<Style>,
    #[serde(default, deserialize_with = "optional_style")]
    pub success: Option<Style>,
    #[serde(default, deserialize_with = "optional_style")]
    pub error: Option<Style>,
    #[serde(default, deserialize_with = "optional_style")]
    pub warning: Option<Style>,
    #[serde(default, deserialize_with = "optional_style")]
    pub attention: Option<Style>,
    #[serde(default, deserialize_with = "optional_style")]
    pub live: Option<Style>,
    #[serde(default, deserialize_with = "optional_style")]
    pub code: Option<Style>,
    #[serde(default, deserialize_with = "optional_style")]
    pub diff_added: Option<Style>,
    #[serde(default, deserialize_with = "optional_style")]
    pub diff_removed: Option<Style>,
    #[serde(default)]
    pub state: StateThemeConfig,
    #[serde(default)]
    pub readiness: ReadinessThemeConfig,
    #[serde(default)]
    pub priority: PriorityThemeConfig,
}

/// `[theme.state]`: an item in each state.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateThemeConfig {
    #[serde(default, deserialize_with = "optional_style")]
    pub draft: Option<Style>,
    #[serde(default, deserialize_with = "optional_style")]
    pub open: Option<Style>,
    #[serde(default, deserialize_with = "optional_style")]
    pub active: Option<Style>,
    #[serde(default, deserialize_with = "optional_style")]
    pub review: Option<Style>,
    #[serde(default, deserialize_with = "optional_style")]
    pub done: Option<Style>,
    #[serde(default, deserialize_with = "optional_style")]
    pub cancelled: Option<Style>,
}

/// `[theme.readiness]`: ready and claimed look like the states `open` and `active`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadinessThemeConfig {
    #[serde(default, deserialize_with = "optional_style")]
    pub blocked: Option<Style>,
    #[serde(default, deserialize_with = "optional_style")]
    pub stranded: Option<Style>,
}

/// `[theme.priority]`
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriorityThemeConfig {
    #[serde(default, deserialize_with = "optional_style")]
    pub p0: Option<Style>,
    #[serde(default, deserialize_with = "optional_style")]
    pub p1: Option<Style>,
    #[serde(default, deserialize_with = "optional_style")]
    pub p2: Option<Style>,
    #[serde(default, deserialize_with = "optional_style")]
    pub p3: Option<Style>,
}

/// What the dashboard paints behind every cell.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Background {
    /// `"none"`: nothing, so the terminal's own background shows, transparent or an image.
    #[default]
    Terminal,
    Color(Color),
}

impl<'de> Deserialize<'de> for Background {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        if text == "none" {
            return Ok(Background::Terminal);
        }
        color(&text)
            .map(Background::Color)
            .map_err(serde::de::Error::custom)
    }
}

/// The colours git calls `bright…`, which `anstyle_git` leaves out, by their number in the
/// terminal's palette.
const BRIGHT: [(&str, &str); 8] = [
    ("brightblack", "8"),
    ("brightred", "9"),
    ("brightgreen", "10"),
    ("brightyellow", "11"),
    ("brightblue", "12"),
    ("brightmagenta", "13"),
    ("brightcyan", "14"),
    ("brightwhite", "15"),
];

fn style(text: &str) -> Result<Style, String> {
    let words: Vec<&str> = text
        .split_whitespace()
        .map(|word| {
            BRIGHT
                .iter()
                .find(|(name, _)| *name == word)
                .map_or(word, |(_, number)| number)
        })
        .collect();
    let parsed = anstyle_git::parse(&words.join(" "))
        .map_err(|_| format!("`{text}` is not a style in git's color syntax"))?;
    // The first 16 colours of the palette are the ANSI colours, by their names.
    let ansi = |color: Option<Color>| match color {
        Some(Color::Ansi256(color)) => color.into_ansi().map(Color::Ansi).or(Some(color.into())),
        other => other,
    };
    Ok(parsed
        .fg_color(ansi(parsed.get_fg_color()))
        .bg_color(ansi(parsed.get_bg_color())))
}

/// A style that is one colour and nothing else, such as `"#1e50a0"` or `"blue"`.
fn color(text: &str) -> Result<Color, String> {
    let parsed = style(text)?;
    match parsed.get_fg_color() {
        Some(color) if parsed == Style::new().fg_color(Some(color)) => Ok(color),
        Some(_) | None => Err(format!("`{text}` is not a single colour")),
    }
}

fn optional_style<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Style>, D::Error> {
    let text = String::deserialize(deserializer)?;
    style(&text).map(Some).map_err(serde::de::Error::custom)
}

fn optional_color<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Color>, D::Error> {
    let text = String::deserialize(deserializer)?;
    color(&text).map(Some).map_err(serde::de::Error::custom)
}

#[cfg(test)]
mod tests {
    use anstyle::{AnsiColor, RgbColor};

    use super::*;

    fn parse(text: &str) -> Result<ThemeConfig, toml::de::Error> {
        toml::from_str(text)
    }

    #[test]
    fn an_empty_theme_sets_nothing() {
        assert_eq!(parse("").unwrap(), ThemeConfig::default());
        assert_eq!(parse("").unwrap().background, Background::Terminal);
    }

    #[test]
    fn values_are_in_gits_color_syntax() {
        let theme = parse(
            r##"
            background = "#1b1d23"
            text_dimmed = "brightblack"
            accent = "#1e50a0"
            state.review = "bold yellow"
            priority.p1 = "red blue"
            "##,
        )
        .unwrap();
        assert_eq!(
            theme.background,
            Background::Color(Color::Rgb(RgbColor(0x1b, 0x1d, 0x23)))
        );
        assert_eq!(
            theme.text_dimmed,
            Some(Style::new().fg_color(Some(AnsiColor::BrightBlack.into())))
        );
        assert_eq!(theme.accent, Some(Color::Rgb(RgbColor(0x1e, 0x50, 0xa0))));
        assert_eq!(
            theme.state.review,
            Some(Style::new().fg_color(Some(AnsiColor::Yellow.into())).bold())
        );
        assert_eq!(
            theme.priority.p1,
            Some(
                Style::new()
                    .fg_color(Some(AnsiColor::Red.into()))
                    .bg_color(Some(AnsiColor::Blue.into()))
            )
        );
    }

    #[test]
    fn none_is_the_terminals_background() {
        assert_eq!(
            parse("background = \"none\"").unwrap().background,
            Background::Terminal
        );
    }

    #[test]
    fn errors_name_the_key() {
        let error = parse("[state]\nreview = \"yelow\"")
            .unwrap_err()
            .to_string();
        assert!(error.contains("review"), "{error}");
        assert!(
            error.contains("`yelow` is not a style in git's color syntax"),
            "{error}"
        );

        let bright = parse("agent = \"brightblue dim\"").unwrap().agent;
        assert_eq!(
            bright,
            Some(
                Style::new()
                    .fg_color(Some(AnsiColor::BrightBlue.into()))
                    .dimmed()
            )
        );

        let error = parse("accent = \"bold blue\"").unwrap_err().to_string();
        assert!(error.contains("accent"), "{error}");
        assert!(error.contains("not a single colour"), "{error}");

        let error = parse("colour = \"red\"").unwrap_err().to_string();
        assert!(error.contains("colour"), "{error}");
    }
}

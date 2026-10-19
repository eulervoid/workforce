use std::io::IsTerminal;

use crate::markdown;
use crate::theme;
use crate::transcript::Format;
use crate::view::ActorView;
use wf_core::id::{HashId, ItemId};

pub(crate) fn paint(style: anstyle::Style, text: impl std::fmt::Display) -> String {
    format!("{style}{text}{style:#}")
}

pub(super) fn paint_actor(actor: &ActorView) -> String {
    paint(theme::actor(actor.kind), &actor.label)
}

/// `2026-10-09 21:42` in the local time zone.
pub(super) fn short_time(timestamp: jiff::Timestamp) -> String {
    local_time(timestamp, "%Y-%m-%d %H:%M")
}

/// A timestamp of a view, in the time zone of this machine and the given format.
pub(super) fn local_time(timestamp: jiff::Timestamp, format: &str) -> String {
    timestamp
        .to_zoned(jiff::tz::TimeZone::system())
        .strftime(format)
        .to_string()
}

pub(super) fn id_label(id: ItemId) -> String {
    paint(theme::id(), format!("#{}", id.short()))
}

/// The width prose is wrapped to: the terminal's, but not so wide that lines are hard to read.
/// `None` when output is not a terminal, so lines read by a program are never broken.
pub(super) fn text_width() -> Option<usize> {
    terminal_size::terminal_size_of(std::io::stdout())
        .map(|(width, _)| usize::from(width.0).clamp(40, 100))
}

/// `line` wrapped to `width` at spaces, continuing under `indent`; unchanged without a width.
/// Words longer than the width are left whole rather than broken.
pub(super) fn fill(line: &str, width: Option<usize>, indent: &str) -> String {
    match width {
        Some(width) => {
            let options = textwrap::Options::new(width)
                .break_words(false)
                .word_separator(textwrap::WordSeparator::AsciiSpace)
                .word_splitter(textwrap::WordSplitter::NoHyphenation)
                .subsequent_indent(indent);
            textwrap::fill(line, options)
        }
        None => line.to_string(),
    }
}

/// Rendered markdown at a terminal; piped, the markdown as written, for the agent or script
/// reading it.
pub(super) fn text_format() -> Format {
    if std::io::stdout().is_terminal() {
        Format::Styled
    } else {
        Format::Raw
    }
}

/// Markdown `text` with each line indented by `indent`: rendered and wrapped to `width` if
/// styled, as written if raw. No newline at the end.
pub(super) fn markdown_text(
    text: &str,
    width: Option<usize>,
    indent: &str,
    format: Format,
) -> String {
    match format {
        Format::Styled => markdown::render(text, width, indent)
            .trim_end_matches('\n')
            .to_string(),
        Format::Raw => indented(text, indent),
    }
}

/// `text` as written, each line that is not blank indented by `indent`.
pub(super) fn indented(text: &str, indent: &str) -> String {
    text.trim_end()
        .lines()
        .map(|line| {
            if line.trim().is_empty() {
                String::new()
            } else {
                format!("{indent}{line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The date and the time of a timestamp in the local time zone.
pub(super) fn local_date_and_time(timestamp: jiff::Timestamp) -> (String, String) {
    let local = timestamp.to_zoned(jiff::tz::TimeZone::system());
    (
        local.strftime("%Y-%m-%d").to_string(),
        local.strftime("%H:%M").to_string(),
    )
}

/// Where a wrapped `↳` line of an item's header continues: under the text after `↳ `.
pub(super) const LINK_INDENT: &str = "    ";

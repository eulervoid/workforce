//! Markdown for a terminal: bold, italic, headings, lists with hanging indents, quotes, inline
//! code and fenced code, as text with escape codes for its styles.

use anstyle::Style;
use pulldown_cmark::{Event, LinkType, Options, Parser, Tag, TagEnd};

use crate::theme;

/// `text` rendered with every line indented by `indent` and wrapped to `width` columns, indent
/// included, or not at all. Words longer than the width, such as URLs, are left whole.
pub fn render(text: &str, width: Option<usize>, indent: &str) -> String {
    write(text, width, indent, None)
}

/// [`render`] with `marker` before the first line, the lines after it continuing under the text
/// rather than under the marker: `- [ ] 1. text` for a step.
pub fn render_after(marker: &str, text: &str, width: Option<usize>, indent: &str) -> String {
    write(text, width, indent, Some(marker.to_owned()))
}

fn write(text: &str, width: Option<usize>, indent: &str, marker: Option<String>) -> String {
    let mut writer = MarkdownWriter {
        width,
        base: indent.to_owned(),
        marker,
        ..MarkdownWriter::default()
    };
    let mut in_table = false;
    for (event, range) in Parser::new_ext(text, Options::ENABLE_TABLES).into_offset_iter() {
        match event {
            Event::Start(Tag::Table(_)) => {
                in_table = true;
                writer.table(&text[range]);
            }
            Event::End(TagEnd::Table) => in_table = false,
            _ if in_table => {}
            event => writer.event(event),
        }
    }
    writer.out
}

fn width_of(text: &str) -> usize {
    textwrap::core::display_width(text)
}

#[derive(Default)]
struct MarkdownWriter {
    width: Option<usize>,
    /// What every line starts with.
    base: String,
    out: String,
    /// The inline text of the block being written.
    line: String,
    /// Each open list: the next number, or `None` for bullets.
    lists: Vec<Option<u64>>,
    quotes: usize,
    /// The marker of the item being written, until its first line is out.
    marker: Option<String>,
    styles: Vec<Style>,
    /// Where each open link goes, shown after its text when that is not the address itself.
    links: Vec<Option<String>>,
    in_code: bool,
}

impl MarkdownWriter {
    fn indent(&self) -> String {
        format!(
            "{}{}{}",
            self.base,
            "│ ".repeat(self.quotes),
            "  ".repeat(self.lists.len().saturating_sub(1))
        )
    }

    fn flush(&mut self) {
        if self.line.is_empty() {
            return;
        }
        let line = std::mem::take(&mut self.line);
        let indent = self.indent();
        let first = match self.marker.take() {
            Some(marker) => format!("{indent}{marker}"),
            None if self.lists.is_empty() => indent.clone(),
            None => format!("{indent}  "),
        };
        let rest = format!(
            "{indent}{}",
            " ".repeat(width_of(&first) - width_of(&indent))
        );
        match self.width {
            Some(width) => {
                let options = textwrap::Options::new(width)
                    .break_words(false)
                    .word_separator(textwrap::WordSeparator::AsciiSpace)
                    .word_splitter(textwrap::WordSplitter::NoHyphenation)
                    .initial_indent(&first)
                    .subsequent_indent(&rest);
                self.out.push_str(&textwrap::fill(&line, options));
            }
            None => {
                for (index, part) in line.split('\n').enumerate() {
                    if index > 0 {
                        self.out.push('\n');
                    }
                    self.out.push_str(if index == 0 { &first } else { &rest });
                    self.out.push_str(part);
                }
            }
        }
        self.out.push('\n');
    }

    fn blank(&mut self) {
        if self.lists.is_empty() {
            self.out.push('\n');
        }
    }

    fn style_on(&mut self, style: Style) {
        self.line.push_str(&style.to_string());
        self.styles.push(style);
    }

    fn style_off(&mut self) {
        self.styles.pop();
        self.line.push_str(&anstyle::Reset.to_string());
        self.restyle();
    }

    /// Starts again the styles still open, after a reset.
    fn restyle(&mut self) {
        for style in &self.styles {
            self.line.push_str(&style.to_string());
        }
    }

    /// A table's rows as written, not wrapped, so its columns still line up.
    fn table(&mut self, source: &str) {
        self.flush();
        let indent = self.indent();
        for row in source.lines() {
            let mut row = row.trim_start();
            for _ in 0..self.quotes {
                row = row.strip_prefix('>').unwrap_or(row).trim_start();
            }
            if !row.is_empty() {
                self.out.push_str(&format!("{indent}{row}\n"));
            }
        }
        self.blank();
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(Tag::Strong) => self.style_on(theme::text_bold()),
            Event::Start(Tag::Emphasis) => self.style_on(Style::new().italic()),
            Event::Start(Tag::Heading { .. }) => self.style_on(theme::text_bold()),
            Event::End(TagEnd::Strong | TagEnd::Emphasis) => self.style_off(),
            Event::End(TagEnd::Heading(_)) => {
                self.style_off();
                self.flush();
                self.blank();
            }
            Event::Start(Tag::List(start)) => {
                self.flush();
                self.lists.push(start);
            }
            Event::End(TagEnd::List(_)) => {
                self.lists.pop();
                self.blank();
            }
            Event::Start(Tag::Item) => {
                self.flush();
                let marker = match self.lists.last_mut() {
                    Some(Some(number)) => {
                        let marker = format!("{number}. ");
                        *number += 1;
                        marker
                    }
                    _ => "• ".to_owned(),
                };
                self.marker = Some(marker);
            }
            Event::End(TagEnd::Item) => self.flush(),
            Event::Start(Tag::BlockQuote(_)) => {
                self.flush();
                self.quotes += 1;
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                self.flush();
                self.quotes -= 1;
                self.blank();
            }
            Event::End(TagEnd::Paragraph) => {
                self.flush();
                self.blank();
            }
            Event::Start(Tag::CodeBlock(_)) => {
                self.flush();
                self.in_code = true;
            }
            Event::End(TagEnd::CodeBlock) => {
                self.in_code = false;
                self.blank();
            }
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                ..
            }) => {
                let shown = match link_type {
                    LinkType::Autolink | LinkType::Email => None,
                    _ => Some(dest_url.into_string()),
                };
                self.links.push(shown);
            }
            Event::End(TagEnd::Link) => {
                if let Some(Some(destination)) = self.links.pop() {
                    let dimmed = theme::text_dimmed();
                    self.line
                        .push_str(&format!(" {dimmed}({destination}){dimmed:#}"));
                    self.restyle();
                }
            }
            Event::Text(text) if self.in_code => {
                let indent = self.indent();
                let dimmed = theme::text_dimmed();
                for line in text.lines() {
                    self.out
                        .push_str(&format!("{indent}    {dimmed}{line}{dimmed:#}\n"));
                }
            }
            Event::Text(text) => self.line.push_str(&text),
            Event::Code(text) => {
                let code = theme::code();
                self.line.push_str(&format!("{code}{text}{code:#}"));
                self.restyle();
            }
            Event::SoftBreak => self.line.push(' '),
            Event::HardBreak => self.line.push('\n'),
            Event::Rule => {
                self.flush();
                let room = self
                    .width
                    .unwrap_or(40)
                    .saturating_sub(width_of(&self.indent()));
                let divider = theme::divider();
                self.out.push_str(&format!(
                    "{}{divider}{}{divider:#}\n",
                    self.indent(),
                    "─".repeat(room.min(40))
                ));
                self.blank();
            }
            Event::Html(text) | Event::InlineHtml(text) => self.line.push_str(&text),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(text: &str) -> String {
        anstream::adapter::strip_str(text).to_string()
    }

    #[test]
    fn list_items_wrap_under_their_text() {
        assert_eq!(
            plain(&render(
                "- one two three four five six seven\n- next\n\nAfter.",
                Some(20),
                ""
            )),
            "• one two three four\n  five six seven\n• next\n\nAfter.\n\n"
        );
        assert_eq!(
            plain(&render("1. one two three", Some(14), "  ")),
            "  1. one two\n     three\n\n"
        );
    }

    #[test]
    fn wrapped_lines_keep_the_indent() {
        assert_eq!(
            plain(&render("one two three four five six", Some(16), "  ")),
            "  one two three\n  four five six\n\n"
        );
    }

    #[test]
    fn quotes_continue_under_their_bar() {
        assert_eq!(
            plain(&render("> one two three", Some(12), "  ")),
            "  │ one two\n  │ three\n\n\n"
        );
    }

    #[test]
    fn text_after_a_marker_continues_under_the_text() {
        let dimmed = theme::text_dimmed();
        let marker = format!("- [ ] {dimmed}1.{dimmed:#} ");
        assert_eq!(
            plain(&render_after(&marker, "one two three", Some(20), "  ")),
            "  - [ ] 1. one two\n           three\n\n"
        );
    }

    #[test]
    fn long_words_and_code_are_not_broken() {
        assert_eq!(
            plain(&render(
                "see https://example.com/a-long/path",
                Some(12),
                "  "
            )),
            "  see\n  https://example.com/a-long/path\n\n"
        );
        assert_eq!(
            plain(&render(
                "```\nlet value = one + two + three;\n```",
                Some(12),
                "  "
            )),
            "      let value = one + two + three;\n\n"
        );
    }

    #[test]
    fn tables_keep_their_rows_as_written() {
        assert_eq!(
            plain(&render(
                "Before.\n\n| step | who |\n|------|-----|\n| one  | ada **now** |\n\n> | a | b |\n> |---|---|\n> | 1 | 2 |",
                Some(12),
                "  "
            )),
            "  Before.\n\n  | step | who |\n  |------|-----|\n  | one  | ada **now** |\n\n  │ | a | b |\n  │ |---|---|\n  │ | 1 | 2 |\n\n\n"
        );
    }

    #[test]
    fn nothing_wraps_without_a_width_and_blank_lines_stay_empty() {
        assert_eq!(
            plain(&render("one two three four five six\n\n- four", None, "  ")),
            "  one two three four five six\n\n  • four\n\n"
        );
    }

    #[test]
    fn links_keep_where_they_go() {
        assert_eq!(
            plain(&render(
                "read [the spec](docs/SPEC.md) or <https://example.com>",
                None,
                ""
            )),
            "read the spec (docs/SPEC.md) or https://example.com\n\n"
        );
    }
}

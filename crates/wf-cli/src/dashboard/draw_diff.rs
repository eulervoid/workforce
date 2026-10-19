//! How the diff detail looks: a header for each file, then its hunks, each line with its numbers
//! on both sides and in the colour of what it does.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use wf_core::diff::{Content, DiffLine, FileChange, FileDiff};

use super::diff::{self, Changes, DiffPane, DiffRow, ShownComment};
use super::draw::{KeysAt, Themed, accent, draw_position, padded, selection_bar, themed};
use crate::dashboard::app::App;
use crate::theme;

/// How many columns a tab in a line takes.
const TAB: &str = "    ";

/// Draws the diff of `pane` in `area`; returns the first row it drew and the row of the cursor.
pub fn draw_diff(frame: &mut Frame, app: &App, pane: &DiffPane, area: Rect) -> (usize, usize) {
    let note = match pane.work() {
        None => Some(" reading the diff…".to_owned()),
        Some(Err(error)) => Some(format!(" {error}")),
        Some(Ok(work)) => match &work.changes {
            Changes::NoCommits => Some(" the agent has not made any commits".to_owned()),
            Changes::Files(files) if files.is_empty() => {
                Some(" the work changes no files".to_owned())
            }
            Changes::Files(_) => None,
        },
    };
    if let Some(note) = note {
        frame.render_widget(
            Paragraph::new(Line::from(note).themed(theme::text_dimmed())),
            area,
        );
        return (0, 0);
    }
    let rows = pane.rows();
    let height = usize::from(area.height);
    let (top, cursor) = diff::placed(&rows, pane.top, pane.cursor, height);
    let files = pane.files();
    let numbers = number_width(files);
    let width = usize::from(padded(area).width);
    let accent = accent(app, KeysAt::Detail);
    let selected = pane.selected();
    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .skip(top)
        .take(height)
        .map(|(index, row)| {
            let is_selected = selected.as_ref().is_some_and(|rows| rows.contains(&index));
            let marker = if index == cursor || is_selected {
                selection_bar(accent)
            } else {
                Span::raw(" ")
            };
            let mut spans = vec![marker];
            spans.extend(row_spans(pane, *row, numbers, width.saturating_sub(1)));
            let line = Line::from(spans);
            if is_selected {
                // Across the whole width, so the selection reads as a block.
                let used = line.width();
                let mut line = line;
                line.spans
                    .push(Span::raw(" ".repeat(width.saturating_sub(used))));
                line.bg(Color::from(theme::accent_dimmed()))
            } else {
                line
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), padded(area));
    draw_position(
        frame,
        area,
        top,
        rows.len().saturating_sub(height),
        app.base,
    );
    (top, cursor)
}

/// How many columns the widest line number of `files` takes.
fn number_width(files: &[FileDiff]) -> usize {
    let widest = files
        .iter()
        .filter_map(|file| match &file.content {
            Content::Text(hunks) => hunks.last()?.lines.last().map(|line| {
                line.old_number()
                    .unwrap_or_default()
                    .max(line.new_number().unwrap_or_default())
            }),
            Content::Binary => None,
        })
        .max()
        .unwrap_or_default();
    widest.max(1).to_string().len()
}

/// What a row shows, right of the cursor's column, in `room` columns.
fn row_spans(pane: &DiffPane, row: DiffRow, numbers: usize, room: usize) -> Vec<Span<'static>> {
    let files = pane.files();
    match row {
        DiffRow::Blank => Vec::new(),
        DiffRow::File(index) => {
            let file = &files[index];
            file_header(file, pane.is_folded(&file.path), room)
        }
        DiffRow::Comment { shown, row, .. } => comment_spans(pane, shown, row, numbers),
        DiffRow::NoLines(index) => {
            let note = match (&files[index].content, &files[index].change) {
                (Content::Binary, _) => "binary, not shown",
                (Content::Text(_), FileChange::Renamed { .. }) => "renamed, no lines changed",
                (Content::Text(_), _) => "no lines changed, only its mode",
            };
            vec![themed(format!("  {note}"), theme::text_dimmed())]
        }
        DiffRow::Hunk { file, hunk } => {
            let Content::Text(hunks) = &files[file].content else {
                return Vec::new();
            };
            let heading = &hunks[hunk].heading;
            let gap = " ".repeat(numbers * 2 + 1);
            vec![themed(format!("{gap} ⋯ {heading}"), theme::text_dimmed())]
        }
        DiffRow::Line { file, hunk, line } => {
            let Content::Text(hunks) = &files[file].content else {
                return Vec::new();
            };
            line_spans(&hunks[hunk].lines[line], numbers)
        }
    }
}

/// A row of a comment, below the numbers of the lines: its header, a line it quotes when it is of
/// another commit, or a line of its text.
fn comment_spans(
    pane: &DiffPane,
    shown: ShownComment,
    row: usize,
    numbers: usize,
) -> Vec<Span<'static>> {
    let gap = " ".repeat(numbers * 2 + 2);
    let comment = pane.comment(shown);
    let (bar, by) = match shown {
        ShownComment::Draft(_) => (theme::attention(), "✎ draft".to_owned()),
        ShownComment::Posted(index) => (theme::text_dimmed(), pane.posted()[index].by.clone()),
    };
    let mut spans = vec![Span::raw(gap), themed("┃ ", bar)];
    let outdated = pane.is_outdated(shown);
    if row == 0 {
        let mut header = format!("{by} · {}", comment.kind.label());
        if let ShownComment::Posted(index) = shown
            && pane.posted()[index].resolved
        {
            header.push_str(" · resolved");
        }
        if outdated {
            header.push_str(&format!(" · outdated: {}", comment.lines.heading()));
        }
        spans.push(themed(header, bar.bold()));
        return spans;
    }
    let quoted = if outdated {
        comment.lines.quote.lines().count()
    } else {
        0
    };
    let body = pane.comment_body(shown);
    let text = body
        .get(row - 1)
        .cloned()
        .unwrap_or_default()
        .replace('\t', TAB);
    if row <= quoted {
        let style = match text.chars().next() {
            Some('+') => theme::diff_added(),
            Some('-') => theme::diff_removed(),
            _ => theme::text_dimmed(),
        };
        spans.push(themed(text, style.dimmed()));
    } else {
        spans.push(Span::raw(text));
    }
    spans
}

/// `▾ src/lib.rs  +12 −3`, with where a file renamed came from; `▸` while it is folded.
fn file_header(file: &FileDiff, folded: bool, room: usize) -> Vec<Span<'static>> {
    let (added, removed) = file.counts();
    let what = match &file.change {
        FileChange::Added => " (new)".to_owned(),
        FileChange::Deleted => " (deleted)".to_owned(),
        FileChange::Modified => String::new(),
        FileChange::Renamed { from } => format!(" (from {from})"),
    };
    let counts = format!("+{added} −{removed}");
    let mut spans = vec![
        themed(file.path.clone(), theme::text_bold()),
        themed(what, theme::text_dimmed()),
    ];
    let used: usize = spans.iter().map(Span::width).sum::<usize>() + 2;
    let padding = room
        .saturating_sub(used + counts.chars().count() + 1)
        .max(2);
    spans.push(Span::raw(" ".repeat(padding)));
    spans.push(themed(format!("+{added}"), theme::diff_added()));
    spans.push(Span::raw(" "));
    spans.push(themed(format!("−{removed}"), theme::diff_removed()));
    let chevron = if folded { "▸ " } else { "▾ " };
    let mut header = vec![themed(chevron, theme::text_dimmed())];
    header.extend(spans);
    header
}

/// ` 12  13 +text`: the numbers on both sides, dim, then the line in the colour of what it does.
fn line_spans(line: &DiffLine, numbers: usize) -> Vec<Span<'static>> {
    let number = |number: Option<u32>| match number {
        Some(number) => format!("{number:>numbers$}"),
        None => " ".repeat(numbers),
    };
    let style = match line {
        DiffLine::Context { .. } => Style::default(),
        DiffLine::Added { .. } => Style::from(theme::diff_added()),
        DiffLine::Removed { .. } => Style::from(theme::diff_removed()),
    };
    let text = line.text().replace('\t', TAB);
    vec![
        themed(
            format!(
                "{} {} ",
                number(line.old_number()),
                number(line.new_number())
            ),
            theme::text_dimmed(),
        ),
        Span::styled(format!("{}{text}", line.marker()), style),
    ]
}

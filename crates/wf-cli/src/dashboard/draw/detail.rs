//! The detail opened from a list, and the log.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Wrap};

use super::chrome::{SEPARATOR, button, draw_breadcrumb};
use super::lists::{RowLines, context_line, right_start};
use super::{Themed, ansi_text, draw_position, padded, themed};
use crate::dashboard::app::{App, Drawn, Opened, Request, Scroll, Spot, Target};
use crate::dashboard::draw_diff;
use crate::dashboard::hint::{ALLOW, DENY};
use crate::dashboard::links;
use crate::dashboard::snapshot::Detail;
use crate::dashboard::snapshot::{InView, Place, SessionInfo, Waiting};
use crate::dashboard::transcript_lines::TranscriptLines;
use crate::theme;
use crate::transcript::{self, Format, Transcript};

/// Where a detail was drawn.
pub(super) enum Shown {
    /// A text, at this scroll.
    Text(Scroll),
    /// A diff, its rows in `area` from `top`, with the cursor on the row `cursor`.
    Diff {
        area: Rect,
        top: usize,
        cursor: usize,
    },
}

/// What drawing showed of a detail, for the rest of what was drawn to go with.
pub(super) fn shown_detail(shown: Option<Shown>) -> Drawn {
    match shown {
        Some(Shown::Text(scroll)) => Drawn {
            text: Some(scroll),
            ..Drawn::default()
        },
        Some(Shown::Diff { area, top, cursor }) => Drawn {
            diff: Some((area, top, cursor)),
            ..Drawn::default()
        },
        None => Drawn::default(),
    }
}

/// Draws a detail under its breadcrumb; returns where it was drawn, and adds its links to
/// `spots`.
pub(super) fn draw_detail(
    frame: &mut Frame,
    app: &App,
    opened: Opened,
    area: Rect,
    spots: &mut Vec<Spot>,
) -> Option<Shown> {
    let [title, body] = Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(area);
    // The same margin as the lists.
    let body = Rect {
        x: body.x + 1,
        width: body.width.saturating_sub(1),
        ..body
    };
    spots.extend(draw_breadcrumb(frame, app, title));
    let in_view = app.in_view_of(&opened.detail);
    // Until the snapshot has caught up with what was opened, the detail stays empty.
    let (scroll, item) = match &opened.detail {
        Detail::Item(id) => {
            let Some(InView::Item { text, .. }) = in_view else {
                return None;
            };
            let text = ansi_text(text);
            (
                draw_scrolled(frame, text, opened.scroll, body, app.base),
                Some(*id),
            )
        }
        Detail::Agent(_) => {
            let Some(InView::Agent { text, .. }) = in_view else {
                return None;
            };
            let text = ansi_text(text);
            (
                draw_scrolled(frame, text, opened.scroll, body, app.base),
                None,
            )
        }
        Detail::Flag(_) => {
            let Some(InView::Flag { text, .. }) = in_view else {
                return None;
            };
            let text = ansi_text(text);
            (
                draw_scrolled(frame, text, opened.scroll, body, app.base),
                None,
            )
        }
        Detail::Session(_) => {
            let Some(InView::Session(session)) = in_view else {
                return None;
            };
            let scroll = draw_session(frame, app, session, opened.scroll, body, spots);
            (scroll, Some(session.item))
        }
        // Code is no text with links in it.
        Detail::Diff(id) => {
            let pane = app.diffs.get(id)?;
            let (top, cursor) = draw_diff::draw_diff(frame, app, pane, body);
            return Some(Shown::Diff {
                area: body,
                top,
                cursor,
            });
        }
    };
    spots.extend(links::find(
        frame.buffer_mut(),
        body,
        item,
        Some(&opened.detail),
        &app.snapshot.links,
    ));
    Some(Shown::Text(scroll))
}

/// Draws a session's transcript, with its permission request under it; returns the scroll it was
/// drawn at, and adds the answers to the request to `spots`.
pub(super) fn draw_session(
    frame: &mut Frame,
    app: &App,
    session: &SessionInfo,
    scroll: Scroll,
    body: Rect,
    spots: &mut Vec<Spot>,
) -> Scroll {
    let mut transcript_lines = app.transcript_lines.borrow_mut();
    let (lines, open): (&[Line<'static>], Vec<Line<'static>>) =
        match (&session.place, app.transcripts.get(&session.id)) {
            (Place::Elsewhere(device), _) => (
                &[],
                vec![Line::from(format!(" transcript on {device}")).themed(theme::text_dimmed())],
            ),
            (Place::Here, None) => (
                &[],
                vec![Line::from(" no transcript yet").themed(theme::text_dimmed())],
            ),
            (Place::Here, Some(transcript)) => {
                let layout = transcript::Layout::new(
                    session.agent_name.as_str(),
                    usize::from(padded(body).width).max(20),
                    Format::Styled,
                    app.verbosity,
                    app.time_zone.clone(),
                );
                let laid_out = transcript_lines
                    .entry(session.id)
                    .or_insert_with(|| TranscriptLines::new(layout.clone()));
                laid_out.update(layout, transcript);
                let open = laid_out.open_lines(transcript);
                (laid_out.lines(), open)
            }
        };
    let asks: Vec<(&str, Request)> = app
        .snapshot
        .waiting
        .iter()
        .filter_map(|entry| match entry {
            Waiting::Permission {
                session: asking,
                what,
                ..
            } if *asking == session.id => Some((what.as_str(), entry.request()?)),
            Waiting::Permission { .. }
            | Waiting::Blocker { .. }
            | Waiting::Review { .. }
            | Waiting::Notice { .. }
            | Waiting::MachineFault { .. } => None,
        })
        .collect();
    let transcript = app.transcripts.get(&session.id);
    let bottom = context_line(
        transcript.and_then(Transcript::model),
        transcript.and_then(Transcript::usage),
    );
    let [body, bar, context] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(u16::from(!asks.is_empty())),
        Constraint::Length(u16::from(bottom.is_some())),
    ])
    .areas(body);
    if let Some(line) = bottom {
        frame.render_widget(Paragraph::new(line), context);
    }
    let shown = draw_lines(frame, lines, &open, scroll, body, app.base);
    if let Some((what, request)) = asks.first() {
        let line = Line::from(vec![
            themed("? ", theme::attention().bold()),
            Span::raw(format!("wants: {what}")),
        ]);
        let (allow_text, deny_text) = (ALLOW.text(), DENY.text());
        let keys = [allow_text.as_str(), deny_text.as_str()].join(SEPARATOR);
        let row = RowLines::new(app, false, bar.width);
        let right = Line::from(themed(keys.clone(), theme::key()));
        let first = row.lines(line.spans, Some(right), Vec::new()).remove(0);
        let allow = right_start(&first, &keys);
        let width = textwrap::core::display_width;
        let deny = allow + width(&allow_text) + width(SEPARATOR);
        spots.push(button(
            bar,
            allow,
            width(&allow_text),
            Target::Allow(*request),
        ));
        spots.push(button(bar, deny, width(&deny_text), Target::Deny(*request)));
        frame.render_widget(Paragraph::new(first), bar);
    }
    shown
}

/// Draws `text` wrapped to `area` without its last column at `scroll`, kept to what the text
/// allows; returns the scroll it was drawn at.
pub(super) fn draw_scrolled(
    frame: &mut Frame,
    text: Text<'static>,
    scroll: Scroll,
    area: Rect,
    base: Style,
) -> Scroll {
    let paragraph = Paragraph::new(text).wrap(Wrap { trim: false });
    let lines = paragraph.line_count(padded(area).width);
    let last_top = lines.saturating_sub(usize::from(area.height));
    let scroll = scroll.within(last_top);
    let top = u16::try_from(scroll.top()).unwrap_or(u16::MAX);
    frame.render_widget(paragraph.scroll((top, 0)), padded(area));
    draw_position(frame, area, scroll.top(), last_top, base);
    scroll
}

/// Draws `lines` then `more`, wrapped already to `area` without its last column, at `scroll`,
/// kept to what they allow; returns the scroll they were drawn at. Only the lines on screen are
/// handed to ratatui, so a frame costs what is shown rather than all there is.
pub(super) fn draw_lines(
    frame: &mut Frame,
    lines: &[Line<'static>],
    more: &[Line<'static>],
    scroll: Scroll,
    area: Rect,
    base: Style,
) -> Scroll {
    let height = usize::from(area.height);
    let last_top = (lines.len() + more.len()).saturating_sub(height);
    let scroll = scroll.within(last_top);
    let shown: Vec<Line> = lines
        .iter()
        .chain(more)
        .skip(scroll.top())
        .take(height)
        .cloned()
        .collect();
    frame.render_widget(Paragraph::new(shown), padded(area));
    draw_position(frame, area, scroll.top(), last_top, base);
    scroll
}

pub(super) fn draw_log(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    spots: &mut Vec<Spot>,
) -> Option<Scroll> {
    let area = Rect {
        x: area.x + 1,
        width: area.width.saturating_sub(1),
        ..area
    };
    let text: String = app.log_lines().map(String::as_str).collect();
    if text.is_empty() {
        let empty = if app.log.filter.is_empty() {
            "no events yet"
        } else {
            "no events match the filter"
        };
        frame.render_widget(Paragraph::new(empty).themed(theme::text_dimmed()), area);
        return None;
    }
    let text = ansi_text(&text);
    let scroll = draw_scrolled(frame, text, app.log.scroll, area, app.base);
    spots.extend(links::find(
        frame.buffer_mut(),
        area,
        None,
        None,
        &app.snapshot.links,
    ));
    Some(scroll)
}

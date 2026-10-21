//! The detail opened from a list, and the log.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Wrap};

use super::chrome::{SEPARATOR, button, draw_breadcrumb, session_crumb};
use super::lists::{RowLines, context_line, right_start};
use super::{Themed, ansi_text, draw_position, padded, themed};
use crate::dashboard::app::{App, DrawnDetail, Opened, Request, Scroll, Spot, Target};
use crate::dashboard::draw_diff;
use crate::dashboard::hint::{ALLOW, DENY, NEXT_SESSION};
use crate::dashboard::links;
use crate::dashboard::snapshot::Detail;
use crate::dashboard::snapshot::{InView, InboxEntry, Place, SessionInfo};
use crate::theme;
use crate::transcript::Transcript;
use crate::view::SessionStatus;
use wf_core::session::EndReason;

/// Draws a detail under its breadcrumb; returns where it was drawn, and adds its links to
/// `spots`.
pub(super) fn draw_detail(
    frame: &mut Frame,
    app: &App,
    opened: Opened,
    area: Rect,
    spots: &mut Vec<Spot>,
) -> Option<DrawnDetail> {
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
            let Some(InView::Session { info, successor }) = in_view else {
                return None;
            };
            let scroll = draw_session(
                frame,
                app,
                info,
                successor.as_deref(),
                opened.scroll,
                body,
                spots,
            );
            (scroll, Some(info.item))
        }
        // Code is no text with links in it.
        Detail::Diff(id) => {
            let pane = app.diffs.get(id)?;
            let body = match draw_diff::header_note(pane, app.snapshot.now_ms) {
                Some(note) => {
                    let [line, rest] =
                        Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(body);
                    frame.render_widget(
                        Paragraph::new(Line::from(note).themed(theme::text_dimmed())),
                        line,
                    );
                    rest
                }
                None => body,
            };
            let (top, cursor) = draw_diff::draw_diff(frame, app, pane, body);
            return Some(DrawnDetail::Diff {
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
    Some(DrawnDetail::Text(scroll))
}

/// Draws a session's transcript, with its permission request under it, or once it ended, that it
/// did and the `successor` its item went on in; returns the scroll it was drawn at, and adds the
/// answers to the request and the key to the successor to `spots`.
pub(super) fn draw_session(
    frame: &mut Frame,
    app: &App,
    session: &SessionInfo,
    successor: Option<&SessionInfo>,
    scroll: Scroll,
    body: Rect,
    spots: &mut Vec<Spot>,
) -> Scroll {
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
            (Place::Here, Some(transcript)) => match app.transcript_lines.get(&session.id) {
                Some(laid_out) => (laid_out.lines(), laid_out.open_lines(transcript)),
                None => (&[], Vec::new()),
            },
        };
    let asks: Vec<(&str, Request)> = app
        .snapshot
        .waiting
        .iter()
        .filter_map(|entry| match entry {
            InboxEntry::Permission {
                session: asking,
                what,
                ..
            } if *asking == session.id => Some((what.as_str(), entry.request()?)),
            InboxEntry::Permission { .. }
            | InboxEntry::Blocker { .. }
            | InboxEntry::Review { .. }
            | InboxEntry::Notice { .. }
            | InboxEntry::MachineFault { .. } => None,
        })
        .collect();
    let transcript = app.transcripts.get(&session.id);
    let bottom = context_line(
        transcript.and_then(Transcript::model),
        transcript.and_then(Transcript::usage),
    );
    let ended = match session.status {
        SessionStatus::Live | SessionStatus::Parked => None,
        SessionStatus::Ended { reason, .. } => Some(reason),
    };
    let [body, bar, ended_bar, context] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(u16::from(!asks.is_empty())),
        Constraint::Length(u16::from(ended.is_some())),
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
    if let Some(reason) = ended {
        let mut line = vec![
            themed("■ ", theme::text_dimmed()),
            Span::raw(format!("ended: {}", reason.label())),
        ];
        let next = NEXT_SESSION.text();
        let right = successor.map(|successor| {
            line.push(Span::raw(match reason {
                EndReason::LeaseExpired => " · taken over by ",
                EndReason::Outclaimed => " · outclaimed by ",
                EndReason::Submitted
                | EndReason::Reviewed
                | EndReason::Supervised
                | EndReason::Released
                | EndReason::TimedOut
                | EndReason::Failed
                | EndReason::Cancelled
                | EndReason::Interrupted
                | EndReason::Diagnosed
                | EndReason::MachineFault(_) => " · its item goes on in ",
            }));
            line.extend(session_crumb(app, successor));
            if let Place::Elsewhere(device) = &successor.place {
                line.push(Span::raw(format!(" on {device}")));
            }
            Line::from(themed(next.clone(), theme::key()))
        });
        let first = RowLines::new(app, false, ended_bar.width)
            .lines(line, right, Vec::new())
            .remove(0);
        if successor.is_some() {
            let start = right_start(&first, &next);
            let width = textwrap::core::display_width(&next);
            if let Some(target) = NEXT_SESSION.target(None) {
                spots.push(button(ended_bar, start, width, target));
            }
        }
        frame.render_widget(Paragraph::new(first), ended_bar);
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
    let shown = app.shown_log();
    if shown.lines().is_empty() {
        let empty = if app.log.filter.is_empty() {
            "no events yet"
        } else {
            "no events match the filter"
        };
        frame.render_widget(Paragraph::new(empty).themed(theme::text_dimmed()), area);
        return None;
    }
    let scroll = draw_lines(frame, shown.lines(), &[], app.log.scroll, area, app.base);
    spots.extend(links::find(
        frame.buffer_mut(),
        area,
        None,
        None,
        &app.snapshot.links,
    ));
    Some(scroll)
}

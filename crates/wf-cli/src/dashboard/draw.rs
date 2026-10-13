//! How the dashboard looks: a function of its [`App`].

use ansi_to_tui::IntoText;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Wrap};
use wf_core::id::HashId;

use super::app::{App, Detail, Opened, Row, Scroll, Tab};
use super::snapshot::{InView, ItemRef, Place, SessionInfo, SessionStatus, Waiting};
use crate::transcript::{self, Format, Now, Transcript};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn draw(frame: &mut Frame, app: &App) {
    let [header, counts, rule, body, rule_below, latest, keys] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    draw_header(frame, app, header);
    frame.render_widget(Paragraph::new(counts_line(app)), counts);
    draw_rule(frame, rule);
    draw_body(frame, app, body);
    draw_rule(frame, rule_below);
    frame.render_widget(Paragraph::new(latest_line(app)), latest);
    frame.render_widget(Paragraph::new(keys_line(app)), keys);
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let mut tabs = Vec::new();
    for tab in Tab::ALL {
        if !tabs.is_empty() {
            tabs.push(Span::raw("  "));
        }
        let style = if tab == app.tab {
            Style::new().bold().reversed()
        } else {
            Style::new().dim()
        };
        let mut label = format!("{} {}", tab.key(), tab.name());
        if tab == Tab::Now && !app.snapshot.waiting.is_empty() {
            label.push_str(&format!("·{}", app.snapshot.waiting.len()));
        }
        tabs.push(Span::styled(label, style));
    }
    let tabs = Line::from(tabs);
    let [name, tabs_area] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(tabs.width() as u16 + 1),
    ])
    .areas(area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(" "),
            Span::raw(app.snapshot.name.clone()).bold(),
        ])),
        name,
    );
    frame.render_widget(Paragraph::new(tabs), tabs_area);
}

/// `3 live · 14 open · 1 blocked · 2 devices`, leaving out what is none.
fn counts_line(app: &App) -> Line<'static> {
    let counts = app.snapshot.counts;
    let mut parts = vec![
        format!("{} live", counts.live_sessions),
        format!("{} open", counts.open_items),
    ];
    if counts.blocked_items > 0 {
        parts.push(format!("{} blocked", counts.blocked_items));
    }
    if counts.devices > 1 {
        parts.push(format!("{} devices", counts.devices));
    }
    Line::from(format!(" {}", parts.join(" · "))).dim()
}

fn draw_rule(frame: &mut Frame, area: Rect) {
    let rule = "─".repeat(usize::from(area.width));
    frame.render_widget(Paragraph::new(rule).dim(), area);
}

fn draw_body(frame: &mut Frame, app: &App, area: Rect) {
    match app.tab {
        Tab::Now => draw_now(frame, app, area),
        Tab::Items | Tab::Agents | Tab::Log => {
            frame.render_widget(Paragraph::new(" Nothing here yet.").dim(), area);
        }
    }
}

/// The list, and the detail on screen: in its place, or right of it in a wide window.
fn draw_now(frame: &mut Frame, app: &App, area: Rect) {
    let in_view = app.in_view();
    if !app.is_wide() {
        match in_view {
            Some(opened) => draw_detail(frame, app, opened, area),
            None => draw_now_list(frame, app, area),
        }
        return;
    }
    let [list, rule, detail] = Layout::horizontal([
        Constraint::Length(app.list_width()),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(area);
    draw_now_list(frame, app, list);
    let rule_text: Vec<Line> = (0..rule.height).map(|_| Line::from("│")).collect();
    frame.render_widget(Paragraph::new(rule_text).dim(), rule);
    if let Some(opened) = in_view {
        draw_detail(frame, app, opened, detail);
    }
}

fn section(title: &str) -> Line<'static> {
    Line::from(format!(" {title}")).bold()
}

/// One row of a list, `width` wide: the selection marker, what it says cut to fit, and `right`
/// flush against the right edge; then the lines under it.
struct RowLines {
    selected: bool,
    width: u16,
}

impl RowLines {
    fn lines(
        &self,
        first: Vec<Span<'static>>,
        right: Option<Span<'static>>,
        more: Vec<Line<'static>>,
    ) -> Vec<Line<'static>> {
        let marker = if self.selected {
            Span::raw("▸").bold()
        } else {
            Span::raw(" ")
        };
        let width = usize::from(self.width);
        let right_width = right.as_ref().map_or(0, |right| right.width() + 2);
        let mut spans = vec![marker];
        spans.extend(cut_spans(first, width.saturating_sub(1 + right_width + 1)));
        if let Some(right) = right {
            let used: usize = spans.iter().map(Span::width).sum();
            let padding = width.saturating_sub(used + right.width() + 1).max(2);
            spans.push(Span::raw(" ".repeat(padding)));
            spans.push(right);
        }
        let mut first = Line::from(spans);
        if self.selected {
            first = first.bold();
        }
        let mut lines = vec![first];
        lines.extend(more);
        lines
    }
}

/// `spans` cut to `room` columns, ending in `…` when anything was cut.
fn cut_spans(spans: Vec<Span<'static>>, room: usize) -> Vec<Span<'static>> {
    let total: usize = spans.iter().map(Span::width).sum();
    if total <= room {
        return spans;
    }
    let mut kept = Vec::new();
    let mut used = 0;
    for span in spans {
        let left = room.saturating_sub(used + 1);
        if span.width() <= left {
            used += span.width();
            kept.push(span);
            continue;
        }
        let mut text = String::new();
        for character in span.content.chars() {
            let next = textwrap::core::display_width(&format!("{text}{character}"));
            if next > left {
                break;
            }
            text.push(character);
        }
        kept.push(Span::styled(format!("{}…", text.trim_end()), span.style));
        break;
    }
    kept
}

fn item_spans(item: &ItemRef) -> Vec<Span<'static>> {
    vec![
        Span::raw(format!("#{}", item.id.short())).fg(Color::Cyan),
        Span::raw(" "),
        Span::raw(item.title.clone()),
    ]
}

fn waiting_lines(entry: &Waiting, row: RowLines) -> Vec<Line<'static>> {
    match entry {
        Waiting::Permission {
            agent, item, what, ..
        } => {
            let mut first = vec![Span::raw("? ").fg(Color::Yellow), Span::raw(agent.clone())];
            if let Some(item) = item {
                first.push(Span::raw(" on "));
                first.push(Span::raw(format!("#{}", item.id.short())).fg(Color::Cyan));
            }
            first.push(Span::raw(" wants: "));
            first.push(Span::raw(what.clone()));
            row.lines(first, None, Vec::new())
        }
        Waiting::Blocker { item, by, text, .. } => {
            let mut first = vec![Span::raw("⧗ ").fg(Color::Yellow)];
            first.push(Span::raw(format!("#{}", item.id.short())).fg(Color::Cyan));
            first.push(Span::raw(format!(" blocker from {by}")));
            let quote = text
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or_default();
            let quote = cut_spans(
                vec![Span::raw(format!("\"{quote}\""))],
                usize::from(row.width) - 4,
            );
            let mut more = vec![Span::raw("   ")];
            more.extend(quote);
            row.lines(first, None, vec![Line::from(more).dim()])
        }
        Waiting::Review { item, commit } => {
            let mut first = vec![Span::raw("◎ ").fg(Color::Yellow)];
            first.push(Span::raw(format!("#{}", item.id.short())).fg(Color::Cyan));
            first.push(Span::raw(" review"));
            if let Some(commit) = commit {
                first.push(Span::raw(format!(" {}", &commit.to_string()[..8])).dim());
            }
            first.push(Span::raw(format!("  {}", item.title)));
            row.lines(first, None, Vec::new())
        }
        Waiting::Notice { item, kind, what } => {
            let mut first = vec![Span::raw("• ").fg(Color::Yellow)];
            first.extend(item_spans(item));
            let more = cut_spans(
                vec![Span::raw(format!("   {kind}: {what}"))],
                usize::from(row.width),
            );
            row.lines(first, None, vec![Line::from(more).dim()])
        }
    }
}

/// `12m`, `3h05m` or `2d`.
fn age(since_ms: u64, now_ms: u64) -> String {
    let minutes = now_ms.saturating_sub(since_ms) / 60_000;
    match minutes {
        0..60 => format!("{minutes}m"),
        60..1440 => format!("{}h{:02}m", minutes / 60, minutes % 60),
        _ => format!("{}d", minutes / 1440),
    }
}

/// What a session does now, from its transcript.
fn activity(app: &App, session: &SessionInfo) -> Option<Line<'static>> {
    if let Place::Elsewhere(device) = &session.place {
        return Some(Line::from(format!("   on {device}")).dim());
    }
    let transcript = app.transcripts.get(&session.id)?;
    let spinner = SPINNER[(app.snapshot.now_ms / 500) as usize % SPINNER.len()];
    let line = match transcript.now() {
        Now::Calling(call) => {
            let waits = match call.asked {
                transcript::Asked::Waiting => " · waits for permission",
                transcript::Asked::Not | transcript::Asked::Answered(_) => "",
            };
            format!("   {spinner} {}{waits}", transcript::call_line(call))
        }
        Now::Writing(text) => format!("   {text}"),
        Now::Thinking => format!("   {spinner} thinking"),
        Now::Idle => return None,
    };
    Some(Line::from(line).dim())
}

fn session_lines(app: &App, session: &SessionInfo, row: RowLines) -> Vec<Line<'static>> {
    let glyph = match session.status {
        SessionStatus::Live => Span::raw("◆ ").fg(Color::Blue),
        SessionStatus::Parked => Span::raw("◇ ").fg(Color::Blue),
        SessionStatus::Ended => Span::raw("· ").dim(),
    };
    let mut first = vec![glyph, Span::raw(format!("{:<9} ", session.agent))];
    first.extend(item_spans(&session.item));
    let mut right = age(session.started_ms, app.snapshot.now_ms);
    if session.status == SessionStatus::Parked {
        right = format!("parked {right}");
    }
    let more = activity(app, session)
        .map(|line| Line::from(cut_spans(line.spans, usize::from(row.width))).dim());
    row.lines(
        first,
        Some(Span::raw(right).dim()),
        more.into_iter().collect(),
    )
}

/// The rows of the Now tab under their sections, and the line the selection starts on.
fn now_lines(app: &App, width: u16) -> (Vec<Line<'static>>, Option<usize>) {
    let snapshot = &app.snapshot;
    let selected = app.now.selection.get();
    let mut lines = Vec::new();
    let mut selected_line = None;
    let mut add = |lines: &mut Vec<Line<'static>>, row: Row, rows: Vec<Line<'static>>| {
        if selected == Some(&row) {
            selected_line = Some(lines.len());
        }
        lines.extend(rows);
    };
    if !snapshot.waiting.is_empty() {
        lines.push(section("WAITING ON YOU"));
        for entry in &snapshot.waiting {
            let row = Row::Waiting(entry.key());
            let rows = waiting_lines(
                entry,
                RowLines {
                    selected: selected == Some(&row),
                    width,
                },
            );
            add(&mut lines, row, rows);
        }
        lines.push(Line::default());
    }
    if !snapshot.working.is_empty() {
        lines.push(section("WORKING"));
        for session in &snapshot.working {
            let row = Row::Session(session.id);
            let rows = session_lines(
                app,
                session,
                RowLines {
                    selected: selected == Some(&row),
                    width,
                },
            );
            add(&mut lines, row, rows);
        }
        lines.push(Line::default());
    }
    if !snapshot.flagged.is_empty() {
        lines.push(section("NEEDS ATTENTION"));
        for flag in &snapshot.flagged {
            let row = Row::Flag(flag.clone());
            let first = vec![Span::raw("! ").fg(Color::Red), Span::raw(flag.clone())];
            let rows = RowLines {
                selected: selected == Some(&row),
                width,
            }
            .lines(first, None, Vec::new());
            add(&mut lines, row, rows);
        }
    }
    if lines.is_empty() {
        lines.push(Line::from(" Nothing waits on you, and no agent is working.").dim());
    }
    (lines, selected_line)
}

fn draw_now_list(frame: &mut Frame, app: &App, area: Rect) {
    let (lines, selected_line) = now_lines(app, area.width);
    // Keep the selection and the line after it on screen.
    let offset = selected_line
        .map(|line| (line + 2).saturating_sub(usize::from(area.height)))
        .unwrap_or_default();
    frame.render_widget(Paragraph::new(lines).scroll((offset as u16, 0)), area);
}

fn draw_detail(frame: &mut Frame, app: &App, opened: Opened, area: Rect) {
    let [title, body] = Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(area);
    // The same margin as the lists.
    let body = Rect {
        x: body.x + 1,
        width: body.width.saturating_sub(1),
        ..body
    };
    match (opened.detail, &app.snapshot.in_view) {
        (Detail::Item(id), Some(InView::Item { id: shown, text })) if id == *shown => {
            // The header line of `wf item show` stays on top.
            let mut text: Text = text.into_text().unwrap_or_default();
            let header = if text.lines.is_empty() {
                Line::default()
            } else {
                text.lines.remove(0)
            };
            let mut spans = vec![Span::raw(" ← ").dim()];
            spans.extend(header.spans);
            frame.render_widget(Paragraph::new(Line::from(spans)), title);
            draw_scrolled(frame, text, opened.scroll, body, true);
        }
        (Detail::Session(id), Some(InView::Session(session))) if id == session.id => {
            frame.render_widget(Paragraph::new(session_title(app, session)), title);
            let text = match (&session.place, app.transcripts.get(&id)) {
                (Place::Elsewhere(device), _) => {
                    Text::from(Line::from(format!(" transcript on {device}")).dim())
                }
                (Place::Here, None) => Text::from(Line::from(" no transcript yet").dim()),
                (Place::Here, Some(transcript)) => {
                    let width = usize::from(body.width).max(20);
                    transcript_text(app, &session.agent, transcript, width)
                }
            };
            draw_scrolled(frame, text, opened.scroll, body, false);
        }
        // The snapshot has not caught up with what was opened yet.
        _ => {}
    }
}

/// `← #k3j1 Parse config lazily · coder · work · 12m  ◆`
fn session_title(app: &App, session: &SessionInfo) -> Line<'static> {
    let mut spans = vec![Span::raw(" ← ").dim()];
    spans.extend(item_spans(&session.item));
    spans.push(
        Span::raw(format!(
            " · {} · {} · {}",
            session.agent,
            session.role,
            age(session.started_ms, app.snapshot.now_ms)
        ))
        .dim(),
    );
    let glyph = match session.status {
        SessionStatus::Live => Span::raw("  ◆").fg(Color::Blue),
        SessionStatus::Parked => Span::raw("  ◇ parked").fg(Color::Blue),
        SessionStatus::Ended => Span::raw("  ended").dim(),
    };
    spans.push(glyph);
    Line::from(spans)
}

/// The whole transcript laid out for `width`: its blocks, then what is still open.
fn transcript_text(app: &App, agent: &str, transcript: &Transcript, width: usize) -> Text<'static> {
    let layout = transcript::Layout::new(
        agent,
        width,
        Format::Styled,
        transcript::Detail::Abridged,
        app.time_zone.clone(),
    );
    let mut out = String::new();
    let mut last = transcript::Spacing::Nothing;
    let mut add = |out: &mut String, opens: transcript::Spacing, leaves, text: String| {
        if text.is_empty() {
            return;
        }
        out.push_str(last.before(opens));
        out.push_str(&text);
        last = leaves;
    };
    for block in transcript.blocks() {
        add(&mut out, block.opens(), block.leaves(), layout.block(block));
    }
    if let Some(prose) = transcript.open_prose() {
        let spacing = transcript::Spacing::Prose;
        add(
            &mut out,
            spacing,
            spacing,
            layout.prose(prose.kind, &prose.text),
        );
    }
    for call in transcript.open_calls() {
        let spacing = transcript::Spacing::Tool;
        add(&mut out, spacing, spacing, layout.running(call));
    }
    out.into_text().unwrap_or_default()
}

fn draw_scrolled(frame: &mut Frame, text: Text<'static>, scroll: Scroll, area: Rect, wrap: bool) {
    let height = usize::from(area.height);
    let mut paragraph = Paragraph::new(text);
    if wrap {
        paragraph = paragraph.wrap(Wrap { trim: false });
    }
    let lines = paragraph.line_count(area.width);
    let last_top = lines.saturating_sub(height);
    let top = match scroll {
        Scroll::FromTop(offset) => usize::from(offset).min(last_top),
        Scroll::FromEnd(offset) => last_top.saturating_sub(usize::from(offset)),
    };
    frame.render_widget(paragraph.scroll((top as u16, 0)), area);
}

/// The latest event, or why the last refresh failed.
fn latest_line(app: &App) -> Line<'static> {
    if let Some(error) = &app.error {
        let first = error.lines().next().unwrap_or_default();
        return Line::from(format!(" error: {first}")).fg(Color::Red);
    }
    match &app.snapshot.latest {
        Some(line) => {
            let first = line.lines().next().unwrap_or_default();
            let text: Text = first.into_text().unwrap_or_default();
            let mut spans = vec![Span::raw(" ")];
            spans.extend(
                text.lines
                    .into_iter()
                    .next()
                    .into_iter()
                    .flat_map(|line| line.spans),
            );
            Line::from(spans)
        }
        None => Line::from(" no events yet").dim(),
    }
}

/// The keys that work on what is on screen.
fn keys_line(app: &App) -> Line<'static> {
    let keys = match app.tab {
        Tab::Now if !app.now.stack.is_empty() => "esc back · j/k scroll · g/G top/end · q quit",
        Tab::Now => match app.now.selection.get().and_then(|row| app.detail_of(row)) {
            Some(_) => "enter open · j/k move · tab next · q quit",
            None => "j/k move · tab next · q quit",
        },
        Tab::Items | Tab::Agents | Tab::Log => "tab next · q quit",
    };
    Line::from(format!(" {keys}")).dim()
}

//! How the dashboard looks: a function of its [`App`].

use ansi_to_tui::IntoText;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;

use super::app::{App, Tab};

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
        let number = tab_number(tab);
        let style = if tab == app.tab {
            Style::new().bold().reversed()
        } else {
            Style::new().dim()
        };
        let mut label = format!("{number} {}", tab.name());
        if tab == Tab::Now && app.snapshot.waiting > 0 {
            label.push_str(&format!("·{}", app.snapshot.waiting));
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

fn tab_number(tab: Tab) -> char {
    match tab {
        Tab::Now => '1',
        Tab::Items => '2',
        Tab::Agents => '3',
        Tab::Log => '4',
    }
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
    let text = match app.tab {
        Tab::Now | Tab::Items | Tab::Agents | Tab::Log => " Nothing here yet.",
    };
    frame.render_widget(Paragraph::new(text).dim(), area);
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

fn keys_line(_app: &App) -> Line<'static> {
    Line::from(" tab next · q quit").dim()
}

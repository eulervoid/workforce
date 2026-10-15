//! How the dashboard looks: a function of its [`App`].

use ansi_to_tui::IntoText;
use ratatui::Frame;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use wf_core::graph::Readiness;
use wf_core::id::HashId;

use super::app::{
    AgentRow, App, Choice, Clickable, ClickableList, Drawn, Hovered, ListTop, Opened, Prompt,
    Request, Row, STRIP, Scroll, Spot, Starter, Status, Tab, Target,
};
use super::links;
use super::snapshot::{
    AgentInfo, InView, ItemRef, MovedBack, Place, SessionInfo, SessionStatus, Waiting,
};
use super::snapshot::{Detail, ItemRow};
use super::transcript_lines::TranscriptLines;
use crate::cli::ItemGrouping;
use crate::render::{self, duration};
use crate::theme;
use crate::transcript::{self, Format, Now, Transcript, Usage};
use wf_core::stats::ProjectStats;

/// `text` in a style of the theme.
fn themed<'a>(text: impl Into<std::borrow::Cow<'a, str>>, style: anstyle::Style) -> Span<'a> {
    Span::styled(text, Style::from(style))
}

/// The place on screen the keys go to, which the accent marks.
#[derive(Clone, Copy, PartialEq, Eq)]
enum KeysAt {
    /// The list of the tab: its selection.
    List,
    /// A detail: the last part of its breadcrumb.
    Detail,
    /// The Log tab without a detail opened: its label.
    Log,
    /// What the footer asks for: its label.
    Footer,
    /// A list to pick from: the option picked.
    Choice,
    /// The keys: their title.
    Keys,
}

impl KeysAt {
    fn of(app: &App) -> Self {
        match &app.prompt {
            Some(
                Prompt::Filter { .. }
                | Prompt::Answer { .. }
                | Prompt::Verdict { .. }
                | Prompt::Comment { .. }
                | Prompt::Stop { .. },
            ) => KeysAt::Footer,
            Some(Prompt::Choose(_)) => KeysAt::Choice,
            Some(Prompt::Keys) => KeysAt::Keys,
            None => match (app.tab, app.list_has_keys()) {
                (Tab::Now | Tab::Items | Tab::Agents, true) => KeysAt::List,
                (Tab::Log, true) => KeysAt::Log,
                (Tab::Now | Tab::Items | Tab::Agents | Tab::Log, false) => KeysAt::Detail,
            },
        }
    }
}

/// The accent of `place`: full while it has the keys and the terminal pane is focused, dimmed
/// otherwise.
fn accent(app: &App, place: KeysAt) -> Color {
    if app.terminal_focused && KeysAt::of(app) == place {
        theme::ACCENT.into()
    } else {
        theme::ACCENT_DIMMED.into()
    }
}

/// The bar left of a selection in `accent`, on the background of the selection: white while the
/// accent is full, in the dimmed accent itself otherwise, so only the dimmed background shows.
fn selection_bar(accent: Color) -> Span<'static> {
    let style = if accent == Color::from(theme::ACCENT) {
        Style::from(theme::ON_ACCENT)
    } else {
        Style::new().fg(accent)
    };
    Span::styled("▎", style.bg(accent))
}

/// The ends of a highlight inside a line, on `background`: a right half block before it and a left
/// half block after it, in `background` on the background of the line, so the colour reaches half
/// a cell past the text on either side.
fn highlight_ends(background: Color) -> (Span<'static>, Span<'static>) {
    let style = Style::new().fg(background);
    (Span::styled("▐", style), Span::styled("▌", style))
}

/// `content` highlighted inside a line: in `style` on `background`, between its ends. The ends
/// take a cell each, where the line would otherwise have a space around the text.
fn highlight(content: Vec<Span<'static>>, style: Style, background: Color) -> Vec<Span<'static>> {
    let (start, end) = highlight_ends(background);
    let style = style.bg(background);
    let mut spans = vec![start];
    spans.extend(content.into_iter().map(|span| span.patch_style(style)));
    spans.push(end);
    spans
}

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Draws the dashboard, and keeps the scroll of the text and of the list to what is shown of them.
pub fn draw(frame: &mut Frame, app: &mut App) {
    let drawn = draw_all(frame, app);
    app.shown(drawn);
}

/// Draws the dashboard; where the text and the list on screen ended up.
fn draw_all(frame: &mut Frame, app: &App) -> Drawn {
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
    let tabs = draw_header(frame, app, header);
    frame.render_widget(Paragraph::new(counts_line(app)), counts);
    draw_rule(frame, rule);
    let mut shown = draw_body(frame, app, body);
    shown.clickable.tabs = tabs;
    draw_rule(frame, rule_below);
    // A warning takes the right of the line, and the latest event what is left.
    let warning = app
        .moved_back
        .last()
        .map(|moved| draw_warning(frame, moved, app.moved_back.len() - 1, latest));
    let event_area = match &warning {
        // Two columns apart, as a tip keeps from the event.
        Some(warning) => Rect {
            width: warning.area.x.saturating_sub(latest.x + 2),
            ..latest
        },
        None => latest,
    };
    let latest_line = latest_line(app);
    let latest_width = latest_line.width().min(usize::from(event_area.width));
    frame.render_widget(Paragraph::new(latest_line), event_area);
    if app.status.is_none() {
        shown.clickable.spots.extend(links::find(
            frame.buffer_mut(),
            event_area,
            None,
            None,
            &app.snapshot.links,
        ));
    }
    shown.clickable.spots.extend(warning.clone());
    match &app.prompt {
        Some(Prompt::Choose(choice)) => {
            draw_choice(frame, choice, accent(app, KeysAt::Choice), body);
            frame.render_widget(
                Paragraph::new(Line::from(" j/k move · enter pick · esc cancel").dim()),
                keys,
            );
        }
        Some(Prompt::Keys) => {
            draw_keys(frame, accent(app, KeysAt::Keys), body);
            frame.render_widget(Paragraph::new(Line::from(" any key closes").dim()), keys);
        }
        Some(prompt) => draw_prompt(frame, app, prompt, keys),
        None => shown.clickable.spots.extend(draw_hints(frame, app, keys)),
    }
    shown.hovered = app
        .pointer
        .filter(|_| app.mouse_passes())
        .and_then(|pointer| shown.clickable.hovered(pointer));
    match &shown.hovered {
        // The warning under the pointer stays: it says more than its tip would.
        Some(Hovered::Spot(spot)) if Some(spot) == warning.as_ref() => {
            frame
                .buffer_mut()
                .set_style(spot.area, Style::new().underlined());
        }
        Some(Hovered::Spot(spot)) => {
            frame
                .buffer_mut()
                .set_style(spot.area, Style::new().underlined());
            // A tip takes the place of the warning while it shows.
            if warning.is_some() {
                let cleared = Rect {
                    x: event_area.right(),
                    width: latest.right() - event_area.right(),
                    ..latest
                };
                frame.render_widget(Clear, cleared);
            }
            draw_tip(frame, &app.tip(&spot.target), latest_width, latest);
        }
        // An inactive tab under the pointer is no longer dim: stronger than the other inactive
        // tabs, weaker than the active one.
        Some(Hovered::Tab(tab)) if *tab != app.tab => {
            let label = shown.clickable.tabs.iter().find(|(each, _)| each == tab);
            if let Some((_, label)) = label {
                frame.buffer_mut().set_style(*label, Style::new().not_dim());
            }
        }
        Some(Hovered::Tab(_)) | None => {}
    }
    shown
}

/// Draws `tip` at the right of the latest-event line, `taken` columns of which the latest event
/// fills. A line too narrow for both leaves the tip out: it never covers the event, nor is cut.
fn draw_tip(frame: &mut Frame, tip: &str, taken: usize, latest: Rect) {
    // A column off the right edge, as the event keeps one off the left.
    let tip = Line::from(format!("{tip} "));
    // Two columns apart, so the tip does not read as part of the event.
    if taken + 2 + tip.width() > usize::from(latest.width) {
        return;
    }
    frame.render_widget(Paragraph::new(tip).right_aligned(), latest);
}

/// Draws the warning of `moved` at the right of the latest-event line, with how many `others` it
/// stands for, its reason cut to fit; returns it as a button that opens the item.
fn draw_warning(frame: &mut Frame, moved: &MovedBack, others: usize, latest: Rect) -> Spot {
    let name = format!("#{}", moved.item.short());
    let opening = format!("⚠ {name} back to open: ");
    let count = if others > 0 {
        format!(" · {others} more")
    } else {
        String::new()
    };
    // A column off either edge.
    let room = usize::from(latest.width).saturating_sub(2);
    let width = textwrap::core::display_width;
    let reason_room = room.saturating_sub(width(&opening) + width(&count));
    let mut spans = vec![Span::raw(opening)];
    spans.extend(cut_spans(
        vec![Span::raw(moved.reason.clone())],
        reason_room,
    ));
    spans.push(Span::raw(count));
    let line = Line::from(spans).style(theme::ATTENTION);
    let line_width = line.width().min(room) as u16;
    let area = Rect {
        x: latest.right().saturating_sub(line_width + 1),
        width: line_width,
        ..latest
    };
    frame.render_widget(Paragraph::new(line), area);
    Spot {
        target: Target::Open {
            detail: Detail::Item(moved.item),
            name,
        },
        area,
    }
}

/// A list to pick from, at the bottom of the body, with the option picked in `accent`.
fn draw_choice(frame: &mut Frame, choice: &Choice, accent: Color, body: Rect) {
    let (title, options): (String, Vec<String>) = match choice {
        Choice::Start { item, starters, .. } => (
            format!(" start #{} ", item.short()),
            starters
                .iter()
                .map(|starter| match starter {
                    Starter::Dispatcher => "release: open it for the dispatcher".to_string(),
                    Starter::Agent(agent) => agent.to_string(),
                })
                .collect(),
        ),
        Choice::State { item, states, .. } => (
            format!(" move #{} to ", item.short()),
            states
                .iter()
                .map(|state| format!("{} {state}", render::state_icon(*state)))
                .collect(),
        ),
    };
    let lines: Vec<Line> = options
        .into_iter()
        .enumerate()
        .map(|(index, option)| {
            if index == choice.index() {
                Line::from(highlight(
                    vec![Span::raw(option)],
                    Style::new().bold(),
                    accent,
                ))
            } else {
                Line::from(format!(" {option}"))
            }
        })
        .collect();
    let height = (lines.len() as u16 + 2).min(body.height);
    let width = lines
        .iter()
        .map(Line::width)
        .chain([title.chars().count()])
        .max()
        .unwrap_or_default() as u16
        + 4;
    let area = Rect {
        x: body.x + 1,
        y: body.y + body.height - height,
        width: width.min(body.width.saturating_sub(1)),
        height,
    };
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(title)),
        area,
    );
}

/// Every key, by where it works.
const KEYS: &[(&str, &str)] = &[
    ("1-4 tab", "switch tabs, also shift-tab"),
    ("j/k g/G", "move, top, end (and follow)"),
    ("pgup/pgdn", "a page up, down; also space"),
    ("wheel", "move what is under the pointer"),
    ("click", "pick a tab or an entry, again to open"),
    ("", "follow a link, a key hint, a crumb"),
    ("shift-drag", "select text, as without the dashboard"),
    ("enter esc", "open, go back"),
    ("H L", "one pane or both, also shift ←/→"),
    ("a d", "allow, deny a permission request"),
    ("", "a click on allow asks for a second"),
    ("r R", "answer a blocker here, in $EDITOR"),
    ("ctrl+g", "carry a comment or answer to $EDITOR"),
    ("v D", "review, show the diff"),
    ("s x", "start a session, stop one"),
    ("m c e", "move, comment on, edit an item"),
    ("i f", "a session's item, full transcript"),
    ("o / F", "items: grouping, filter, readiness"),
    ("q", "quit"),
    ("tmux", "focus-events on: dims when unfocused"),
];

/// Draws the keys, under their title in `accent`.
fn draw_keys(frame: &mut Frame, accent: Color, body: Rect) {
    let lines: Vec<Line> = KEYS
        .iter()
        .map(|(keys, what)| {
            Line::from(vec![
                themed(format!(" {keys:<12}"), theme::KEY),
                Span::raw(*what),
            ])
        })
        .collect();
    let height = (lines.len() as u16 + 2).min(body.height);
    let area = Rect {
        x: body.x + 1,
        y: body.y,
        width: body.width.saturating_sub(2).min(52),
        height,
    };
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(Line::from(highlight(
            vec![Span::raw("keys")],
            Style::new().bold(),
            accent,
        )))),
        area,
    );
}

/// The line the footer asks for, with the cursor in it.
fn draw_prompt(frame: &mut Frame, app: &App, prompt: &Prompt, area: Rect) {
    // The label of what is asked has the accent; a question answered with keys has it on the
    // whole line.
    let accent = accent(app, KeysAt::Footer);
    let question = |text: String| Line::from(Span::styled(text, Style::new().bold().bg(accent)));
    let (label, input, hint) = match prompt {
        Prompt::Filter { input, .. } => ("/".to_owned(), input, ""),
        Prompt::Comment { item, input } => (
            format!("comment on #{}:", item.short()),
            input,
            "enter post · ctrl+g editor · esc cancel ",
        ),
        Prompt::Stop { session } => {
            let agent = app
                .snapshot
                .working
                .iter()
                .find(|working| working.id == *session)
                .map_or_else(String::new, |working| {
                    format!("{} on #{} ", working.agent, working.item.id.short())
                });
            let mut line = question(format!(" stop {agent}now? "));
            line.extend(answer_keys(&["y/n"]));
            draw_question(frame, app, line, area);
            return;
        }
        Prompt::Choose(_) | Prompt::Keys => return,
        Prompt::Answer { blocker, input, .. } => (
            format!("⧗ {} answer:", blocker.short()),
            input,
            "enter resolve · ctrl+g editor · esc cancel ",
        ),
        Prompt::Verdict { item } => {
            let mut line = question(format!(" ◎ #{}: ", item.short()));
            line.extend(answer_keys(&["a approve", "r changes", "D diff", "esc"]));
            draw_question(frame, app, line, area);
            return;
        }
    };
    // The label between its ends, and a space before what is typed.
    let mut label = highlight(vec![Span::raw(label)], Style::new().bold(), accent);
    label.push(Span::raw(" "));
    let label_width: usize = label.iter().map(Span::width).sum();
    // The hint keeps to the right while it leaves room to type; a narrow footer leaves it out.
    let hint_width = textwrap::core::display_width(hint) as u16;
    let area = if area.width >= label_width as u16 + hint_width + 16 {
        let [typed, hint_area] =
            Layout::horizontal([Constraint::Fill(1), Constraint::Length(hint_width)]).areas(area);
        frame.render_widget(Paragraph::new(Line::from(hint).dim()), hint_area);
        typed
    } else {
        area
    };
    let width = usize::from(area.width).saturating_sub(label_width + 1);
    let scroll = input.visual_scroll(width);
    label.push(Span::raw(
        input.value().chars().skip(scroll).collect::<String>(),
    ));
    let line = Line::from(label);
    frame.render_widget(Paragraph::new(line), area);
    let cursor = (input.visual_cursor().max(scroll) - scroll) as u16;
    frame.set_cursor_position((area.x + label_width as u16 + cursor, area.y));
}

/// Draws a question answered with keys: the accent covers the whole line, so the keys read as
/// part of it.
fn draw_question(frame: &mut Frame, app: &App, line: Line, area: Rect) {
    let style = Style::new().bg(accent(app, KeysAt::Footer));
    frame.render_widget(Paragraph::new(line).style(style), area);
}

/// The `keys` that answer a question, each a key and what it does: the key in bold, the words in
/// normal weight.
fn answer_keys(keys: &[&'static str]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (index, hint) in keys.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(SEPARATOR));
        }
        let (key, what) = hint.split_once(' ').unwrap_or((hint, ""));
        spans.push(themed(key, theme::ON_ACCENT.bold()));
        if !what.is_empty() {
            spans.push(Span::raw(format!(" {what}")));
        }
    }
    spans
}

/// Draws the name of the project and the tabs; returns where the label of each tab went.
fn draw_header(frame: &mut Frame, app: &App, area: Rect) -> Vec<(Tab, Rect)> {
    let mut tabs = Vec::new();
    // Each tab with the column its label starts on and its width, from the left of the tabs.
    let mut labels = Vec::new();
    for tab in Tab::ALL {
        if !tabs.is_empty() {
            tabs.push(Span::raw(" "));
        }
        let label = format!("{} {}", tab.key(), tab.name());
        let label = if tab == app.tab && tab == Tab::Log {
            // The log is the list of its tab: its label has the accent, as a selection would.
            highlight(
                vec![Span::raw(label)],
                Style::new().bold(),
                accent(app, KeysAt::Log),
            )
        } else if tab == app.tab {
            // Reversed, the background of the label is the colour of the text.
            highlight(
                vec![Span::raw(label)],
                Style::new().bold().reversed(),
                Color::Reset,
            )
        } else {
            // A space where the active tab has its ends, so the tabs keep their places.
            vec![Span::raw(format!(" {label} ")).dim()]
        };
        let start: usize = tabs.iter().map(Span::width).sum();
        let width: usize = label.iter().map(Span::width).sum();
        labels.push((tab, start, width));
        tabs.extend(label);
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
    labels
        .into_iter()
        .map(|(tab, start, width)| {
            let label = Rect {
                x: tabs_area.x.saturating_add(start as u16),
                width: width as u16,
                ..tabs_area
            };
            // What a narrow window cuts off cannot be clicked.
            (tab, label.intersection(tabs_area))
        })
        .collect()
}

/// `3 live · 14 open · 2 waiting · 1 blocked · 2 devices`, leaving out what is none; `waiting`
/// counts every entry that waits on the principal.
fn counts_line(app: &App) -> Line<'static> {
    let counts = app.snapshot.counts;
    let mut parts = vec![
        format!("{} live", counts.live_sessions),
        format!("{} open", counts.open_items),
    ];
    if !app.snapshot.waiting.is_empty() {
        parts.push(format!("{} waiting", app.snapshot.waiting.len()));
    }
    if counts.blocked_items > 0 {
        parts.push(format!("{} blocked", counts.blocked_items));
    }
    if counts.devices > 1 {
        parts.push(format!("{} devices", counts.devices));
    }
    Line::from(format!(" {}", parts.join(SEPARATOR))).dim()
}

/// A button `width` columns wide, `start` columns into the line `area`; what a narrow window cuts
/// off cannot be clicked.
fn button(area: Rect, start: usize, width: usize, target: Target) -> Spot {
    let button = Rect {
        x: area.x.saturating_add(start as u16),
        width: width as u16,
        ..area
    };
    Spot {
        target,
        area: button.intersection(area),
    }
}

fn draw_rule(frame: &mut Frame, area: Rect) {
    let rule = "─".repeat(usize::from(area.width));
    frame.render_widget(Paragraph::new(rule).dim(), area);
}

fn draw_body(frame: &mut Frame, app: &App, area: Rect) -> Drawn {
    match app.tab {
        Tab::Now => draw_split(frame, app, area, now_lines),
        Tab::Items => draw_split(frame, app, area, items_lines),
        Tab::Log => {
            let mut spots = Vec::new();
            let text = match app.in_view() {
                Some(opened) => draw_detail(frame, app, opened, area, &mut spots),
                None => draw_log(frame, app, area, &mut spots),
            };
            Drawn {
                text,
                clickable: Clickable {
                    spots,
                    ..Clickable::default()
                },
                ..Drawn::default()
            }
        }
        Tab::Agents => draw_split(frame, app, area, agents_lines),
    }
}

/// The list, and the detail on screen: right of it in a wide window, or in its place in a
/// narrow one or with the other pane folded away to a strip.
fn draw_split(frame: &mut Frame, app: &App, area: Rect, list_lines: ListLines) -> Drawn {
    let in_view = app.in_view();
    if !app.shows_both() {
        let (area, strip) = if app.is_wide() {
            let (pane, strip) = draw_strip(frame, app, area);
            (pane, Some(strip))
        } else {
            (area, None)
        };
        let mut drawn = match in_view {
            Some(opened) => {
                let mut spots = Vec::new();
                Drawn {
                    text: draw_detail(frame, app, opened, area, &mut spots),
                    clickable: Clickable {
                        spots,
                        ..Clickable::default()
                    },
                    ..Drawn::default()
                }
            }
            None => {
                let mut spots = Vec::new();
                let (list_top, list) = draw_list(frame, app, list_lines, area, &mut spots);
                Drawn {
                    text: None,
                    list_top: Some(list_top),
                    clickable: Clickable {
                        list: Some(list),
                        spots,
                        ..Clickable::default()
                    },
                    ..Drawn::default()
                }
            }
        };
        drawn.clickable.strip = strip;
        return drawn;
    }
    let [list, rule, detail] = Layout::horizontal([
        Constraint::Length(app.list_width()),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(area);
    let mut spots = Vec::new();
    let (list_top, list) = draw_list(frame, app, list_lines, list, &mut spots);
    let rule_text: Vec<Line> = (0..rule.height).map(|_| Line::from("│")).collect();
    frame.render_widget(Paragraph::new(rule_text).dim(), rule);
    Drawn {
        text: in_view.and_then(|opened| draw_detail(frame, app, opened, detail, &mut spots)),
        list_top: Some(list_top),
        clickable: Clickable {
            list: Some(list),
            detail: Some(detail),
            spots,
            ..Clickable::default()
        },
        ..Drawn::default()
    }
}

/// Draws the strip of the pane folded away at its edge, with a chevron halfway down: `‹` for the
/// list at the left, `›` for the detail at the right. Returns what is left for the pane on
/// screen, and the strip.
fn draw_strip(frame: &mut Frame, app: &App, area: Rect) -> (Rect, Rect) {
    let (pane, strip, chevron) = if app.list_has_keys() {
        let [pane, strip] =
            Layout::horizontal([Constraint::Fill(1), Constraint::Length(STRIP)]).areas(area);
        (pane, strip, " ›")
    } else {
        // The detail keeps its own margin, which stands in for the strip's second column.
        let [strip, pane] =
            Layout::horizontal([Constraint::Length(STRIP - 1), Constraint::Fill(1)]).areas(area);
        (pane, strip, "‹")
    };
    let mut lines = vec![Line::default(); usize::from(strip.height / 2)];
    lines.push(Line::from(chevron));
    frame.render_widget(Paragraph::new(lines).dim(), strip);
    (pane, strip)
}

/// The lines of a list `width` wide.
type ListLines = fn(&App, u16) -> ListText;

/// The lines of a list, with the entry each shows.
#[derive(Default)]
struct ListText {
    lines: Vec<Line<'static>>,
    /// For each line, the entry it shows by its place among the entries; `None` for a line that
    /// is no entry.
    entries: Vec<Option<usize>>,
    /// The line the selection starts on.
    selected_line: Option<usize>,
    /// How many entries there are so far.
    count: usize,
    buttons: Vec<ListButton>,
}

/// A button on a line of a list, `start` columns into it.
struct ListButton {
    line: usize,
    start: usize,
    width: usize,
    target: Target,
}

impl ListText {
    /// A line that is no entry: a heading, a blank line or a note.
    fn push(&mut self, line: Line<'static>) {
        self.lines.push(line);
        self.entries.push(None);
    }

    /// The lines of the next entry.
    fn entry(&mut self, lines: Vec<Line<'static>>, selected: bool) {
        if selected {
            self.selected_line = Some(self.lines.len());
        }
        self.entries
            .extend(std::iter::repeat_n(Some(self.count), lines.len()));
        self.lines.extend(lines);
        self.count += 1;
    }
}

/// Draws a list from where it was, moved only as far as keeps the selection on screen unless the
/// wheel scrolled it away; returns the first line drawn, and what of the list is on screen.
fn draw_list(
    frame: &mut Frame,
    app: &App,
    list_lines: ListLines,
    area: Rect,
    spots: &mut Vec<Spot>,
) -> (usize, ClickableList) {
    let list = list_lines(app, area.width);
    let previous = app
        .panes()
        .map_or(ListTop::FollowsSelection(0), |panes| panes.list_top);
    let top = match previous {
        ListTop::FollowsSelection(previous) => list_top(
            previous,
            list.selected_line,
            list.lines.len(),
            usize::from(area.height),
        ),
        ListTop::Scrolled(top) => {
            top.min(list.lines.len().saturating_sub(usize::from(area.height)))
        }
    };
    let scroll = u16::try_from(top).unwrap_or(u16::MAX);
    frame.render_widget(Paragraph::new(list.lines).scroll((scroll, 0)), area);
    let height = usize::from(area.height);
    let bottom = (top + height).min(list.entries.len());
    // What is out of view, counted in entries rather than lines.
    let mut indicators = Vec::new();
    let above = entries_among(&list.entries[..top.min(bottom)]);
    if above > 0 {
        indicators.push(draw_indicator(frame, row(area, 0), &format!("  ↑ {above}")));
    }
    let below = entries_among(&list.entries[bottom..]);
    if below > 0 {
        let last = area.height.saturating_sub(1);
        indicators.push(draw_indicator(
            frame,
            row(area, last),
            &format!("  ↓ {below}"),
        ));
    }
    for ListButton {
        line,
        start,
        width,
        target,
    } in list.buttons
    {
        if (top..top + height).contains(&line) {
            let spot = button(row(area, (line - top) as u16), start, width, target);
            // A button under an indicator is not on screen: a click there is one on its row.
            if !indicators
                .iter()
                .any(|indicator| indicator.intersects(spot.area))
            {
                spots.push(spot);
            }
        }
    }
    let entries = list
        .entries
        .into_iter()
        .skip(top)
        .take(usize::from(area.height))
        .collect();
    (top, ClickableList { area, entries })
}

/// How many entries have a line among `lines`; an entry of two lines counts once.
fn entries_among(lines: &[Option<usize>]) -> usize {
    let mut entries: Vec<usize> = lines.iter().flatten().copied().collect();
    entries.dedup();
    entries.len()
}

/// The row of `area` `index` rows down from its top.
fn row(area: Rect, index: u16) -> Rect {
    Rect {
        y: area.y + index,
        height: 1,
        ..area
    }
}

/// Draws `text` dim at the right end of `row`, a space either side of it, over what is there,
/// with the background of the row's end behind it; returns where it was drawn.
fn draw_indicator(frame: &mut Frame, row: Rect, text: &str) -> Rect {
    let text = format!(" {text} ");
    let width = (textwrap::core::display_width(&text) as u16).min(row.width);
    let area = Rect {
        x: row.right() - width,
        width,
        ..row
    };
    if area.is_empty() {
        return area;
    }
    let buffer = frame.buffer_mut();
    let background = buffer[(row.right() - 1, row.y)].bg;
    for x in area.left()..area.right() {
        buffer[(x, row.y)].reset();
    }
    let style = Style::new().dim().bg(background);
    buffer.set_stringn(area.x, area.y, text, usize::from(width), style);
    area
}

/// Draws where a scrolled text `area` high is, at the right end of its last row, as neovim's
/// ruler shows it: `Top`, `Bot`, or how far down as a percentage. Nothing when all of the text
/// is on screen, as for a list that fits.
fn draw_position(frame: &mut Frame, area: Rect, top: usize, last_top: usize) {
    if last_top == 0 || area.height == 0 {
        return;
    }
    let position = format!("{}%", (top * 100 / last_top).clamp(0, 100));
    draw_indicator(frame, row(area, area.height - 1), &position);
}

/// `area` without its last column, which keeps text off the right edge and out of the corner.
fn padded(area: Rect) -> Rect {
    Rect {
        width: area.width.saturating_sub(1),
        ..area
    }
}

/// The first line of a list of `lines` drawn `height` high: `previous`, moved only as far as
/// keeps the selection on screen with the line before it (a heading, maybe) and the line after
/// it (the rest of its row, maybe).
pub fn list_top(
    previous: usize,
    selected_line: Option<usize>,
    lines: usize,
    height: usize,
) -> usize {
    let mut top = previous.min(lines.saturating_sub(height));
    if let Some(line) = selected_line {
        if line < top + 1 {
            top = line.saturating_sub(1);
        }
        if line + 2 > top + height {
            top = (line + 2).saturating_sub(height);
        }
    }
    top
}

fn section(title: &str) -> Line<'static> {
    Line::from(format!(" {title}")).bold()
}

/// One row of a list, `width` wide: the selection marker, what it says cut to fit, and `right`
/// flush against the right edge; then the lines under it.
struct RowLines {
    selected: bool,
    /// The accent of the selection: full while the list has the keys.
    accent: Color,
    width: u16,
}

impl RowLines {
    fn new(app: &App, selected: bool, width: u16) -> Self {
        Self {
            selected,
            accent: accent(app, KeysAt::List),
            width,
        }
    }

    /// The keys of a row, shown only while it is selected.
    fn keys(&self, keys: &'static str) -> Option<Span<'static>> {
        self.selected.then(|| themed(keys, theme::KEY))
    }

    /// The lines of the row; `more` start right of the marker column, and are cut to fit.
    fn lines(
        &self,
        first: Vec<Span<'static>>,
        right: Option<Line<'static>>,
        more: Vec<Line<'static>>,
    ) -> Vec<Line<'static>> {
        let width = usize::from(self.width);
        let right_width = right.as_ref().map_or(0, |right| right.width() + 2);
        let mut spans = vec![self.marker()];
        spans.extend(cut_spans(first, width.saturating_sub(1 + right_width + 1)));
        let used: usize = spans.iter().map(Span::width).sum();
        match right {
            Some(right) => {
                let padding = width.saturating_sub(used + right.width() + 1).max(2);
                spans.push(Span::raw(" ".repeat(padding)));
                spans.extend(right.spans);
                spans.push(Span::raw(" "));
            }
            // Across the whole width, for the background of the selection.
            None => spans.push(Span::raw(" ".repeat(width.saturating_sub(used)))),
        }
        let mut first = Line::from(spans);
        if self.selected {
            first = first.bold();
        }
        let mut lines = vec![first];
        for line in more {
            let mut spans = vec![self.marker()];
            spans.extend(cut_spans(line.spans, width.saturating_sub(1)));
            let used: usize = spans.iter().map(Span::width).sum();
            spans.push(Span::raw(" ".repeat(width.saturating_sub(used))));
            lines.push(Line::from(spans).style(line.style));
        }
        if self.selected {
            lines = lines.into_iter().map(|line| line.bg(self.accent)).collect();
        }
        lines
    }

    /// The marker column: the bar of the selection, on every line of it.
    fn marker(&self) -> Span<'static> {
        if self.selected {
            selection_bar(self.accent)
        } else {
            Span::raw(" ")
        }
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
        themed(format!("#{}", item.id.short()), theme::ID),
        Span::raw(" "),
        Span::raw(item.title.clone()),
    ]
}

/// The keys that answer a permission request, on its row: allow first, deny last.
const ROW_ANSWERS: &str = "a / d";

/// Where `right` starts on the first line of a row drawn with it: flush against the right edge,
/// before the margin.
fn right_start(first: &Line, right: &str) -> usize {
    first
        .width()
        .saturating_sub(textwrap::core::display_width(right) + 1)
}

fn waiting_lines(entry: &Waiting, row: RowLines) -> Vec<Line<'static>> {
    match entry {
        Waiting::Permission {
            agent, item, what, ..
        } => {
            let mut first = vec![
                themed("? ", theme::ATTENTION),
                themed(agent.clone(), theme::AGENT),
            ];
            if let Some(item) = item {
                first.push(Span::raw(" on "));
                first.push(themed(format!("#{}", item.id.short()), theme::ID));
            }
            first.push(Span::raw(" wants: "));
            first.push(Span::raw(what.clone()));
            row.lines(first, row.keys(ROW_ANSWERS).map(Line::from), Vec::new())
        }
        Waiting::Blocker { item, by, text, .. } => {
            let mut first = vec![themed("⧗ ", theme::ATTENTION)];
            first.push(themed(format!("#{}", item.id.short()), theme::ID));
            first.push(Span::raw(format!(" blocker from {by}")));
            let quote = text
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or_default();
            let quote = cut_spans(
                vec![Span::raw(format!("\"{quote}\""))],
                usize::from(row.width).saturating_sub(4),
            );
            let mut more = vec![Span::raw("  ")];
            more.extend(quote);
            row.lines(
                first,
                row.keys("r").map(Line::from),
                vec![Line::from(more).dim()],
            )
        }
        Waiting::Review { item, commit } => {
            let mut first = vec![themed("◎ ", theme::ATTENTION)];
            first.push(themed(format!("#{}", item.id.short()), theme::ID));
            first.push(Span::raw(" review"));
            if let Some(commit) = commit {
                first.push(Span::raw(format!(" {}", &commit.to_string()[..8])).dim());
            }
            first.push(Span::raw(format!("  {}", item.title)));
            row.lines(first, row.keys("v").map(Line::from), Vec::new())
        }
        Waiting::Notice { item, kind, what } => {
            let mut first = vec![themed("• ", theme::ATTENTION)];
            first.extend(item_spans(item));
            let more = Line::from(format!("  {kind}: {what}")).dim();
            row.lines(first, None, vec![more])
        }
    }
}

/// How long ago `since_ms` was: `12m`, `3h05m` or `2d`.
fn age(since_ms: u64, now_ms: u64) -> String {
    duration(now_ms.saturating_sub(since_ms))
}

/// What a session does now, from its transcript.
fn activity(app: &App, session: &SessionInfo) -> Option<Line<'static>> {
    if let Place::Elsewhere(device) = &session.place {
        return Some(Line::from(format!("  on {device}")).dim());
    }
    let transcript = app.transcripts.get(&session.id)?;
    let spinner = SPINNER[(app.snapshot.now_ms / 500) as usize % SPINNER.len()];
    let line = match transcript.now() {
        Now::Calling(call) => {
            let waits = match call.asked {
                transcript::Asked::Waiting => " · waits for permission",
                transcript::Asked::Not | transcript::Asked::Answered(_) => "",
            };
            format!("  {spinner} {}{waits}", transcript::call_line(call))
        }
        Now::Writing(text) => format!("  {text}"),
        Now::Thinking => format!("  {spinner} thinking"),
        Now::Idle => return None,
    };
    Some(Line::from(line).dim())
}

fn session_lines(app: &App, session: &SessionInfo, row: RowLines) -> Vec<Line<'static>> {
    let glyph = match session.status {
        SessionStatus::Live => themed("◆ ", theme::LIVE),
        SessionStatus::Parked => themed("◇ ", theme::LIVE),
        SessionStatus::Ended { .. } => Span::raw("· ").dim(),
    };
    let mut first = vec![
        glyph,
        themed(format!("{:<9}", session.agent), theme::AGENT),
        Span::raw(" "),
    ];
    first.extend(item_spans(&session.item));
    let mut right = age(session.started_ms, app.snapshot.now_ms);
    if session.status == SessionStatus::Parked {
        right = format!("parked {right}");
    }
    row.lines(
        first,
        Some(with_context(app, session, right)),
        activity(app, session).into_iter().collect(),
    )
}

/// `right`, then how full the session's context is, `· 69k/1M`, when its agent reported it.
fn with_context(app: &App, session: &SessionInfo, right: String) -> Line<'static> {
    let mut line = Line::from(Span::raw(right).dim());
    if let Some(usage) = app.transcripts.get(&session.id).and_then(Transcript::usage) {
        line.push_span(Span::raw(" · ").dim());
        line.push_span(context_span(
            usage,
            format!("{}/{}", tokens(usage.used), tokens(usage.size)),
        ));
    }
    line
}

/// `Opus 5.5 · 69k of 1M (7%) · $0.42`: the model and the usage when the agent reports them, the
/// cost only when it reports that.
fn context_line(model: Option<&str>, usage: Option<&Usage>) -> Option<Line<'static>> {
    let mut spans = Vec::new();
    if let Some(model) = model {
        spans.push(Span::raw(model.to_owned()).dim());
    }
    if let Some(usage) = usage {
        if !spans.is_empty() {
            spans.push(Span::raw(" · ").dim());
        }
        let mut text = format!(
            "{} of {} ({}%)",
            tokens(usage.used),
            tokens(usage.size),
            context_percent(usage)
        );
        if let Some(cost) = &usage.cost {
            let amount = match cost.currency.as_str() {
                "USD" => format!("${:.2}", cost.amount),
                currency => format!("{:.2} {currency}", cost.amount),
            };
            text = format!("{text} · {amount}");
        }
        spans.push(context_span(usage, text));
    }
    (!spans.is_empty()).then(|| Line::from(spans))
}

/// `text` about `usage`: yellow once the context is nearly full, dim before.
fn context_span(usage: &Usage, text: String) -> Span<'static> {
    if context_percent(usage) >= 80 {
        themed(text, theme::ATTENTION)
    } else {
        Span::raw(text).dim()
    }
}

/// How much of the context window is used, in whole percent.
fn context_percent(usage: &Usage) -> u64 {
    (usage.used * 100 + usage.size / 2)
        .checked_div(usage.size)
        .unwrap_or(0)
}

/// A count of tokens, short: `850`, `69k`, `1M`, `1.5M`.
fn tokens(count: u64) -> String {
    let thousands = (count + 500) / 1000;
    if count < 1000 {
        count.to_string()
    } else if thousands < 1000 {
        format!("{thousands}k")
    } else {
        let millions = format!("{:.1}", count as f64 / 1_000_000.0);
        format!("{}M", millions.trim_end_matches(".0"))
    }
}

/// The rows of the Now tab under their sections.
fn now_lines(app: &App, width: u16) -> ListText {
    let snapshot = &app.snapshot;
    let selected = app.now.selection.get();
    let mut list = ListText::default();
    if !snapshot.waiting.is_empty() {
        list.push(section("WAITING ON YOU"));
        for entry in &snapshot.waiting {
            let is_selected = selected == Some(&Row::Waiting(entry.key()));
            let lines = waiting_lines(entry, RowLines::new(app, is_selected, width));
            // The keys of the selected request answer it on a click.
            if is_selected && let Some(request) = entry.request() {
                let start = right_start(&lines[0], ROW_ANSWERS);
                let line = list.lines.len();
                list.buttons.push(ListButton {
                    line,
                    start,
                    width: 1,
                    target: Target::Allow(request),
                });
                list.buttons.push(ListButton {
                    line,
                    start: start + ROW_ANSWERS.len() - 1,
                    width: 1,
                    target: Target::Deny(request),
                });
            }
            list.entry(lines, is_selected);
        }
        list.push(Line::default());
    }
    if !snapshot.working.is_empty() {
        list.push(section("WORKING"));
        for session in &snapshot.working {
            let is_selected = selected == Some(&Row::Session(session.id));
            list.entry(
                session_lines(app, session, RowLines::new(app, is_selected, width)),
                is_selected,
            );
        }
        list.push(Line::default());
    }
    if !snapshot.flagged.is_empty() {
        list.push(section("NEEDS ATTENTION"));
        for flag in &snapshot.flagged {
            let is_selected = selected == Some(&Row::Flag(flag.clone()));
            let first = vec![themed("! ", theme::WARNING), Span::raw(flag.to_string())];
            list.entry(
                RowLines::new(app, is_selected, width).lines(first, None, Vec::new()),
                is_selected,
            );
        }
    }
    if list.lines.is_empty() {
        list.push(Line::from(" Nothing waits on you, and no agent is working.").dim());
    }
    list
}

/// `active · ready · "parse"`: how the items are grouped and filtered.
fn items_heading(app: &App) -> Line<'static> {
    let grouping = match app.items.grouping {
        ItemGrouping::State => "state",
        ItemGrouping::Lane => "lane",
        ItemGrouping::Priority => "priority",
        ItemGrouping::Type => "type",
        ItemGrouping::Label => "label",
        ItemGrouping::Readiness => "readiness",
    };
    let mut parts = vec![format!("by {grouping}")];
    if let Some(readiness) = app.items.readiness.name() {
        parts.push(format!("only {readiness}"));
    }
    if !app.items.filter.is_empty() {
        parts.push(format!("\"{}\"", app.items.filter));
    }
    Line::from(format!(" {}", parts.join(" · "))).dim()
}

fn item_row_lines(item: &ItemRow, row: RowLines) -> Vec<Line<'static>> {
    let icon = render::item_icon(item.state, item.readiness);
    let first = vec![
        themed(format!("{icon} "), icon_style(item)),
        themed(format!("#{}", item.id.short()), theme::ID),
        Span::raw(" "),
        Span::raw(item.title.clone()),
    ];
    let mut annotations = Vec::new();
    match item.readiness {
        Some(Readiness::Blocked) => annotations.push("blocked".to_owned()),
        Some(Readiness::Stranded) => annotations.push("stranded".to_owned()),
        Some(Readiness::Ready | Readiness::Claimed) | None => {}
    }
    if let Some((done, total)) = item.steps {
        annotations.push(format!("{done}/{total}"));
    }
    // The annotations dim, the priority in its own style, as `wf item list` shows it.
    let mut right = Vec::new();
    if !annotations.is_empty() {
        right.push(Span::raw(annotations.join(" ")).dim());
    }
    if let Some(priority) = item.priority {
        if !right.is_empty() {
            right.push(Span::raw(" "));
        }
        right.push(themed(priority.to_string(), theme::priority(priority)));
    }
    let right = (!right.is_empty()).then(|| Line::from(right));
    row.lines(first, right, Vec::new())
}

/// The style of an item's icon: its state's, or a warning while the item is stranded.
fn icon_style(item: &ItemRow) -> anstyle::Style {
    match item.readiness {
        Some(Readiness::Stranded) => theme::readiness(Readiness::Stranded),
        Some(Readiness::Ready | Readiness::Claimed | Readiness::Blocked) | None => {
            theme::state(item.state)
        }
    }
}

/// `this week: 5 done · 3h05m start to done · 2 of 9 attempts failed · 0.5 change requests a
/// merge`, leaving out what there is nothing to say about, wrapped between its parts to `width`.
fn stats_lines(stats: &ProjectStats, width: u16) -> Vec<Line<'static>> {
    let mut parts = vec![format!("this week: {} done", stats.done_lately)];
    if let Some(lead) = stats.median_lead_ms {
        parts.push(format!("{} start to done", duration(lead)));
    }
    if stats.attempts > 0 {
        parts.push(format!(
            "{} of {} attempts failed",
            stats.failed_attempts, stats.attempts
        ));
    }
    if stats.merged > 0 {
        let per_merge = stats.change_requests as f64 / stats.merged as f64;
        parts.push(format!("{per_merge:.1} change requests a merge"));
    }
    let room = usize::from(width).saturating_sub(2);
    let mut lines: Vec<String> = Vec::new();
    for part in parts {
        match lines.last_mut() {
            Some(line) if line.chars().count() + 3 + part.chars().count() <= room => {
                line.push_str(" · ");
                line.push_str(&part);
            }
            Some(_) | None => lines.push(part),
        }
    }
    lines
        .into_iter()
        .map(|line| Line::from(format!(" {line}")).dim())
        .collect()
}

/// `coder  claude-code · opus  2/2 busy`, `1/2 live` while there is room, `0/2 idle`
fn agent_lines(agent: &AgentInfo, row: RowLines) -> Vec<Line<'static>> {
    let first = vec![
        Span::raw(agent.id.name.to_string()).bold(),
        Span::raw(format!("  {}", agent.runs_on)).dim(),
    ];
    let live = agent.stats.live;
    let load = match agent.capacity {
        Some(capacity) if live as u32 >= capacity => {
            themed(format!("{live}/{capacity} busy"), theme::LIVE)
        }
        Some(capacity) if live > 0 => themed(format!("{live}/{capacity} live"), theme::LIVE),
        Some(capacity) => Span::raw(format!("{live}/{capacity} idle")).dim(),
        None => Span::raw(format!("{live} live · not here")).dim(),
    };
    row.lines(first, Some(Line::from(load)), Vec::new())
}

/// A session under its agent: `◆ #k3j1 Parse config lazily  work 12m`.
fn agent_session_lines(app: &App, session: &SessionInfo, row: RowLines) -> Vec<Line<'static>> {
    let glyph = match session.status {
        SessionStatus::Live => themed("  ◆ ", theme::LIVE),
        SessionStatus::Parked => themed("  ◇ ", theme::LIVE),
        SessionStatus::Ended { .. } => Span::raw("  · ").dim(),
    };
    let mut first = vec![glyph];
    first.extend(item_spans(&session.item));
    let now_ms = app.snapshot.now_ms;
    let right = match session.status {
        SessionStatus::Live => format!("{} {}", session.role, age(session.started_ms, now_ms)),
        SessionStatus::Parked => format!(
            "{} parked {}",
            session.role,
            age(session.started_ms, now_ms)
        ),
        SessionStatus::Ended { reason, at_ms } => {
            format!(
                "{} {} ago",
                reason.name().replace('_', " "),
                age(at_ms, now_ms)
            )
        }
    };
    row.lines(first, Some(with_context(app, session, right)), Vec::new())
}

/// The agents with their sessions under the stats of the project.
fn agents_lines(app: &App, width: u16) -> ListText {
    let selected = app.agents.selection.get();
    let mut list = ListText::default();
    for line in stats_lines(&app.snapshot.stats, width) {
        list.push(line);
    }
    for agent in &app.snapshot.agents {
        list.push(Line::default());
        let is_selected = selected == Some(&AgentRow::Agent(agent.id.clone()));
        list.entry(
            agent_lines(agent, RowLines::new(app, is_selected, width)),
            is_selected,
        );
        for session in &agent.sessions {
            let is_selected = selected == Some(&AgentRow::Session(session.id));
            list.entry(
                agent_session_lines(app, session, RowLines::new(app, is_selected, width)),
                is_selected,
            );
        }
    }
    if app.snapshot.agents.is_empty() {
        list.push(Line::from(" no agents yet; `wf agent add` defines one").dim());
    }
    list
}

/// The items in their groups under how they are grouped.
fn items_lines(app: &App, width: u16) -> ListText {
    let selected = app.items.list.selection.get();
    let mut list = ListText::default();
    list.push(items_heading(app));
    for (name, items) in app.item_groups() {
        list.push(Line::default());
        list.push(section(&format!("{name} ({})", items.len())));
        for item in items {
            let is_selected = selected == Some(&item.id);
            list.entry(
                item_row_lines(item, RowLines::new(app, is_selected, width)),
                is_selected,
            );
        }
    }
    if list.lines.len() == 1 {
        list.push(Line::from(" no items match").dim());
    }
    list
}

/// Draws a detail under its breadcrumb; returns the scroll it was drawn at, and adds its links to
/// `spots`.
fn draw_detail(
    frame: &mut Frame,
    app: &App,
    opened: Opened,
    area: Rect,
    spots: &mut Vec<Spot>,
) -> Option<Scroll> {
    let [title, body] = Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(area);
    // The same margin as the lists.
    let body = Rect {
        x: body.x + 1,
        width: body.width.saturating_sub(1),
        ..body
    };
    spots.extend(draw_breadcrumb(frame, app, title));
    let in_view = app.snapshot.in_view.as_ref();
    // Until the snapshot has caught up with what was opened, the detail stays empty.
    let (scroll, item) = match &opened.detail {
        Detail::Item(id) => {
            let Some(InView::Item { text, .. }) = in_view
                .filter(|shown| matches!(shown, InView::Item { id: shown, .. } if shown == id))
            else {
                return None;
            };
            let text: Text = text.into_text().unwrap_or_default();
            (draw_scrolled(frame, text, opened.scroll, body), Some(*id))
        }
        Detail::Agent(id) => {
            let Some(InView::Agent { text, .. }) = in_view
                .filter(|shown| matches!(shown, InView::Agent { id: shown, .. } if shown == id))
            else {
                return None;
            };
            let text: Text = text.into_text().unwrap_or_default();
            (draw_scrolled(frame, text, opened.scroll, body), None)
        }
        Detail::Flag(flag) => {
            let Some(InView::Flag { text, .. }) = in_view
                .filter(|shown| matches!(shown, InView::Flag { flag: shown, .. } if shown == flag))
            else {
                return None;
            };
            let text: Text = text.into_text().unwrap_or_default();
            (draw_scrolled(frame, text, opened.scroll, body), None)
        }
        Detail::Session(id) => {
            let Some(InView::Session(session)) =
                in_view.filter(|shown| matches!(shown, InView::Session(shown) if shown.id == *id))
            else {
                return None;
            };
            let scroll = draw_session(frame, app, session, opened.scroll, body, spots);
            (scroll, Some(session.item.id))
        }
    };
    spots.extend(links::find(
        frame.buffer_mut(),
        body,
        item,
        Some(&opened.detail),
        &app.snapshot.links,
    ));
    Some(scroll)
}

/// Draws a session's transcript, with its permission request under it; returns the scroll it was
/// drawn at, and adds the answers to the request to `spots`.
fn draw_session(
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
                vec![Line::from(format!(" transcript on {device}")).dim()],
            ),
            (Place::Here, None) => (&[], vec![Line::from(" no transcript yet").dim()]),
            (Place::Here, Some(transcript)) => {
                let layout = transcript::Layout::new(
                    &session.agent,
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
            | Waiting::Notice { .. } => None,
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
    let shown = draw_lines(frame, lines, &open, scroll, body);
    if let Some((what, request)) = asks.first() {
        let line = Line::from(vec![
            themed("? ", theme::ATTENTION.bold()),
            Span::raw(format!("wants: {what}")),
        ]);
        let keys = [ALLOW, DENY].join(SEPARATOR);
        let row = RowLines::new(app, false, bar.width);
        let right = Line::from(themed(keys.clone(), theme::KEY));
        let first = row.lines(line.spans, Some(right), Vec::new()).remove(0);
        let allow = right_start(&first, &keys);
        let width = textwrap::core::display_width;
        let deny = allow + width(ALLOW) + width(SEPARATOR);
        spots.push(button(bar, allow, width(ALLOW), Target::Allow(*request)));
        spots.push(button(bar, deny, width(DENY), Target::Deny(*request)));
        frame.render_widget(Paragraph::new(first), bar);
    }
    shown
}

/// Draws where the detail on screen is: `Now › #k3j1 › coder · work · 12m ◆`. Its last part
/// has the accent, full while the detail has the keys. Returns the parts that go back to a detail opened before, or to the list.
fn draw_breadcrumb(frame: &mut Frame, app: &App, area: Rect) -> Vec<Spot> {
    // Each part, with how many of the details opened it goes back to; `None` for an item named
    // only for its session, which was not opened.
    let mut crumbs: Vec<(Vec<Span<'static>>, Option<usize>)> =
        vec![(vec![Span::raw(app.tab.name())], Some(0))];
    let path = app.path();
    for (index, detail) in path.iter().enumerate() {
        let depth = Some(index + 1);
        match detail {
            Detail::Item(item) => {
                crumbs.push((vec![Span::raw(format!("#{}", item.short()))], depth));
            }
            Detail::Agent(agent) => crumbs.push((vec![Span::raw(agent.name.to_string())], depth)),
            Detail::Flag(flag) => crumbs.push((vec![Span::raw(flag.heading())], depth)),
            Detail::Session(id) => {
                let Some(session) = app.session_info(*id) else {
                    crumbs.push((vec![Span::raw(id.short())], depth));
                    continue;
                };
                // A session is about an item: named, unless the way here went through it.
                let item = session.item.id;
                if crumbs
                    .last()
                    .and_then(|(crumb, _)| crumb.first())
                    .map(|span| span.content.as_ref())
                    != Some(format!("#{}", item.short()).as_str())
                {
                    let mut crumb = vec![Span::raw(format!("#{}", item.short()))];
                    // With the detail in its place, the list with its title is not on screen.
                    if !app.shows_both()
                        && let Some(title) = app.item_title(item)
                    {
                        crumb.push(Span::raw(format!(" {title}")));
                    }
                    crumbs.push((crumb, None));
                }
                crumbs.push((session_crumb(app, session), depth));
            }
        }
    }
    let accent = accent(app, KeysAt::Detail);
    let (start_of_last, end_of_last) = highlight_ends(accent);
    let last = crumbs.len() - 1;
    // Only what was opened, below the detail on top, is there to go back to; nothing while the
    // list has the keys, even with a detail kept beside it.
    let opened = if app.list_has_keys() {
        0
    } else {
        app.stack().len()
    };
    let mut spots = Vec::new();
    let mut spans = Vec::new();
    for (index, (crumb, depth)) in crumbs.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(" ›").dim());
        }
        // The last part has its start of the highlight where the others have a space.
        spans.push(if index == last {
            start_of_last.clone()
        } else {
            Span::raw(" ")
        });
        let start: usize = spans.iter().map(Span::width).sum();
        let width: usize = crumb.iter().map(Span::width).sum();
        if let Some(depth) = depth.filter(|depth| *depth < opened)
            && let Some(name) = crumb.first()
        {
            let button = Rect {
                x: area.x.saturating_add(start as u16),
                width: width as u16,
                ..area
            };
            spots.push(Spot {
                target: Target::Back {
                    depth,
                    name: name.content.trim().to_owned(),
                },
                // What a narrow window cuts off cannot be clicked.
                area: button.intersection(area),
            });
        }
        for span in crumb {
            spans.push(if index == last {
                span.patch_style(Style::new().bold().bg(accent))
            } else {
                span.dim()
            });
        }
    }
    // A breadcrumb cut short leaves out the end of the highlight, as it does the rest.
    let room = usize::from(area.width);
    let width: usize = spans.iter().map(Span::width).sum();
    let mut spans = cut_spans(spans, room);
    if width < room {
        spans.push(end_of_last);
    }
    let line = Line::from(spans);
    frame.render_widget(Paragraph::new(line), area);
    spots.retain(|spot| !spot.area.is_empty());
    spots
}

/// `coder · work · 12m ◆`
fn session_crumb(app: &App, session: &SessionInfo) -> Vec<Span<'static>> {
    let status = match session.status {
        SessionStatus::Live => themed(" ◆", theme::LIVE),
        SessionStatus::Parked => themed(" ◇ parked", theme::LIVE),
        SessionStatus::Ended { reason, .. } => Span::raw(format!(" · {}", reason.name())),
    };
    vec![
        Span::raw(format!(
            "{} · {} · {}",
            session.agent,
            session.role,
            age(session.started_ms, app.snapshot.now_ms)
        )),
        status,
    ]
}

/// Draws `text` wrapped to `area` without its last column at `scroll`, kept to what the text
/// allows; returns the scroll it was drawn at.
fn draw_scrolled(frame: &mut Frame, text: Text<'static>, scroll: Scroll, area: Rect) -> Scroll {
    let paragraph = Paragraph::new(text).wrap(Wrap { trim: false });
    let lines = paragraph.line_count(padded(area).width);
    let last_top = lines.saturating_sub(usize::from(area.height));
    let scroll = scroll.within(last_top);
    let top = u16::try_from(scroll.top()).unwrap_or(u16::MAX);
    frame.render_widget(paragraph.scroll((top, 0)), padded(area));
    draw_position(frame, area, scroll.top(), last_top);
    scroll
}

/// Draws `lines` then `more`, wrapped already to `area` without its last column, at `scroll`,
/// kept to what they allow; returns the scroll they were drawn at. Only the lines on screen are
/// handed to ratatui, so a frame costs what is shown rather than all there is.
fn draw_lines(
    frame: &mut Frame,
    lines: &[Line<'static>],
    more: &[Line<'static>],
    scroll: Scroll,
    area: Rect,
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
    draw_position(frame, area, scroll.top(), last_top);
    scroll
}

fn draw_log(frame: &mut Frame, app: &App, area: Rect, spots: &mut Vec<Spot>) -> Option<Scroll> {
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
        frame.render_widget(Paragraph::new(empty).dim(), area);
        return None;
    }
    let text: Text = text.into_text().unwrap_or_default();
    let scroll = draw_scrolled(frame, text, app.log.scroll, area);
    spots.extend(links::find(
        frame.buffer_mut(),
        area,
        None,
        None,
        &app.snapshot.links,
    ));
    Some(scroll)
}

/// The latest event, or why the last refresh failed.
fn latest_line(app: &App) -> Line<'static> {
    match &app.status {
        Some(Status::Done { what, .. }) => {
            return Line::from(format!(" ✓ {what}")).style(theme::SUCCESS);
        }
        Some(Status::ActionFailed { error, .. } | Status::RefreshFailed(error)) => {
            let first = error.lines().next().unwrap_or_default();
            return Line::from(format!(" error: {first}")).style(theme::ERROR);
        }
        None => {}
    }
    match app.log.lines.last() {
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

/// Draws the keys that work on what is on screen; returns them as buttons, but for `j/k`.
fn draw_hints(frame: &mut Frame, app: &App, area: Rect) -> Vec<Spot> {
    let hints = hints(app);
    let request = app.subjects().into_iter().find_map(Waiting::request);
    let mut spots = Vec::new();
    let mut start = 1;
    for hint in &hints {
        let width = textwrap::core::display_width(hint);
        if let Some((key, what)) = hint.split_once(' ')
            && let Some(target) = hint_target(key, what, request)
        {
            spots.push(button(area, start, width, target));
        }
        start += width + textwrap::core::display_width(SEPARATOR);
    }
    let line = Line::from(format!(" {}", hints.join(SEPARATOR))).dim();
    frame.render_widget(Paragraph::new(line), area);
    spots.retain(|spot| !spot.area.is_empty());
    spots
}

/// Between two hints.
const SEPARATOR: &str = " · ";

/// The hints that answer a permission request.
const ALLOW: &str = "a allow";
const DENY: &str = "d deny";

/// What a click on the hint for `key` does: what the key does, but for allow, which asks for a
/// second click; `None` for a hint that is no button. `request` is the permission request the
/// keys answer.
fn hint_target(key: &'static str, what: &'static str, request: Option<Request>) -> Option<Target> {
    let code = match key {
        "a" => return request.map(Target::Allow),
        "d" => return request.map(Target::Deny),
        "enter" => KeyCode::Enter,
        "esc" => KeyCode::Esc,
        "j/k" => return None,
        _ => {
            let mut characters = key.chars();
            match (characters.next(), characters.next()) {
                (Some(character), None) => KeyCode::Char(character),
                _ => return None,
            }
        }
    };
    Some(Target::Key { code, key, what })
}

/// The keys that work on what is on screen: `esc back`, `v review`.
fn hints(app: &App) -> Vec<&'static str> {
    let mut acts: Vec<&str> = Vec::new();
    for entry in app.subjects() {
        let keys: &[&str] = match entry {
            Waiting::Permission { .. } => &[ALLOW, DENY],
            Waiting::Blocker { .. } => &["r answer", "R in editor"],
            Waiting::Review { .. } => &["v review", "D diff"],
            Waiting::Notice { .. } => &[],
        };
        for key in keys {
            if !acts.contains(key) {
                acts.push(key);
            }
        }
    }
    let item_session = matches!(
        &app.snapshot.in_view,
        Some(InView::Item {
            session: Some(_),
            ..
        })
    );
    // A detail kept while the list has the keys takes none.
    let opened = app.stack().last().filter(|_| !app.list_has_keys());
    let navigation: &[&str] = match (app.tab, opened.map(|opened| &opened.detail)) {
        (_, Some(Detail::Session(_))) => &["esc back", "j/k scroll", "i item", "f full"],
        (_, Some(Detail::Item(_))) if item_session => &["esc back", "j/k scroll", "enter session"],
        (_, Some(Detail::Item(_) | Detail::Agent(_) | Detail::Flag(_))) => {
            &["esc back", "j/k scroll"]
        }
        (Tab::Now, None) => &["enter open", "j/k move"],
        (Tab::Items, None) => &["enter open", "o group", "/ filter", "F ready"],
        (Tab::Log, None) if !app.log.filter.is_empty() => &["/ change filter", "esc clear"],
        (Tab::Log, None) => &["/ filter", "j/k scroll"],
        (Tab::Agents, None) => &["enter open", "j/k move"],
    };
    // What follows what is added, once scrolled away from its end.
    let followed = match (app.tab, opened) {
        (_, Some(opened)) => match opened.detail {
            Detail::Session(_) => Some(opened.scroll),
            Detail::Item(_) | Detail::Agent(_) | Detail::Flag(_) => None,
        },
        (Tab::Log, None) => Some(app.log.scroll),
        (Tab::Now | Tab::Items | Tab::Agents, None) => None,
    };
    let mut hints = navigation.to_vec();
    if let Some(Scroll::At(_)) = followed {
        hints.push("G follow");
    }
    hints.extend(acts);
    hints.push("? keys");
    hints
}

//! How the dashboard looks: a function of its [`App`].

use ansi_to_tui::IntoText;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use wf_core::graph::Readiness;
use wf_core::id::HashId;
use wf_core::policy::StateCategory;

use super::app::{AgentRow, App, Choice, Drawn, Opened, Prompt, Row, Scroll, Status, Tab};
use super::snapshot::{
    AgentInfo, InView, ItemRef, Place, SessionInfo, SessionStatus, Waiting, duration,
};
use super::snapshot::{Detail, ItemRow};
use crate::cli::ItemGrouping;
use crate::render;
use crate::transcript::{self, Format, Now, Transcript};
use wf_core::stats::ProjectStats;

/// The background of the selected row while the list has the keys.
const SELECTION: Color = Color::DarkGray;

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
    draw_header(frame, app, header);
    frame.render_widget(Paragraph::new(counts_line(app)), counts);
    draw_rule(frame, rule);
    let shown = draw_body(frame, app, body);
    draw_rule(frame, rule_below);
    frame.render_widget(Paragraph::new(latest_line(app)), latest);
    match &app.prompt {
        Some(Prompt::Choose(choice)) => {
            draw_choice(frame, choice, body);
            frame.render_widget(
                Paragraph::new(Line::from(" j/k move · enter pick · esc cancel").dim()),
                keys,
            );
        }
        Some(Prompt::Keys) => {
            draw_keys(frame, body);
            frame.render_widget(Paragraph::new(Line::from(" any key closes").dim()), keys);
        }
        Some(prompt) => draw_prompt(frame, app, prompt, keys),
        None => frame.render_widget(Paragraph::new(keys_line(app)), keys),
    }
    shown
}

/// A list to pick from, at the bottom of the body.
fn draw_choice(frame: &mut Frame, choice: &Choice, body: Rect) {
    let (title, options): (String, Vec<String>) = match choice {
        Choice::Agent { item, agents, .. } => (
            format!(" start on #{} ", item.short()),
            agents.iter().map(ToString::to_string).collect(),
        ),
        Choice::State { item, states, .. } => (
            format!(" move #{} to ", item.short()),
            states
                .iter()
                .map(|(name, category)| render::state_choice(&name.to_string(), *category))
                .collect(),
        ),
    };
    let lines: Vec<Line> = options
        .into_iter()
        .enumerate()
        .map(|(index, option)| {
            if index == choice.index() {
                Line::from(format!("▸ {option}")).bold().reversed()
            } else {
                Line::from(format!("  {option}"))
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
    ("1-4 tab", "switch tabs"),
    ("j/k g/G", "move, top, end (and follow)"),
    ("enter esc", "open, go back"),
    ("a d", "allow, deny a permission request"),
    ("r R", "answer a blocker here, in $EDITOR"),
    ("v D", "review, show the diff"),
    ("s x", "start a session, stop one"),
    ("m c e", "move, comment on, edit an item"),
    ("i f", "a session's item, full transcript"),
    ("o / F", "items: grouping, filter, readiness"),
    ("q", "quit"),
];

fn draw_keys(frame: &mut Frame, body: Rect) {
    let lines: Vec<Line> = KEYS
        .iter()
        .map(|(keys, what)| {
            Line::from(vec![
                Span::raw(format!(" {keys:<10}")).fg(Color::Yellow),
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
        Paragraph::new(lines).block(Block::bordered().title(" keys ")),
        area,
    );
}

/// The line the footer asks for, with the cursor in it.
fn draw_prompt(frame: &mut Frame, app: &App, prompt: &Prompt, area: Rect) {
    let question = |text: String| Line::from(text).bold();
    let (label, input) = match prompt {
        Prompt::Filter(input) => (" / ".to_owned(), input),
        Prompt::Comment { item, input } => (format!(" comment on #{}: ", item.short()), input),
        Prompt::Stop { session } => {
            let agent = app
                .snapshot
                .working
                .iter()
                .find(|working| working.id == *session)
                .map_or_else(String::new, |working| {
                    format!("{} on #{} ", working.agent, working.item.id.short())
                });
            let line = question(format!(" stop {agent}now? y/n"));
            frame.render_widget(Paragraph::new(line), area);
            return;
        }
        Prompt::Choose(_) | Prompt::Keys => return,
        Prompt::Answer { blocker, input, .. } => {
            (format!(" ⧗ {} answer: ", blocker.short()), input)
        }
        Prompt::Verdict { item } => {
            let line = Line::from(vec![
                Span::raw(format!(" ◎ #{}: ", item.short())).bold(),
                Span::raw("a approve · r changes · D diff · esc"),
            ]);
            frame.render_widget(Paragraph::new(line), area);
            return;
        }
    };
    let label_width = textwrap::core::display_width(&label);
    let width = usize::from(area.width).saturating_sub(label_width + 1);
    let scroll = input.visual_scroll(width);
    let line = Line::from(vec![
        Span::raw(label).bold(),
        Span::raw(input.value().chars().skip(scroll).collect::<String>()),
    ]);
    frame.render_widget(Paragraph::new(line), area);
    let cursor = (input.visual_cursor().max(scroll) - scroll) as u16;
    frame.set_cursor_position((area.x + label_width as u16 + cursor, area.y));
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let mut tabs = Vec::new();
    for tab in Tab::ALL {
        if !tabs.is_empty() {
            tabs.push(Span::raw(" "));
        }
        let style = if tab == app.tab {
            Style::new().bold().reversed()
        } else {
            Style::new().dim()
        };
        // A column of the highlight on either side of the label.
        let mut label = format!(" {} {}", tab.key(), tab.name());
        if tab == Tab::Now && !app.snapshot.waiting.is_empty() {
            label.push_str(&format!("·{}", app.snapshot.waiting.len()));
        }
        label.push(' ');
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

fn draw_body(frame: &mut Frame, app: &App, area: Rect) -> Drawn {
    match app.tab {
        Tab::Now => draw_split(frame, app, area, now_lines),
        Tab::Items => draw_split(frame, app, area, items_lines),
        Tab::Log => Drawn {
            text: draw_log(frame, app, area),
            list_top: None,
        },
        Tab::Agents => draw_split(frame, app, area, agents_lines),
    }
}

/// The list, and the detail on screen: in its place, or right of it in a wide window.
fn draw_split(frame: &mut Frame, app: &App, area: Rect, list_lines: ListLines) -> Drawn {
    let in_view = app.in_view();
    if !app.is_wide() {
        return match in_view {
            Some(opened) => Drawn {
                text: draw_detail(frame, app, opened, area),
                list_top: None,
            },
            None => Drawn {
                text: None,
                list_top: Some(draw_list(frame, app, list_lines, area)),
            },
        };
    }
    let [list, rule, detail] = Layout::horizontal([
        Constraint::Length(app.list_width()),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(area);
    let list_top = draw_list(frame, app, list_lines, list);
    let rule_text: Vec<Line> = (0..rule.height).map(|_| Line::from("│")).collect();
    frame.render_widget(Paragraph::new(rule_text).dim(), rule);
    Drawn {
        text: in_view.and_then(|opened| draw_detail(frame, app, opened, detail)),
        list_top: Some(list_top),
    }
}

/// The lines of a list `width` wide, and the line its selection starts on.
type ListLines = fn(&App, u16) -> (Vec<Line<'static>>, Option<usize>);

/// Draws a list from where it was, moved only as far as keeps the selection on screen; returns
/// the first line drawn.
fn draw_list(frame: &mut Frame, app: &App, list_lines: ListLines, area: Rect) -> usize {
    let (lines, selected_line) = list_lines(app, area.width);
    let previous = app.panes().map_or(0, |panes| panes.list_top);
    let top = list_top(
        previous,
        selected_line,
        lines.len(),
        usize::from(area.height),
    );
    let scroll = u16::try_from(top).unwrap_or(u16::MAX);
    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), area);
    top
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
    /// Whether the keys go to the list, rather than to a detail opened from it.
    focused: bool,
    width: u16,
}

impl RowLines {
    fn new(app: &App, selected: bool, width: u16) -> Self {
        Self {
            selected,
            focused: app.list_has_keys(),
            width,
        }
    }

    /// The keys of a row, shown only while it is selected.
    fn keys(&self, keys: &'static str) -> Option<Span<'static>> {
        self.selected.then(|| Span::raw(keys).fg(Color::Yellow))
    }

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
        let used: usize = spans.iter().map(Span::width).sum();
        match right {
            Some(right) => {
                let padding = width.saturating_sub(used + right.width() + 1).max(2);
                spans.push(Span::raw(" ".repeat(padding)));
                spans.push(right);
                spans.push(Span::raw(" "));
            }
            // Across the whole width, for the background of the selection.
            None => spans.push(Span::raw(" ".repeat(width.saturating_sub(used)))),
        }
        let mut first = Line::from(spans);
        if self.selected {
            first = first.bold();
            // While the list has the keys, the selection stands out; behind an open detail it
            // only keeps its marker.
            if self.focused {
                first = first.bg(SELECTION);
            }
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
            row.lines(first, row.keys("a / d"), Vec::new())
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
            row.lines(first, row.keys("r"), vec![Line::from(more).dim()])
        }
        Waiting::Review { item, commit } => {
            let mut first = vec![Span::raw("◎ ").fg(Color::Yellow)];
            first.push(Span::raw(format!("#{}", item.id.short())).fg(Color::Cyan));
            first.push(Span::raw(" review"));
            if let Some(commit) = commit {
                first.push(Span::raw(format!(" {}", &commit.to_string()[..8])).dim());
            }
            first.push(Span::raw(format!("  {}", item.title)));
            row.lines(first, row.keys("v"), Vec::new())
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

/// How long ago `since_ms` was: `12m`, `3h05m` or `2d`.
fn age(since_ms: u64, now_ms: u64) -> String {
    duration(now_ms.saturating_sub(since_ms))
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
        SessionStatus::Ended { .. } => Span::raw("· ").dim(),
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
            let rows = waiting_lines(entry, RowLines::new(app, selected == Some(&row), width));
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
                RowLines::new(app, selected == Some(&row), width),
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
            let rows =
                RowLines::new(app, selected == Some(&row), width).lines(first, None, Vec::new());
            add(&mut lines, row, rows);
        }
    }
    if lines.is_empty() {
        lines.push(Line::from(" Nothing waits on you, and no agent is working.").dim());
    }
    (lines, selected_line)
}

/// `in_progress · ready · "parse"`: how the items are grouped and filtered.
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
    let icon = render::item_icon(item.state_category, item.readiness);
    let first = vec![
        Span::raw(format!("{icon} ")).fg(state_color(item)),
        Span::raw(format!("#{}", item.id.short())).fg(Color::Cyan),
        Span::raw(" "),
        Span::raw(item.title.clone()),
    ];
    let mut right = Vec::new();
    if let Some(readiness @ (Readiness::Blocked | Readiness::Stranded)) = item.readiness {
        right.push(
            match readiness {
                Readiness::Blocked => "blocked",
                _ => "stranded",
            }
            .to_owned(),
        );
    }
    if let Some((done, total)) = item.steps {
        right.push(format!("{done}/{total}"));
    }
    if let Some(priority) = item.priority {
        right.push(priority.to_string());
    }
    let right = (!right.is_empty()).then(|| Span::raw(right.join(" ")).dim());
    row.lines(first, right, Vec::new())
}

fn state_color(item: &ItemRow) -> Color {
    if item.readiness == Some(Readiness::Stranded) {
        return Color::Red;
    }
    match item.state_category {
        Some(StateCategory::Open) => Color::Green,
        Some(StateCategory::Active) => Color::Blue,
        Some(StateCategory::Review) => Color::Yellow,
        Some(StateCategory::Done) => Color::Magenta,
        Some(StateCategory::Cancelled | StateCategory::Inactive) | None => Color::DarkGray,
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

/// `coder  claude-code · opus  2/2 busy`
fn agent_lines(agent: &AgentInfo, row: RowLines) -> Vec<Line<'static>> {
    let first = vec![
        Span::raw(agent.id.name.to_string()).bold(),
        Span::raw(format!("  {}", agent.runs_on)).dim(),
    ];
    let live = agent.stats.live;
    let load = match agent.capacity {
        Some(capacity) if live as u32 >= capacity => {
            Span::raw(format!("{live}/{capacity} busy")).fg(Color::Blue)
        }
        Some(capacity) => Span::raw(format!("{live}/{capacity} idle")).dim(),
        None => Span::raw(format!("{live} live · not here")).dim(),
    };
    row.lines(first, Some(load), Vec::new())
}

/// A session under its agent: `◆ #k3j1 Parse config lazily  work 12m`.
fn agent_session_lines(app: &App, session: &SessionInfo, row: RowLines) -> Vec<Line<'static>> {
    let glyph = match session.status {
        SessionStatus::Live => Span::raw("  ◆ ").fg(Color::Blue),
        SessionStatus::Parked => Span::raw("  ◇ ").fg(Color::Blue),
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
    row.lines(first, Some(Span::raw(right).dim()), Vec::new())
}

/// The agents with their sessions under the stats of the project, and the line the selection
/// starts on.
fn agents_lines(app: &App, width: u16) -> (Vec<Line<'static>>, Option<usize>) {
    let selected = app.agents.selection.get();
    let mut lines = stats_lines(&app.snapshot.stats, width);
    let mut selected_line = None;
    for agent in &app.snapshot.agents {
        lines.push(Line::default());
        let row = AgentRow::Agent(agent.id.clone());
        if selected == Some(&row) {
            selected_line = Some(lines.len());
        }
        lines.extend(agent_lines(
            agent,
            RowLines::new(app, selected == Some(&row), width),
        ));
        for session in &agent.sessions {
            let row = AgentRow::Session(session.id);
            if selected == Some(&row) {
                selected_line = Some(lines.len());
            }
            lines.extend(agent_session_lines(
                app,
                session,
                RowLines::new(app, selected == Some(&row), width),
            ));
        }
    }
    if app.snapshot.agents.is_empty() {
        lines.push(Line::from(" no agents yet; `wf agent add` defines one").dim());
    }
    (lines, selected_line)
}

/// The items in their groups under how they are grouped, and the line the selection starts on.
fn items_lines(app: &App, width: u16) -> (Vec<Line<'static>>, Option<usize>) {
    let selected = app.items.list.selection.get();
    let mut lines = vec![items_heading(app)];
    let mut selected_line = None;
    for (name, items) in app.item_groups() {
        lines.push(Line::default());
        lines.push(section(&format!("{name} ({})", items.len())));
        for item in items {
            if selected == Some(&item.id) {
                selected_line = Some(lines.len());
            }
            let row = RowLines::new(app, selected == Some(&item.id), width);
            lines.extend(item_row_lines(item, row));
        }
    }
    if lines.len() == 1 {
        lines.push(Line::from(" no items match").dim());
    }
    (lines, selected_line)
}

fn draw_detail(frame: &mut Frame, app: &App, opened: Opened, area: Rect) -> Option<Scroll> {
    let [title, body] = Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(area);
    // The same margin as the lists.
    let body = Rect {
        x: body.x + 1,
        width: body.width.saturating_sub(1),
        ..body
    };
    frame.render_widget(Paragraph::new(breadcrumb(app, title.width)), title);
    let in_view = app.snapshot.in_view.as_ref();
    // Until the snapshot has caught up with what was opened, the detail stays empty.
    match &opened.detail {
        Detail::Item(id) => {
            let Some(InView::Item { text, .. }) = in_view
                .filter(|shown| matches!(shown, InView::Item { id: shown, .. } if shown == id))
            else {
                return None;
            };
            let text: Text = text.into_text().unwrap_or_default();
            Some(draw_scrolled(frame, text, opened.scroll, body, true))
        }
        Detail::Agent(id) => {
            let Some(InView::Agent { text, .. }) = in_view
                .filter(|shown| matches!(shown, InView::Agent { id: shown, .. } if shown == id))
            else {
                return None;
            };
            Some(draw_scrolled(
                frame,
                Text::from(text.clone()),
                opened.scroll,
                body,
                true,
            ))
        }
        Detail::Session(id) => {
            let Some(InView::Session(session)) =
                in_view.filter(|shown| matches!(shown, InView::Session(shown) if shown.id == *id))
            else {
                return None;
            };
            Some(draw_session(frame, app, session, opened.scroll, body))
        }
    }
}

fn draw_session(
    frame: &mut Frame,
    app: &App,
    session: &SessionInfo,
    scroll: Scroll,
    body: Rect,
) -> Scroll {
    let text = match (&session.place, app.transcripts.get(&session.id)) {
        (Place::Elsewhere(device), _) => {
            Text::from(Line::from(format!(" transcript on {device}")).dim())
        }
        (Place::Here, None) => Text::from(Line::from(" no transcript yet").dim()),
        (Place::Here, Some(transcript)) => {
            let width = usize::from(body.width).max(20);
            transcript_text(app, &session.agent, transcript, width)
        }
    };
    let asks: Vec<&str> = app
        .snapshot
        .waiting
        .iter()
        .filter_map(|entry| match entry {
            Waiting::Permission {
                session: asking,
                what,
                ..
            } if *asking == session.id => Some(what.as_str()),
            Waiting::Permission { .. }
            | Waiting::Blocker { .. }
            | Waiting::Review { .. }
            | Waiting::Notice { .. } => None,
        })
        .collect();
    let [body, bar] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(u16::from(!asks.is_empty())),
    ])
    .areas(body);
    let shown = draw_scrolled(frame, text, scroll, body, false);
    if let Some(what) = asks.first() {
        let line = Line::from(vec![
            Span::raw("? ").fg(Color::Yellow).bold(),
            Span::raw(format!("wants: {what}")),
        ]);
        let keys = Span::raw("a allow · d deny").fg(Color::Yellow);
        let row = RowLines::new(app, false, bar.width);
        let mut lines = row.lines(line.spans, Some(keys), Vec::new());
        frame.render_widget(Paragraph::new(lines.remove(0)), bar);
    }
    shown
}

/// Where the detail on screen is: `Now › #k3j1 › coder · work · 12m ◆`. Its last part stands
/// out while the detail has the keys, and is dim while it only shows what the list selects.
fn breadcrumb(app: &App, width: u16) -> Line<'static> {
    let separator = || Span::raw(" › ").dim();
    let mut crumbs: Vec<Vec<Span<'static>>> = vec![vec![Span::raw(app.tab.name())]];
    let path = app.path();
    for detail in &path {
        match detail {
            Detail::Item(item) => crumbs.push(vec![Span::raw(format!("#{}", item.short()))]),
            Detail::Agent(agent) => crumbs.push(vec![Span::raw(agent.name.to_string())]),
            Detail::Session(id) => {
                let Some(session) = app.session_info(*id) else {
                    crumbs.push(vec![Span::raw(id.short())]);
                    continue;
                };
                // A session is about an item: named, unless the way here went through it.
                let item = session.item.id;
                if crumbs
                    .last()
                    .and_then(|crumb| crumb.first())
                    .map(|span| span.content.as_ref())
                    != Some(format!("#{}", item.short()).as_str())
                {
                    let mut crumb = vec![Span::raw(format!("#{}", item.short()))];
                    // In a narrow window the list with its title is not on screen.
                    if !app.is_wide()
                        && let Some(title) = app.item_title(item)
                    {
                        crumb.push(Span::raw(format!(" {title}")));
                    }
                    crumbs.push(crumb);
                }
                crumbs.push(session_crumb(app, session));
            }
        }
    }
    let focused = !app.list_has_keys();
    let last = crumbs.len() - 1;
    let mut spans = vec![Span::raw(" ")];
    for (index, crumb) in crumbs.into_iter().enumerate() {
        if index > 0 {
            spans.push(separator());
        }
        for span in crumb {
            spans.push(match (index == last, focused) {
                (true, true) => span.bold(),
                (true, false) | (false, _) => span.dim(),
            });
        }
    }
    Line::from(cut_spans(spans, usize::from(width)))
}

/// `coder · work · 12m ◆`
fn session_crumb(app: &App, session: &SessionInfo) -> Vec<Span<'static>> {
    let status = match session.status {
        SessionStatus::Live => Span::raw(" ◆").fg(Color::Blue),
        SessionStatus::Parked => Span::raw(" ◇ parked").fg(Color::Blue),
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

/// The whole transcript laid out for `width`: its blocks, then what is still open.
fn transcript_text(app: &App, agent: &str, transcript: &Transcript, width: usize) -> Text<'static> {
    let detail = if app.full_transcripts {
        transcript::Detail::Full
    } else {
        transcript::Detail::Abridged
    };
    let layout =
        transcript::Layout::new(agent, width, Format::Styled, detail, app.time_zone.clone());
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

/// Draws `text` at `scroll`, kept to what the text allows; returns the scroll it was drawn at.
fn draw_scrolled(
    frame: &mut Frame,
    text: Text<'static>,
    scroll: Scroll,
    area: Rect,
    wrap: bool,
) -> Scroll {
    let height = usize::from(area.height);
    let mut paragraph = Paragraph::new(text);
    if wrap {
        paragraph = paragraph.wrap(Wrap { trim: false });
    }
    let lines = paragraph.line_count(area.width);
    let last_top = u16::try_from(lines.saturating_sub(height)).unwrap_or(u16::MAX);
    let scroll = scroll.within(last_top);
    frame.render_widget(paragraph.scroll((scroll.top(), 0)), area);
    scroll
}

fn draw_log(frame: &mut Frame, app: &App, area: Rect) -> Option<Scroll> {
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
    Some(draw_scrolled(frame, text, app.log.scroll, area, true))
}

/// The latest event, or why the last refresh failed.
fn latest_line(app: &App) -> Line<'static> {
    match &app.status {
        Some(Status::Done(what)) => return Line::from(format!(" ✓ {what}")).fg(Color::Green),
        Some(Status::ActionFailed(error) | Status::RefreshFailed(error)) => {
            let first = error.lines().next().unwrap_or_default();
            return Line::from(format!(" error: {first}")).fg(Color::Red);
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

/// The keys that work on what is on screen.
fn keys_line(app: &App) -> Line<'static> {
    let mut acts: Vec<&str> = Vec::new();
    for entry in app.subjects() {
        let keys: &[&str] = match entry {
            Waiting::Permission { .. } => &["a allow", "d deny"],
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
    let opened = app.stack().and_then(|stack| stack.last());
    let navigation = match (app.tab, opened.map(|opened| &opened.detail)) {
        (_, Some(Detail::Session(_))) => "esc back · j/k scroll · i item · f full",
        (_, Some(Detail::Item(_))) if item_session => "esc back · j/k scroll · enter session",
        (_, Some(Detail::Item(_) | Detail::Agent(_))) => "esc back · j/k scroll",
        (Tab::Now, None) => "enter open · j/k move",
        (Tab::Items, None) => "enter open · o group · / filter · F ready",
        (Tab::Log, None) if !app.log.filter.is_empty() => "/ change filter · esc clear",
        (Tab::Log, None) => "/ filter · j/k scroll",
        (Tab::Agents, None) => "enter open · j/k move",
    };
    // What follows what is added, once scrolled away from its end.
    let followed = match (app.tab, opened) {
        (Tab::Log, _) => Some(app.log.scroll),
        (_, Some(opened)) => match opened.detail {
            Detail::Session(_) => Some(opened.scroll),
            Detail::Item(_) | Detail::Agent(_) => None,
        },
        (Tab::Now | Tab::Items | Tab::Agents, None) => None,
    };
    let mut parts: Vec<&str> = vec![navigation];
    if let Some(Scroll::At(_)) = followed {
        parts.push("G follow");
    }
    parts.extend(acts);
    parts.push("? keys");
    Line::from(format!(" {}", parts.join(" · "))).dim()
}

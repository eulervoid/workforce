//! The lists of the tabs: their rows, and the list drawn beside or in place of a detail.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use wf_core::graph::Readiness;
use wf_core::id::HashId;

use super::chrome::button;
use super::{KeysAt, SPINNER, Themed, accent, list_top, selection_bar, themed};
use crate::cli::{ItemGrouping, ItemOrder};
use crate::dashboard::app::{AgentRow, App, ClickableList, FirstRow, NowRow, Spot, Target};
use crate::dashboard::snapshot::{AgentInfo, InboxEntry, Place, SessionInfo};
use crate::render;
use crate::text::{duration, prefix_within};
use crate::theme;
use crate::transcript::{self, Now, Transcript, Usage};
use crate::view::{AgentView, ItemSummary, SessionStatus};
use wf_core::id::ItemId;
use wf_core::stats::ProjectStats;

/// The lines of a list `width` wide.
pub(super) type ListLines = fn(&App, u16) -> ListText;

/// The lines of a list, with the entry each shows.
#[derive(Default)]
pub(super) struct ListText {
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
pub(super) struct ListButton {
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
pub(super) fn draw_list(
    frame: &mut Frame,
    app: &App,
    list_lines: ListLines,
    area: Rect,
    spots: &mut Vec<Spot>,
) -> (usize, ClickableList) {
    let previous = app
        .panes()
        .map_or(FirstRow::Follows(0), |panes| panes.list_top);
    // Nothing of it fits: it stays where it was.
    if area.is_empty() {
        let entries = Vec::new();
        return (previous.line(), ClickableList { area, entries });
    }
    let list = list_lines(app, area.width);
    let top = match previous {
        FirstRow::Follows(previous) => list_top(
            previous,
            list.selected_line,
            list.lines.len(),
            usize::from(area.height),
        ),
        FirstRow::Scrolled(top) => {
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
        indicators.push(draw_indicator(
            frame,
            row(area, 0),
            &format!("  ↑ {above}"),
            app.base,
        ));
    }
    let below = entries_among(&list.entries[bottom..]);
    if below > 0 {
        let last = area.height.saturating_sub(1);
        indicators.push(draw_indicator(
            frame,
            row(area, last),
            &format!("  ↓ {below}"),
            app.base,
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
pub(super) fn entries_among(lines: &[Option<usize>]) -> usize {
    let mut entries: Vec<usize> = lines.iter().flatten().copied().collect();
    entries.dedup();
    entries.len()
}

/// The row of `area` `index` rows down from its top.
pub(super) fn row(area: Rect, index: u16) -> Rect {
    Rect {
        y: area.y + index,
        height: 1,
        ..area
    }
}

/// Draws `text` dim at the right end of `row`, a space either side of it, over what is there,
/// with the background of the row's end behind it; returns where it was drawn.
pub(super) fn draw_indicator(frame: &mut Frame, row: Rect, text: &str, base: Style) -> Rect {
    if row.is_empty() {
        return row;
    }
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
    let style = base.patch(theme::text_dimmed()).bg(background);
    buffer.set_stringn(area.x, area.y, text, usize::from(width), style);
    area
}

pub(super) fn section(title: &str) -> Line<'static> {
    Line::from(format!(" {title}")).themed(theme::text_bold())
}

/// One row of a list, `width` wide: the selection marker, what it says cut to fit, and `right`
/// flush against the right edge; then the lines under it.
pub(super) struct RowLines {
    selected: bool,
    /// The accent of the selection: full while the list has the keys.
    accent: Color,
    width: u16,
}

impl RowLines {
    pub(super) fn new(app: &App, selected: bool, width: u16) -> Self {
        Self {
            selected,
            accent: accent(app, KeysAt::List),
            width,
        }
    }

    /// The keys of a row, shown only while it is selected.
    fn keys(&self, keys: &'static str) -> Option<Span<'static>> {
        self.selected.then(|| themed(keys, theme::key()))
    }

    /// The lines of the row; `more` start right of the marker column, and are cut to fit.
    pub(super) fn lines(
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
            first = first.themed(theme::text_bold());
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
pub(super) fn cut_spans(spans: Vec<Span<'static>>, room: usize) -> Vec<Span<'static>> {
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
        let text = prefix_within(&span.content, left);
        kept.push(Span::styled(format!("{}…", text.trim_end()), span.style));
        break;
    }
    kept
}

pub(super) fn item_spans(id: ItemId, title: &str) -> Vec<Span<'static>> {
    vec![
        themed(format!("#{}", id.short()), theme::id()),
        Span::raw(" "),
        Span::raw(title.to_owned()),
    ]
}

/// The keys that answer a permission request, on its row: allow first, deny last.
pub(super) const ROW_ANSWERS: &str = "a / d";

/// Where `right` starts on the first line of a row drawn with it: flush against the right edge,
/// before the margin.
pub(super) fn right_start(first: &Line, right: &str) -> usize {
    first
        .width()
        .saturating_sub(textwrap::core::display_width(right) + 1)
}

pub(super) fn waiting_lines(entry: &InboxEntry, row: RowLines) -> Vec<Line<'static>> {
    match entry {
        InboxEntry::Permission {
            agent, item, what, ..
        } => {
            let mut first = vec![
                themed("? ", theme::attention()),
                themed(agent.clone(), theme::agent()),
            ];
            if let Some(item) = item {
                first.push(Span::raw(" on "));
                first.push(themed(format!("#{}", item.id.short()), theme::id()));
            }
            first.push(Span::raw(" wants: "));
            first.push(Span::raw(what.clone()));
            row.lines(first, row.keys(ROW_ANSWERS).map(Line::from), Vec::new())
        }
        InboxEntry::Blocker { item, by, text, .. } => {
            let mut first = vec![themed("⧗ ", theme::attention())];
            first.push(themed(format!("#{}", item.id.short()), theme::id()));
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
                vec![Line::from(more).themed(theme::text_dimmed())],
            )
        }
        InboxEntry::Review {
            item,
            commit,
            stale,
        } => {
            let mut first = vec![themed("◎ ", theme::attention())];
            first.push(themed(format!("#{}", item.id.short()), theme::id()));
            first.push(Span::raw(" review"));
            if let Some(commit) = commit {
                first.push(Span::raw(format!(" {}", commit.short())).themed(theme::text_dimmed()));
            }
            first.push(Span::raw(format!("  {}", item.title)));
            let more = stale
                .iter()
                .map(|stale| {
                    Line::from(format!(
                        "  stale since {}: it no longer merges",
                        wf_core::inbox::timestamp(stale.since_ms)
                    ))
                    .themed(theme::text_dimmed())
                })
                .collect();
            row.lines(first, row.keys("v").map(Line::from), more)
        }
        InboxEntry::Notice { item, kind, what } => {
            let mut first = vec![themed("• ", theme::attention())];
            first.extend(item_spans(item.id, &item.title));
            let more = Line::from(format!("  {kind}: {what}")).themed(theme::text_dimmed());
            row.lines(first, None, vec![more])
        }
        InboxEntry::MachineFault { title, what } => {
            let first = vec![themed("! ", theme::attention()), Span::raw(title.clone())];
            let more = Line::from(format!("  {what}")).themed(theme::text_dimmed());
            row.lines(first, None, vec![more])
        }
    }
}

/// How long ago `since` was: `12m`, `3h05m` or `2d`.
pub(super) fn age(since: jiff::Timestamp, now_ms: u64) -> String {
    let since_ms = u64::try_from(since.as_millisecond()).unwrap_or(0);
    duration(now_ms.saturating_sub(since_ms))
}

/// What a session does now, from its transcript.
pub(super) fn activity(app: &App, session: &SessionInfo) -> Option<Line<'static>> {
    if let Place::Elsewhere(device) = &session.place {
        return Some(Line::from(format!("  on {device}")).themed(theme::text_dimmed()));
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
    Some(Line::from(line).themed(theme::text_dimmed()))
}

pub(super) fn session_lines(app: &App, session: &SessionInfo, row: RowLines) -> Vec<Line<'static>> {
    let glyph = match session.status {
        SessionStatus::Live => themed("◆ ", theme::live()),
        SessionStatus::Parked => themed("◇ ", theme::live()),
        SessionStatus::Ended { .. } => Span::raw("· ").themed(theme::text_dimmed()),
    };
    let mut first = vec![
        glyph,
        themed(
            format!("{:<9}", session.agent_name.as_str()),
            theme::agent(),
        ),
        Span::raw(" "),
    ];
    first.extend(item_spans(session.item, &session.title));
    let mut right = age(session.started, app.snapshot.now_ms);
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
pub(super) fn with_context(app: &App, session: &SessionInfo, right: String) -> Line<'static> {
    let mut line = Line::from(Span::raw(right).themed(theme::text_dimmed()));
    if let Some(usage) = app.transcripts.get(&session.id).and_then(Transcript::usage) {
        line.push_span(Span::raw(" · ").themed(theme::text_dimmed()));
        line.push_span(context_span(
            usage,
            format!("{}/{}", tokens(usage.used), tokens(usage.size)),
        ));
    }
    line
}

/// `Opus 5.5 · 69k of 1M (7%) · $0.42`: the model and the usage when the agent reports them, the
/// cost only when it reports that.
pub(super) fn context_line(model: Option<&str>, usage: Option<&Usage>) -> Option<Line<'static>> {
    let mut spans = Vec::new();
    if let Some(model) = model {
        spans.push(Span::raw(model.to_owned()).themed(theme::text_dimmed()));
    }
    if let Some(usage) = usage {
        if !spans.is_empty() {
            spans.push(Span::raw(" · ").themed(theme::text_dimmed()));
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
pub(super) fn context_span(usage: &Usage, text: String) -> Span<'static> {
    if context_percent(usage) >= 80 {
        themed(text, theme::attention())
    } else {
        Span::raw(text).themed(theme::text_dimmed())
    }
}

/// How much of the context window is used, in whole percent.
pub(super) fn context_percent(usage: &Usage) -> u64 {
    (usage.used * 100 + usage.size / 2)
        .checked_div(usage.size)
        .unwrap_or(0)
}

/// A count of tokens, short: `850`, `69k`, `1M`, `1.5M`.
pub(super) fn tokens(count: u64) -> String {
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
pub(super) fn now_lines(app: &App, width: u16) -> ListText {
    let snapshot = &app.snapshot;
    let selected = app.now.selection.get();
    let mut list = ListText::default();
    if !snapshot.waiting.is_empty() {
        list.push(section("WAITING ON YOU"));
        for entry in &snapshot.waiting {
            let is_selected = selected == Some(&NowRow::Waiting(entry.key()));
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
            let is_selected = selected == Some(&NowRow::Session(session.id));
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
            let is_selected = selected == Some(&NowRow::Flag(flag.clone()));
            let first = vec![themed("! ", theme::warning()), Span::raw(flag.to_string())];
            list.entry(
                RowLines::new(app, is_selected, width).lines(first, None, Vec::new()),
                is_selected,
            );
        }
    }
    if list.lines.is_empty() {
        list.push(
            Line::from(" Nothing waits on you, and no agent is working.")
                .themed(theme::text_dimmed()),
        );
    }
    list
}

/// `by state · sorted by merge · only ready · "parse"`: how the items are grouped, ordered and filtered.
pub(super) fn items_heading(app: &App) -> Line<'static> {
    let grouping = match app.items.grouping {
        ItemGrouping::State => "state",
        ItemGrouping::Lane => "lane",
        ItemGrouping::Priority => "priority",
        ItemGrouping::Type => "type",
        ItemGrouping::Label => "label",
        ItemGrouping::Readiness => "readiness",
    };
    let order = match app.items.order {
        ItemOrder::Priority => "sorted by priority",
        ItemOrder::Created => "sorted by creation",
        ItemOrder::Merged => "sorted by merge",
    };
    let mut parts = vec![format!("by {grouping}"), order.to_owned()];
    if let Some(readiness) = app.items.readiness.name() {
        parts.push(format!("only {readiness}"));
    }
    if !app.items.filter.is_empty() {
        parts.push(format!("\"{}\"", app.items.filter));
    }
    Line::from(format!(" {}", parts.join(" · "))).themed(theme::text_dimmed())
}

pub(super) fn item_row_lines(item: &ItemSummary, row: RowLines) -> Vec<Line<'static>> {
    let icon = render::item_icon(item.state, item.readiness);
    let first = vec![
        themed(format!("{icon} "), icon_style(item)),
        themed(format!("#{}", item.id.short()), theme::id()),
        Span::raw(" "),
        Span::raw(item.title.clone()),
    ];
    let mut annotations = Vec::new();
    match item.readiness {
        Some(Readiness::Blocked) => annotations.push("blocked".to_owned()),
        Some(Readiness::Stranded) => annotations.push("stranded".to_owned()),
        Some(Readiness::Ready | Readiness::Claimed) | None => {}
    }
    if let Some(crate::view::StepsView { done, total }) = item.steps {
        annotations.push(format!("{done}/{total}"));
    }
    // The annotations dim, the priority in its own style, as `wf item list` shows it.
    let mut right = Vec::new();
    if !annotations.is_empty() {
        right.push(Span::raw(annotations.join(" ")).themed(theme::text_dimmed()));
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
pub(super) fn icon_style(item: &ItemSummary) -> anstyle::Style {
    match item.readiness {
        Some(Readiness::Stranded) => theme::readiness(Readiness::Stranded),
        Some(Readiness::Ready | Readiness::Claimed | Readiness::Blocked) | None => {
            theme::state(item.state)
        }
    }
}

/// `this week: 5 done · 3h05m start to done · 2 of 9 attempts failed · 0.5 change requests a
/// merge`, leaving out what there is nothing to say about, wrapped between its parts to `width`.
pub(super) fn stats_lines(stats: &ProjectStats, width: u16) -> Vec<Line<'static>> {
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
        .map(|line| Line::from(format!(" {line}")).themed(theme::text_dimmed()))
        .collect()
}

/// `claude-code · opus`
pub(super) fn runs_on(agent: &AgentView) -> String {
    match &agent.model {
        Some(model) => format!("{} · {model}", agent.harness),
        None => agent.harness.clone(),
    }
}

/// `coder  claude-code · opus  2/2 busy`, `1/2 live` while there is room, `0/2 idle`
pub(super) fn agent_lines(agent: &AgentInfo, row: RowLines) -> Vec<Line<'static>> {
    let first = vec![
        Span::raw(agent.id.name.to_string()).themed(theme::text_bold()),
        Span::raw(format!("  {}", runs_on(&agent.view))).themed(theme::text_dimmed()),
    ];
    let live = agent.stats.live;
    let load = match agent.capacity {
        Some(capacity) if live as u32 >= capacity => {
            themed(format!("{live}/{capacity} busy"), theme::live())
        }
        Some(capacity) if live > 0 => themed(format!("{live}/{capacity} live"), theme::live()),
        Some(capacity) => Span::raw(format!("{live}/{capacity} idle")).themed(theme::text_dimmed()),
        None => Span::raw(format!("{live} live · not here")).themed(theme::text_dimmed()),
    };
    row.lines(first, Some(Line::from(load)), Vec::new())
}

/// A session under its agent: `◆ #k3j1 Parse config lazily  work 12m`.
pub(super) fn agent_session_lines(
    app: &App,
    session: &SessionInfo,
    row: RowLines,
) -> Vec<Line<'static>> {
    let glyph = match session.status {
        SessionStatus::Live => themed("  ◆ ", theme::live()),
        SessionStatus::Parked => themed("  ◇ ", theme::live()),
        SessionStatus::Ended { .. } => Span::raw("  · ").themed(theme::text_dimmed()),
    };
    let mut first = vec![glyph];
    first.extend(item_spans(session.item, &session.title));
    let now_ms = app.snapshot.now_ms;
    let right = match session.status {
        SessionStatus::Live => format!("{} {}", session.role.name(), age(session.started, now_ms)),
        SessionStatus::Parked => format!(
            "{} parked {}",
            session.role.name(),
            age(session.started, now_ms)
        ),
        SessionStatus::Ended { reason, at } => {
            format!("{} {} ago", reason.label(), age(at, now_ms))
        }
    };
    row.lines(first, Some(with_context(app, session, right)), Vec::new())
}

/// The agents with their sessions under the stats of the project.
pub(super) fn agents_lines(app: &App, width: u16) -> ListText {
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
        list.push(
            Line::from(" no agents yet; `wf agent add` defines one").themed(theme::text_dimmed()),
        );
    }
    list
}

/// The items in their groups under how they are grouped.
pub(super) fn items_lines(app: &App, width: u16) -> ListText {
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
        list.push(Line::from(" no items match").themed(theme::text_dimmed()));
    }
    list
}

//! Plain text output, rendered from the same views `--json` prints.

use std::fmt::Write;

use comfy_table::presets::NOTHING;
use comfy_table::{Attribute, Cell, ContentArrangement, Table};

use crate::cli::ItemGrouping;
use crate::devices::{DeviceView, MemberView};
use crate::view::{
    ActivityKind, AgentView, BodyPartView, CommentKindView, InboxEntryView, ItemDetail, ItemGroup,
    ItemSummary, LogView, MentionsView, ReviewStatusView, SessionView, SourceView, StatusView,
    StepRefView, StepsView,
};
use wf_core::graph::Readiness;
use wf_core::item::Priority;
use wf_core::policy::StateCategory;

fn short(id: &str) -> &str {
    &id[..id.len().min(wf_core::id::SHORT_ID_LEN)]
}

/// The one place where every table's look is defined.
fn table(header: &[&str]) -> Table {
    let header: Vec<Cell> = header
        .iter()
        .map(|title| Cell::new(title).add_attribute(Attribute::Bold))
        .collect();
    let mut table = Table::new();
    table
        .load_style(NOTHING)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(header);
    table
}

/// Where in an item a reference is.
fn written_in(source: &SourceView) -> &'static str {
    match source.in_comment {
        None => "body",
        Some(_) => "comment",
    }
}

/// Colors are always written as ANSI escapes; `anstream` strips them when output is not a terminal.
mod style {
    use anstyle::{AnsiColor, Style};

    use crate::view::{ActorKind, ActorView};

    pub const BOLD: Style = Style::new().bold();
    pub const DIM: Style = Style::new().dimmed();
    pub const ID: Style = Style::new().fg_color(Some(anstyle::Color::Ansi(AnsiColor::Cyan)));
    pub const PRINCIPAL: Style =
        Style::new().fg_color(Some(anstyle::Color::Ansi(AnsiColor::Magenta)));
    pub const AGENT: Style =
        Style::new().fg_color(Some(anstyle::Color::Ansi(AnsiColor::BrightBlue)));
    pub const DAEMON: Style = Style::new().fg_color(Some(anstyle::Color::Ansi(AnsiColor::Green)));
    pub const WARNING: Style = Style::new()
        .bold()
        .fg_color(Some(anstyle::Color::Ansi(AnsiColor::Red)));

    pub fn color(color: AnsiColor) -> Style {
        Style::new().fg_color(Some(anstyle::Color::Ansi(color)))
    }

    fn actor(kind: ActorKind) -> Style {
        match kind {
            ActorKind::Principal => PRINCIPAL,
            ActorKind::Agent => AGENT,
            ActorKind::Daemon => DAEMON,
        }
    }

    pub fn paint(style: Style, text: impl std::fmt::Display) -> String {
        format!("{style}{text}{style:#}")
    }

    pub fn paint_actor(actor: &ActorView) -> String {
        paint(self::actor(actor.kind), &actor.label)
    }
}

fn state_style(state_category: Option<StateCategory>) -> anstyle::Style {
    use anstyle::AnsiColor;
    match state_category {
        Some(StateCategory::Open) => style::color(AnsiColor::Green),
        Some(StateCategory::Active) => style::color(AnsiColor::Blue),
        Some(StateCategory::Review) => style::color(AnsiColor::Yellow),
        Some(StateCategory::Done) => style::color(AnsiColor::Magenta),
        Some(StateCategory::Cancelled) => style::color(AnsiColor::BrightBlack),
        Some(StateCategory::Inactive) | None => style::DIM,
    }
}

/// The icon of a state category; `?` when the state is unknown under the current policy.
fn state_icon(state_category: Option<StateCategory>) -> &'static str {
    match state_category {
        Some(StateCategory::Inactive) => "◌",
        Some(StateCategory::Open) => "○",
        Some(StateCategory::Active) => "◐",
        Some(StateCategory::Review) => "◎",
        Some(StateCategory::Done) => "✓",
        Some(StateCategory::Cancelled) => "✗",
        None => "?",
    }
}

/// `◐ in_progress`. Pickers take plain text, so the icon is not colored.
pub fn state_choice(name: &str, state_category: StateCategory) -> String {
    format!("{} {name}", state_icon(Some(state_category)))
}

fn priority_style(priority: Priority) -> anstyle::Style {
    match priority {
        Priority::P0 => style::WARNING,
        Priority::P1 => style::color(anstyle::AnsiColor::Yellow),
        Priority::P2 => anstyle::Style::new(),
        Priority::P3 => style::DIM,
    }
}

/// `2026-10-09 21:42` in the local time zone; text that is not a timestamp is kept as it is.
fn short_time(timestamp: &str) -> String {
    local_time(timestamp, "%Y-%m-%d %H:%M")
}

/// A timestamp of a view, in the time zone of this machine and the given format.
fn local_time(timestamp: &str, format: &str) -> String {
    timestamp.parse::<jiff::Timestamp>().map_or_else(
        |_| timestamp.to_string(),
        |time| {
            time.to_zoned(jiff::tz::TimeZone::system())
                .strftime(format)
                .to_string()
        },
    )
}

/// The icon of an item: its state's icon, or a warning icon when the item is stranded.
pub fn item_icon(
    state_category: Option<StateCategory>,
    readiness: Option<Readiness>,
) -> &'static str {
    match readiness {
        Some(Readiness::Stranded) => "⚠",
        Some(Readiness::Ready | Readiness::Claimed | Readiness::Blocked) | None => {
            state_icon(state_category)
        }
    }
}

/// `○ #7ph6ds (task:open)  Title`. Pickers take plain text, so nothing is colored.
pub fn item_choice(summary: &ItemSummary) -> String {
    let state = if summary.archived {
        "archived"
    } else {
        summary.state.as_deref().unwrap_or("unknown")
    };
    format!(
        "{} #{} ({}:{state})  {}",
        item_icon(summary.state_category, summary.readiness),
        short(&summary.id),
        summary.item_type,
        summary.title,
    )
}

/// `○ #q7x2 (task:open) `, the icon and state colored by its state category. A stranded item gets a warning
/// icon instead. `suffix` follows the state.
fn item_label(
    id: &str,
    item_type: &str,
    state: Option<&str>,
    state_category: Option<StateCategory>,
    readiness: Option<Readiness>,
    suffix: &str,
) -> String {
    let state_style = state_style(state_category);
    let icon_style = match readiness {
        Some(Readiness::Stranded) => style::WARNING,
        Some(Readiness::Ready | Readiness::Claimed | Readiness::Blocked) | None => state_style,
    };
    format!(
        "{} {} {}{}{}{}",
        style::paint(icon_style, item_icon(state_category, readiness)),
        id_label(id),
        style::paint(style::DIM, format!("({item_type}:")),
        style::paint(state_style, state.unwrap_or("unknown")),
        style::paint(style::DIM, suffix),
        style::paint(style::DIM, ")"),
    )
}

fn id_label(id: &str) -> String {
    style::paint(style::ID, format!("#{}", short(id)))
}

const ITEM_COLUMNS: [&str; 5] = [
    "ITEM (TYPE:STATE)",
    "PRIORITY",
    "TITLE",
    "STEPS",
    "BLOCKING",
];

fn item_row(item: &ItemSummary) -> [String; 5] {
    let suffix = if item.archived { " · archived" } else { "" };
    let priority = item.priority.map_or_else(
        || style::paint(style::DIM, "-"),
        |priority| style::paint(priority_style(priority), priority),
    );
    let blocking = match item.blocking.split_first() {
        None => String::new(),
        Some((first, [])) => id_label(&first.id),
        Some((first, rest)) => format!(
            "{} {}",
            id_label(&first.id),
            style::paint(style::DIM, format!("+{}", rest.len()))
        ),
    };
    [
        item_label(
            &item.id,
            &item.item_type,
            item.state.as_deref(),
            item.state_category,
            item.readiness,
            suffix,
        ),
        priority,
        item.title.clone(),
        steps_label(item.steps),
        blocking,
    ]
}

/// `2/5`, or nothing for an item without steps.
fn steps_label(steps: Option<StepsView>) -> String {
    steps.map_or_else(String::new, |steps| {
        format!("{}/{}", steps.done, steps.total)
    })
}

pub fn item_list(items: &[ItemSummary]) -> String {
    if items.is_empty() {
        return "no items\n".to_string();
    }
    let mut table = table(&ITEM_COLUMNS);
    for item in items {
        table.add_row(item_row(item));
    }
    format!("{table}\n")
}

/// `◐ in_progress (3)`: the group's icon and name, with the number of items.
fn group_heading(grouping: ItemGrouping, group: &ItemGroup) -> String {
    let Some(first) = group.items.first() else {
        return group.group.clone();
    };
    let (icon, heading_style) = match grouping {
        ItemGrouping::State => (
            state_icon(first.state_category),
            state_style(first.state_category),
        ),
        ItemGrouping::Readiness => match first.readiness {
            Some(Readiness::Ready) => ("○", state_style(Some(StateCategory::Open))),
            Some(Readiness::Claimed) => ("◐", state_style(Some(StateCategory::Active))),
            Some(Readiness::Blocked) => ("⊘", style::color(anstyle::AnsiColor::Red)),
            Some(Readiness::Stranded) => ("⚠", style::WARNING),
            None => ("·", style::DIM),
        },
        ItemGrouping::Label | ItemGrouping::Type | ItemGrouping::Lane | ItemGrouping::Priority => {
            ("·", style::DIM)
        }
    };
    format!(
        "{} {}",
        style::paint(heading_style, format!("{icon} {}", group.group)),
        style::paint(style::DIM, format!("({})", group.items.len()))
    )
}

/// One table, so columns line up across groups; each group starts with a heading row.
pub fn item_groups(grouping: ItemGrouping, groups: &[ItemGroup]) -> String {
    if groups.is_empty() {
        return "no items\n".to_string();
    }
    let mut table = table(&ITEM_COLUMNS);
    for (index, group) in groups.iter().enumerate() {
        if index > 0 {
            table.add_row([""; 5]);
        }
        table.add_row([
            group_heading(grouping, group),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
        ]);
        for item in &group.items {
            table.add_row(item_row(item));
        }
    }
    format!("{table}\n")
}

/// The width prose is wrapped to: the terminal's, but not so wide that lines are hard to read.
/// `None` when output is not a terminal, so lines read by a program are never broken.
fn text_width() -> Option<usize> {
    terminal_size::terminal_size_of(std::io::stdout())
        .map(|(width, _)| usize::from(width.0).clamp(40, 100))
}

/// Markdown source, each line indented by `indent` and wrapped to `width` if given. A wrapped
/// list item or quote continues under its text; fenced code and table rows are never wrapped.
fn wrap_markdown(text: &str, width: Option<usize>, indent: &str) -> String {
    let mut out = String::new();
    let mut in_fence = false;
    for line in text.lines() {
        let plain = anstream::adapter::strip_str(line).to_string();
        let trimmed = plain.trim_start();
        let is_fence = trimmed.starts_with("```") || trimmed.starts_with("~~~");
        if is_fence {
            in_fence = !in_fence;
        }
        match width {
            _ if line.trim().is_empty() => {}
            Some(_) if !(in_fence || is_fence || trimmed.starts_with('|')) => {
                let rest = format!("{indent}{}", hanging_indent(&plain));
                out.push_str(&fill(&format!("{indent}{line}"), width, &rest));
            }
            Some(_) | None => {
                out.push_str(indent);
                out.push_str(line);
            }
        }
        out.push('\n');
    }
    out
}

/// `line` wrapped to `width` at spaces, continuing under `indent`; unchanged without a width.
/// Words longer than the width are left whole rather than broken.
fn fill(line: &str, width: Option<usize>, indent: &str) -> String {
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

/// What a wrapped markdown line continues under: spaces as wide as its leading whitespace, list
/// marker, checkbox and step number, or its `> ` quote markers.
fn hanging_indent(line: &str) -> String {
    let rest = line.trim_start();
    if rest.starts_with('>') {
        let quotes = rest.len() - rest.trim_start_matches(['>', ' ']).len();
        return format!("{}{}", &line[..line.len() - rest.len()], &rest[..quotes]);
    }
    let mut rest = rest;
    for marker in [
        bullet_marker as fn(&str) -> Option<&str>,
        checkbox_marker,
        number_marker,
    ] {
        if let Some(after) = marker(rest) {
            rest = after;
        }
    }
    " ".repeat(textwrap::core::display_width(
        &line[..line.len() - rest.len()],
    ))
}

fn bullet_marker(text: &str) -> Option<&str> {
    ["- ", "* ", "+ "]
        .iter()
        .find_map(|marker| text.strip_prefix(marker))
}

fn checkbox_marker(text: &str) -> Option<&str> {
    ["[ ] ", "[x] ", "[X] "]
        .iter()
        .find_map(|marker| text.strip_prefix(marker))
}

fn number_marker(text: &str) -> Option<&str> {
    let digits = text.len() - text.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 {
        return None;
    }
    let after = &text[digits..];
    after
        .strip_prefix(". ")
        .or_else(|| after.strip_prefix(") "))
}

pub fn item_detail(detail: &ItemDetail) -> String {
    item_detail_for(detail, text_width())
}

/// [`item_detail`] with prose wrapped to `width`, or not at all.
pub fn item_detail_for(detail: &ItemDetail, width: Option<usize>) -> String {
    use style::{BOLD, DIM, WARNING, paint};

    let summary = &detail.summary;
    let mut out = String::new();
    // A header line continues under its text: past the label for the title, past `↳ ` for a link.
    let mut line = |text: String, indent: &str| {
        let _ = writeln!(out, "{}", fill(&text, width, indent));
    };
    let archived = if summary.archived { "[archived]" } else { "" };
    let label = item_label(
        &summary.id,
        &summary.item_type,
        summary.state.as_deref(),
        summary.state_category,
        summary.readiness,
        archived,
    );
    let title_indent = " ".repeat(textwrap::core::display_width(&label) + 1);
    line(
        format!("{label} {}", paint(BOLD, &summary.title)),
        &title_indent,
    );

    let mut status = Vec::new();
    if let Some(priority) = summary.priority {
        status.push(paint(priority_style(priority), priority));
    }
    if let Some(lane) = &detail.summary.lane {
        status.push(paint(DIM, format!("lane={lane}")));
    }
    if let Some(review) = &detail.review {
        status.push(paint(DIM, format!("review={review}")));
    }
    if let Some(recovery) = &detail.recovery {
        status.push(paint(DIM, format!("recovery={recovery}")));
    }
    if !detail.summary.labels.is_empty() {
        status.push(paint(
            DIM,
            format!("labels={}", detail.summary.labels.join(",")),
        ));
    }
    if !detail.requires.is_empty() {
        status.push(paint(
            DIM,
            format!("requires={}", detail.requires.join(",")),
        ));
    }
    if let Some(steps) = summary.steps {
        status.push(format!("steps {}", steps_label(Some(steps))));
    }
    if let Some(waits) = &detail.waits_after_failures {
        status.push(paint(style::WARNING, format!("waits for you: {waits}")));
    }
    if let Some(review) = &detail.review_status {
        let short_commit = |commit: &str| commit[..commit.len().min(8)].to_string();
        let text = match review {
            ReviewStatusView::Awaiting { step } => format!("review: awaiting {step}"),
            ReviewStatusView::ChangesRequested => "review: changes requested".to_string(),
            ReviewStatusView::Approved {
                commit: Some(commit),
            } => format!("review: approved {}", short_commit(commit)),
            ReviewStatusView::Approved { commit: None } => "review: approved".to_string(),
            ReviewStatusView::Merged { commit } => {
                format!("review: merged {}", short_commit(commit))
            }
        };
        status.push(text);
    }
    if !summary.blocked_by.is_empty() {
        let ids: Vec<String> = summary
            .blocked_by
            .iter()
            .map(|blocker| id_label(&blocker.id))
            .collect();
        status.push(format!("blocked by {}", ids.join(", ")));
    }
    if !status.is_empty() {
        line(format!("  {}", status.join("  ")), "  ");
    }

    line(
        format!(
            "  {}",
            [
                style::paint_actor(&detail.author),
                paint(DIM, format!("created {}", short_time(&detail.created))),
                paint(DIM, format!("updated {}", short_time(&detail.updated))),
            ]
            .join(&format!(" {} ", paint(DIM, "·")))
        ),
        "  ",
    );
    for flag in &detail.flags {
        line(format!("  {}", paint(WARNING, format!("! {flag}"))), "    ");
    }
    if !detail.open.is_empty() {
        let open: Vec<String> = detail
            .open
            .iter()
            .map(|comment| {
                format!(
                    "{} {} {} by {}",
                    comment_marker(comment.kind),
                    comment.id,
                    comment_kind_label(comment.kind),
                    style::paint_actor(&comment.by)
                )
            })
            .collect();
        line(
            format!(
                "  {} {}",
                paint(WARNING, format!("{} open:", detail.open.len())),
                open.join(&format!(" {} ", paint(DIM, "·")))
            ),
            "  ",
        );
    }

    for (label, links) in [
        ("depends on", detail.depends_on.as_slice()),
        ("required by", &detail.required_by),
        ("supersedes", detail.supersedes.as_slice()),
    ] {
        for link in links {
            line(
                format!(
                    "  ↳ {label} {} \"{}\"{}",
                    item_label(
                        &link.id,
                        &link.item_type,
                        link.state.as_deref(),
                        link.state_category,
                        None,
                        ""
                    ),
                    link.title,
                    if link.ignored {
                        " [ignored: cycle]"
                    } else {
                        ""
                    }
                ),
                LINK_INDENT,
            );
        }
    }
    for reference in &detail.references {
        let link = &reference.item;
        let what = match &reference.comment {
            Some(comment) => format!("{} on ", paint(DIM, comment)),
            None => String::new(),
        };
        line(
            format!(
                "  ↳ references {what}{} \"{}\"",
                item_label(
                    &link.id,
                    &link.item_type,
                    link.state.as_deref(),
                    link.state_category,
                    None,
                    ""
                ),
                link.title,
            ),
            LINK_INDENT,
        );
    }
    for backlink in &detail.referenced_by {
        let source = &backlink.from;
        let what = match &backlink.comment {
            Some(comment) => format!("{} ", paint(DIM, comment)),
            None => String::new(),
        };
        line(
            format!(
                "  ↳ {what}referenced by {} \"{}\" ({})",
                id_label(&source.item),
                source.title,
                written_in(source)
            ),
            LINK_INDENT,
        );
    }
    if !detail.mentions.is_empty() {
        let mentions: Vec<String> = detail.mentions.iter().map(style::paint_actor).collect();
        line(format!("  ↳ mentions {}", mentions.join(" ")), LINK_INDENT);
    }
    if !detail.body.is_empty() {
        out.push('\n');
        out.push_str(&wrap_markdown(&body(&detail.body), width, "  "));
    }
    if !detail.activity.is_empty() {
        out.push_str(&activity(&detail.activity, width));
    }
    out
}

/// Where a wrapped `↳` line of an item's header continues: under the text after `↳ `.
const LINK_INDENT: &str = "    ";

/// The activity of an item: one line per thing that happened, and the text that goes with it
/// below. The date heads the section and is repeated when the day changes.
fn activity(entries: &[crate::view::ActivityView], width: Option<usize>) -> String {
    use style::{BOLD, DIM, paint};
    let mut out = String::new();
    let mut day = None;
    for entry in entries {
        let (date, time) = local_date_and_time(&entry.at);
        if day.as_ref() != Some(&date) {
            match day {
                None => {
                    let _ = writeln!(
                        out,
                        "\n{} {}",
                        paint(BOLD, "activity"),
                        paint(DIM, format!("· {date}"))
                    );
                }
                Some(_) => {
                    let _ = writeln!(out, "  {}", paint(DIM, format!("· {date}")));
                }
            }
            day = Some(date);
        }
        let (line, text) = activity_entry(entry, &format!("  {}  ", paint(DIM, time)), width);
        let _ = writeln!(out, "{line}");
        if let Some(text) = text {
            let _ = writeln!(out, "{text}");
        }
    }
    out
}

/// Where the text of an activity entry starts, and where a long line of who did what continues:
/// two columns further in than what happened, to set it apart.
const ACTIVITY_TEXT_INDENT: &str = "           ";

/// One entry of an item's activity: `lead` (its time), who did what, and the text that goes with
/// it, if it has any. Both are wrapped to `width` under [`ACTIVITY_TEXT_INDENT`].
fn activity_entry(
    entry: &crate::view::ActivityView,
    lead: &str,
    width: Option<usize>,
) -> (String, Option<String>) {
    use style::{DIM, WARNING, paint};
    let mut parts = vec![style::paint_actor(&entry.by)];
    match &entry.comment {
        Some(comment) if entry.kind == ActivityKind::Comment => {
            match comment.kind {
                CommentKindView::Blocker | CommentKindView::ChangeRequest => parts.push(paint(
                    WARNING,
                    format!(
                        "{} {}",
                        comment_marker(comment.kind),
                        comment_kind_label(comment.kind)
                    ),
                )),
                CommentKindView::Comment if comment.resolves.is_empty() => {
                    parts.push(paint(DIM, "commented"))
                }
                CommentKindView::Comment => {}
            }
            if !comment.resolves.is_empty() {
                parts.push(format!("resolved {}", comment.resolves.join(", ")));
            }
            if let Some(by) = &comment.resolved_by {
                parts.push(paint(DIM, format!("resolved by {by}")));
            }
            match &comment.step {
                Some(StepRefView {
                    number: Some(number),
                    ..
                }) => parts.push(paint(DIM, format!("on step {number}"))),
                Some(StepRefView { .. }) => parts.push(paint(DIM, "on a step since edited")),
                None => {}
            }
            parts.push(paint(DIM, &comment.short));
        }
        Some(_) | None => parts.push(entry.summary.clone()),
    }
    let line = format!("{lead}{}", parts.join(&format!(" {} ", paint(DIM, "·"))));
    let line = fill(&line, width, ACTIVITY_TEXT_INDENT);
    let text = entry
        .text
        .as_deref()
        .filter(|text| !text.trim().is_empty())
        .map(|text| {
            let text = wrap_markdown(text, width, ACTIVITY_TEXT_INDENT);
            text.trim_end_matches('\n').to_string()
        });
    (line, text)
}

/// One line of `wf watch`: `14:02  #k3f7ab  @ada/coder · moved to done`, and the text that goes
/// with it below.
pub fn watch_line(line: &crate::view::WatchLineView) -> String {
    watch_line_for(line, text_width())
}

/// [`watch_line`] wrapped to `width`, or not at all.
pub fn watch_line_for(line: &crate::view::WatchLineView, width: Option<usize>) -> String {
    use style::{DIM, paint};
    let (_, time) = local_date_and_time(&line.activity.at);
    let item = match &line.item {
        Some(item) => format!("{}  ", id_label(item)),
        None => String::new(),
    };
    let lead = format!("{}  {item}", paint(DIM, time));
    let (entry, text) = activity_entry(&line.activity, &lead, width);
    let mut out = format!("{entry}\n");
    if let Some(text) = text {
        out.push_str(&text);
        out.push('\n');
    }
    out
}

/// The date and the time of a timestamp in the local time zone.
fn local_date_and_time(timestamp: &str) -> (String, String) {
    match timestamp.parse::<jiff::Timestamp>() {
        Ok(time) => {
            let local = time.to_zoned(jiff::tz::TimeZone::system());
            (
                local.strftime("%Y-%m-%d").to_string(),
                local.strftime("%H:%M").to_string(),
            )
        }
        Err(_) => (timestamp.to_string(), String::new()),
    }
}

/// Marks a blocker or change request: `⧗` waits on someone else, `△` waits on the work.
fn comment_marker(kind: CommentKindView) -> &'static str {
    match kind {
        CommentKindView::Blocker => "⧗",
        CommentKindView::ChangeRequest => "△",
        CommentKindView::Comment => "",
    }
}

fn comment_kind_label(kind: CommentKindView) -> &'static str {
    match kind {
        CommentKindView::Comment => "comment",
        CommentKindView::Blocker => "blocker",
        CommentKindView::ChangeRequest => "change request",
    }
}

/// The body as markdown with each step's number before its text: `- [ ] 1. write parser`.
fn body(parts: &[BodyPartView]) -> String {
    let mut out = String::new();
    let mut previous_was_step = false;
    for (index, part) in parts.iter().enumerate() {
        let is_step = matches!(part, BodyPartView::Step { .. });
        if index > 0 {
            out.push_str(if previous_was_step && is_step {
                "\n"
            } else {
                "\n\n"
            });
        }
        match part {
            BodyPartView::Markdown { text } => out.push_str(text),
            BodyPartView::Step { number, done, text } => {
                let checkbox = if *done { "[x]" } else { "[ ]" };
                let number = style::paint(style::DIM, format!("{number}."));
                let _ = write!(out, "- {checkbox} {number} {text}");
            }
        }
        previous_was_step = is_step;
    }
    out
}

pub fn mentions(view: &MentionsView) -> String {
    if view.sources.is_empty() {
        return format!("{} is not mentioned anywhere\n", view.target);
    }
    let mut table = table(&["ITEM", "TITLE", "IN"]);
    for source in &view.sources {
        table.add_row([
            format!("#{}", short(&source.item)),
            source.title.clone(),
            written_in(source).to_string(),
        ]);
    }
    format!("{table}\n")
}

pub fn agents(agents: &[AgentView]) -> String {
    if agents.is_empty() {
        return "no agents defined\n".to_string();
    }
    let mut table = table(&[
        "AGENT",
        "HARNESS",
        "MODEL",
        "CAPABILITIES",
        "LANES",
        "START",
        "PERMISSIONS",
    ]);
    for agent in agents {
        table.add_row([
            agent.id.clone(),
            agent.harness.clone(),
            agent.model.clone().unwrap_or_else(|| "default".to_string()),
            agent.capabilities.join(", "),
            agent.lanes.join(", "),
            agent.start.clone(),
            agent.permissions.clone(),
        ]);
    }
    format!("{table}\n")
}

pub fn devices(devices: &[DeviceView]) -> String {
    let mut table = table(&[
        "DEVICE",
        "NAME",
        "PRINCIPAL",
        "HARDWARE",
        "SESSIONS",
        "LAST_SEEN",
        "FLAGS",
    ]);
    for device in devices {
        let name = device.name.clone().unwrap_or_else(|| "-".to_string());
        table.add_row([
            device.device.chars().take(8).collect::<String>(),
            if device.this_device {
                format!("{name} (this)")
            } else {
                name
            },
            device.principal.clone(),
            device.hardware.clone().unwrap_or_else(|| "-".to_string()),
            device
                .max_sessions
                .map_or_else(|| "-".to_string(), |sessions| sessions.to_string()),
            if device.this_device {
                "now".to_string()
            } else {
                device
                    .last_seen
                    .as_deref()
                    .map_or_else(|| "-".to_string(), short_time)
            },
            device.flags.join(", "),
        ]);
    }
    format!("{table}\n")
}

pub fn members(members: &[MemberView]) -> String {
    let mut table = table(&["MEMBER", "ROLE", "DEVICES"]);
    for member in members {
        table.add_row([
            member.handle.clone(),
            member.role.clone(),
            member.devices.join(", "),
        ]);
    }
    format!("{table}\n")
}

pub fn log(view: &LogView) -> String {
    let mut table = table(&["EVENT", "DEVICE", "SEQ", "AT", "KIND", "ACTOR", "SUMMARY"]);
    for event in &view.events {
        table.add_row([
            short(&event.id).to_string(),
            event.device.to_string(),
            event.seq.to_string(),
            local_time(&event.at, "%Y-%m-%d %H:%M:%S"),
            event.kind.to_string(),
            event.actor.to_string(),
            event.summary.to_string(),
        ]);
    }
    let verified = view.events.iter().filter(|event| event.verified).count();
    format!(
        "{table}\n{verified}/{} events verified\n",
        view.events.len()
    )
}

pub fn status(view: &StatusView) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "workforce \"{}\" {}", view.name, view.id);
    if let Some(parent) = &view.forked_from {
        let _ = writeln!(out, "forked from {parent}");
    }
    let _ = writeln!(out, "members: {}", view.members.join(", "));
    let _ = writeln!(
        out,
        "events: {}  heads: {}  pending: {}  agents: {}",
        view.events, view.heads, view.pending, view.agents
    );
    let devices: Vec<String> = view
        .devices
        .iter()
        .map(|device| match (&device.last_seen, device.this_device) {
            (_, true) => format!("{} (this)", device.name),
            (Some(seen), false) => format!("{} (seen {})", device.name, short_time(seen)),
            (None, false) => device.name.clone(),
        })
        .collect();
    let _ = writeln!(out, "devices: {}", devices.join(", "));
    let daemon = if view.daemon { "running" } else { "stopped" };
    let _ = writeln!(out, "daemon: {daemon}");
    let _ = writeln!(
        out,
        "sessions: {} live, {} parked",
        view.live_sessions, view.parked_sessions,
    );
    let _ = writeln!(
        out,
        "items: {} ready, {} waiting, {} archived",
        view.ready_items, view.waiting_on_you, view.archived_items,
    );
    for line in &view.after_failures {
        let _ = writeln!(out, "waits for you: {line}");
    }
    for flag in &view.flagged {
        let _ = writeln!(out, "! {flag}");
    }
    if let Some(next) = &view.next {
        let _ = writeln!(out, "next: {next}");
    }
    if view.refolded {
        let _ = writeln!(out, "refolded from the log and wrote a fresh snapshot");
    }
    out
}

pub fn sessions(sessions: &[SessionView]) -> String {
    if sessions.is_empty() {
        return "no sessions\n".to_string();
    }
    let mut table = table(&[
        "SESSION", "AGENT", "MODEL", "ITEM", "TITLE", "STEPS", "ROLE", "STATUS", "STARTED",
    ]);
    for session in sessions {
        table.add_row([
            short(&session.id).to_string(),
            session.agent.clone(),
            session.model.clone().unwrap_or_default(),
            format!("#{}", short(&session.item)),
            session.title.clone(),
            steps_label(session.steps),
            session.role.clone(),
            session.status.clone(),
            short_time(&session.started),
        ]);
    }
    format!("{table}\n")
}

/// `a1b2c3 coder on #7ph6ds (live)  Title`. Pickers take plain text, so nothing is colored.
pub fn session_choice(session: &SessionView) -> String {
    format!(
        "{} {} on #{} ({})  {}",
        short(&session.id),
        session.agent,
        short(&session.item),
        session.status,
        session.title
    )
}

pub fn session(session: &SessionView) -> String {
    let mut out = format!(
        "session {} of {} on #{} {}\n",
        short(&session.id),
        session.agent,
        short(&session.item),
        session.title
    );
    let _ = writeln!(out, "  role     {}", session.role);
    if let Some(model) = &session.model {
        let _ = writeln!(out, "  model    {model}");
    }
    let _ = writeln!(out, "  status   {}", session.status);
    let _ = writeln!(out, "  started  {}", short_time(&session.started));
    if let Some(ended) = &session.ended {
        let _ = writeln!(out, "  ended    {}", short_time(ended));
    }
    if let Some(commit) = &session.commit {
        let _ = writeln!(out, "  commit   {commit}");
    }
    out
}

pub fn inbox(entries: &[InboxEntryView]) -> String {
    if entries.is_empty() {
        return "nothing waits on you\n".to_string();
    }
    let mut table = table(&["WAITING", "ITEM", "TITLE", "WHAT"]);
    for entry in entries {
        table.add_row([
            entry.kind.to_string(),
            entry
                .item
                .as_deref()
                .map_or_else(String::new, |item| format!("#{}", short(item))),
            entry.title.clone(),
            std::iter::once(entry.what.clone())
                .chain(entry.passed.iter().map(|why| format!("  passed by {why}")))
                .collect::<Vec<_>>()
                .join("\n"),
        ]);
    }
    format!("{table}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapped_lines_keep_the_indent() {
        let text = "one two three four five\nsix seven eight nine ten";
        assert_eq!(
            wrap_markdown(text, Some(16), "  "),
            "  one two three\n  four five\n  six seven\n  eight nine ten\n"
        );
    }

    #[test]
    fn list_items_continue_under_their_text() {
        assert_eq!(
            wrap_markdown("- one two three four", Some(12), "  "),
            "  - one two\n    three\n    four\n"
        );
        assert_eq!(
            wrap_markdown("  1. one two three", Some(14), "  "),
            "    1. one two\n       three\n"
        );
        assert_eq!(
            wrap_markdown("> one two three", Some(12), "  "),
            "  > one two\n  > three\n"
        );
    }

    #[test]
    fn steps_continue_under_their_text_past_the_painted_number() {
        let step = format!("- [ ] {} one two three", style::paint(style::DIM, "1."));
        let wrapped = wrap_markdown(&step, Some(20), "  ");
        assert_eq!(
            anstream::adapter::strip_str(&wrapped).to_string(),
            "  - [ ] 1. one two\n           three\n"
        );
    }

    #[test]
    fn long_words_and_code_are_not_broken() {
        assert_eq!(
            wrap_markdown("see https://example.com/a-long/path", Some(12), "  "),
            "  see\n  https://example.com/a-long/path\n"
        );
        let code = "```\nlet value = one + two + three;\n```";
        assert_eq!(
            wrap_markdown(code, Some(12), "  "),
            "  ```\n  let value = one + two + three;\n  ```\n"
        );
    }

    fn checked_step(text: &str) -> crate::view::ActivityView {
        crate::view::ActivityView {
            at: "not a timestamp".to_string(),
            by: crate::view::ActorView {
                kind: crate::view::ActorKind::Agent,
                label: "@ada/coder/bh2m".to_string(),
            },
            kind: ActivityKind::Edited,
            summary: "checked step 1".to_string(),
            comment: None,
            text: Some(text.to_string()),
        }
    }

    #[test]
    fn the_text_of_an_activity_entry_is_wrapped_below_its_line() {
        let entry = checked_step("wrap long activity lines under their indent");
        let (line, text) = activity_entry(&entry, "  00:27  ", Some(44));
        assert_eq!(
            anstream::adapter::strip_str(&line).to_string(),
            "  00:27  @ada/coder/bh2m · checked step 1"
        );
        assert_eq!(
            anstream::adapter::strip_str(&text.unwrap()).to_string(),
            "           wrap long activity lines under\n           their indent"
        );
        let (line, text) = activity_entry(&entry, "  00:27  ", None);
        assert_eq!(
            anstream::adapter::strip_str(&line).to_string(),
            "  00:27  @ada/coder/bh2m · checked step 1"
        );
        assert_eq!(
            anstream::adapter::strip_str(&text.unwrap()).to_string(),
            "           wrap long activity lines under their indent"
        );
    }

    #[test]
    fn a_long_activity_line_continues_under_the_text() {
        let mut entry = checked_step("");
        entry.summary = "renamed it to \"a title too long for one line\"".to_string();
        let (line, _) = activity_entry(&entry, "  00:27  ", Some(40));
        assert_eq!(
            anstream::adapter::strip_str(&line).to_string(),
            "  00:27  @ada/coder/bh2m · renamed it\n           to \"a title too long for\n           one line\""
        );
    }

    fn detail_with_a_reference(title: &str, referenced: &str) -> ItemDetail {
        let author = crate::view::ActorView {
            kind: crate::view::ActorKind::Agent,
            label: "@ada/coder/bh2m".to_string(),
        };
        ItemDetail {
            summary: crate::view::ItemSummary {
                id: "dm3q4jpcwtdq".to_string(),
                item_type: "task".to_string(),
                title: title.to_string(),
                state: None,
                state_category: None,
                priority: None,
                readiness: None,
                blocked_by: Vec::new(),
                blocking: Vec::new(),
                labels: Vec::new(),
                lane: None,
                steps: None,
                archived: false,
            },
            body: Vec::new(),
            author,
            created: "not a timestamp".to_string(),
            updated: "not a timestamp".to_string(),
            review: None,
            review_status: None,
            recovery: None,
            waits_after_failures: None,
            requires: Vec::new(),
            depends_on: Vec::new(),
            required_by: Vec::new(),
            supersedes: None,
            references: vec![crate::view::ReferenceView {
                item: crate::view::LinkView {
                    id: "kvb7vjaaaaaa".to_string(),
                    item_type: "task".to_string(),
                    title: referenced.to_string(),
                    state: None,
                    state_category: None,
                    ignored: false,
                },
                comment: None,
            }],
            referenced_by: Vec::new(),
            mentions: Vec::new(),
            activity: Vec::new(),
            open: Vec::new(),
            flags: Vec::new(),
        }
    }

    #[test]
    fn header_lines_continue_under_their_text() {
        let detail = detail_with_a_reference(
            "Wrap the header of wf item show under its indent",
            "Wrap long activity lines under their indent",
        );
        let shown = anstream::adapter::strip_str(&item_detail_for(&detail, Some(40))).to_string();
        assert_eq!(
            shown,
            "? #dm3q4j (task:unknown) Wrap the header\n\
             \x20                        of wf item\n\
             \x20                        show under its\n\
             \x20                        indent\n\
             \x20 @ada/coder/bh2m · created not a\n\
             \x20 timestamp · updated not a timestamp\n\
             \x20 ↳ references ? #kvb7vj (task:unknown)\n\
             \x20   \"Wrap long activity lines under\n\
             \x20   their indent\"\n"
        );
        let shown = anstream::adapter::strip_str(&item_detail_for(&detail, None)).to_string();
        assert_eq!(
            shown,
            "? #dm3q4j (task:unknown) Wrap the header of wf item show under its indent\n\
             \x20 @ada/coder/bh2m · created not a timestamp · updated not a timestamp\n\
             \x20 ↳ references ? #kvb7vj (task:unknown) \"Wrap long activity lines under their indent\"\n"
        );
    }

    #[test]
    fn blank_lines_stay_empty_and_nothing_wraps_without_a_width() {
        let text = "one two three\n\nfour";
        assert_eq!(
            wrap_markdown(text, None, "  "),
            "  one two three\n\n  four\n"
        );
    }
}

//! Plain text output, rendered from the same views `--json` prints.

use std::fmt::Write;
use std::io::IsTerminal;

use comfy_table::presets::NOTHING;
use comfy_table::{Attribute, Cell, ContentArrangement, Table};

use crate::cli::ItemGrouping;
use crate::devices::{DeviceView, MemberView};
use crate::markdown;
use crate::theme;
use crate::transcript::Format;
use crate::view::{
    ActivityKind, ActorKind, ActorView, AgentDetail, AgentView, BodyPartView, CommentKindView,
    EventView, Flag, InboxEntryView, ItemDetail, ItemGroup, ItemStatsView, ItemSummary,
    MentionsView, ReviewStatusView, SessionView, SourceView, StatusView, StepRefView, StepsView,
};
use wf_core::graph::Readiness;
use wf_core::inbox::InboxKind;
use wf_core::item::ItemState;
use wf_core::stats::ItemStats;

use wf_core::id::short;

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
fn paint(style: anstyle::Style, text: impl std::fmt::Display) -> String {
    format!("{style}{text}{style:#}")
}

fn paint_actor(actor: &ActorView) -> String {
    paint(theme::actor(actor.kind), &actor.label)
}

pub(crate) fn state_icon(state: ItemState) -> &'static str {
    match state {
        ItemState::Draft => "◌",
        ItemState::Open => "○",
        ItemState::Active => "◐",
        ItemState::Review => "◎",
        ItemState::Done => "✓",
        ItemState::Cancelled => "✗",
    }
}

fn readiness_icon(readiness: Readiness) -> &'static str {
    match readiness {
        Readiness::Ready => "○",
        Readiness::Claimed => "◐",
        Readiness::Blocked => "⊘",
        Readiness::Stranded => "⚠",
    }
}

/// `2026-10-09 21:42` in the local time zone; text that is not a timestamp is kept as it is.
/// Device ids are shown with 8 characters, as before ids got their own short length.
const DEVICE_ID_LEN: usize = 8;

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
pub fn item_icon(state: ItemState, readiness: Option<Readiness>) -> &'static str {
    match readiness {
        Some(Readiness::Stranded) => "⚠",
        Some(Readiness::Ready | Readiness::Claimed | Readiness::Blocked) | None => {
            state_icon(state)
        }
    }
}

/// `○ #7ph6ds (task:open)  Title`. Pickers take plain text, so nothing is colored.
pub fn item_choice(summary: &ItemSummary) -> String {
    let state = if summary.archived {
        "archived"
    } else {
        summary.state.name()
    };
    format!(
        "{} #{} ({}:{state})  {}",
        item_icon(summary.state, summary.readiness),
        short(&summary.id),
        summary.item_type,
        summary.title,
    )
}

/// `○ #q7x2 (task:open) `, the icon and state colored by the state. A stranded item gets a warning
/// icon instead. `suffix` follows the state.
fn item_label(
    id: &str,
    item_type: &str,
    state: ItemState,
    readiness: Option<Readiness>,
    suffix: &str,
) -> String {
    let state_style = theme::state(state);
    let icon_style = match readiness {
        Some(Readiness::Stranded) => theme::warning(),
        Some(Readiness::Ready | Readiness::Claimed | Readiness::Blocked) | None => state_style,
    };
    format!(
        "{} {} {}{}{}{}",
        paint(icon_style, item_icon(state, readiness)),
        id_label(id),
        paint(theme::text_dimmed(), format!("({item_type}:")),
        paint(state_style, state.name()),
        paint(theme::text_dimmed(), suffix),
        paint(theme::text_dimmed(), ")"),
    )
}

fn id_label(id: &str) -> String {
    paint(theme::id(), format!("#{}", short(id)))
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
        || paint(theme::text_dimmed(), "-"),
        |priority| paint(theme::priority(priority), priority),
    );
    let blocking = match item.blocking.split_first() {
        None => String::new(),
        Some((first, [])) => id_label(&first.id),
        Some((first, rest)) => format!(
            "{} {}",
            id_label(&first.id),
            paint(theme::text_dimmed(), format!("+{}", rest.len()))
        ),
    };
    [
        item_label(
            &item.id,
            &item.item_type,
            item.state,
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

/// `◐ active (3)`: the group's icon and name, with the number of items.
fn group_heading(grouping: ItemGrouping, group: &ItemGroup) -> String {
    let Some(first) = group.items.first() else {
        return group.group.clone();
    };
    let (icon, heading_style) = match grouping {
        ItemGrouping::State => (state_icon(first.state), theme::state(first.state)),
        ItemGrouping::Readiness => match first.readiness {
            Some(readiness) => (readiness_icon(readiness), theme::readiness(readiness)),
            None => ("·", theme::text_dimmed()),
        },
        ItemGrouping::Label | ItemGrouping::Type | ItemGrouping::Lane | ItemGrouping::Priority => {
            ("·", theme::text_dimmed())
        }
    };
    format!(
        "{} {}",
        paint(heading_style, format!("{icon} {}", group.group)),
        paint(theme::text_dimmed(), format!("({})", group.items.len()))
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

/// Rendered markdown at a terminal; piped, the markdown as written, for the agent or script
/// reading it.
fn text_format() -> Format {
    if std::io::stdout().is_terminal() {
        Format::Styled
    } else {
        Format::Raw
    }
}

/// Markdown `text` with each line indented by `indent`: rendered and wrapped to `width` if
/// styled, as written if raw. No newline at the end.
fn markdown_text(text: &str, width: Option<usize>, indent: &str, format: Format) -> String {
    match format {
        Format::Styled => markdown::render(text, width, indent)
            .trim_end_matches('\n')
            .to_string(),
        Format::Raw => indented(text, indent),
    }
}

/// `text` as written, each line that is not blank indented by `indent`.
fn indented(text: &str, indent: &str) -> String {
    text.trim_end()
        .lines()
        .map(|line| {
            if line.trim().is_empty() {
                String::new()
            } else {
                format!("{indent}{line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn item_detail(detail: &ItemDetail) -> String {
    item_detail_for(detail, text_width(), text_format())
}

/// [`item_detail`] with prose wrapped to `width`, or not at all, and its markdown in `format`.
pub fn item_detail_for(detail: &ItemDetail, width: Option<usize>, format: Format) -> String {
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
        summary.state,
        summary.readiness,
        archived,
    );
    let title_indent = " ".repeat(textwrap::core::display_width(&label) + 1);
    line(
        format!("{label} {}", paint(theme::text_bold(), &summary.title)),
        &title_indent,
    );

    let mut status = Vec::new();
    if let Some(priority) = summary.priority {
        status.push(paint(theme::priority(priority), priority));
    }
    if let Some(lane) = &detail.summary.lane {
        status.push(paint(theme::text_dimmed(), format!("lane={lane}")));
    }
    if let Some(review) = &detail.review {
        status.push(paint(theme::text_dimmed(), format!("review={review}")));
    }
    if let Some(recovery) = &detail.recovery {
        status.push(paint(theme::text_dimmed(), format!("recovery={recovery}")));
    }
    if !detail.summary.labels.is_empty() {
        status.push(paint(
            theme::text_dimmed(),
            format!("labels={}", detail.summary.labels.join(",")),
        ));
    }
    if !detail.requires.is_empty() {
        status.push(paint(
            theme::text_dimmed(),
            format!("requires={}", detail.requires.join(",")),
        ));
    }
    if let Some(steps) = summary.steps {
        status.push(format!("steps {}", steps_label(Some(steps))));
    }
    if let Some(review) = &detail.review_status {
        let short_commit = wf_core::session::short_commit;
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
                paint_actor(&detail.author),
                paint(
                    theme::text_dimmed(),
                    format!("created {}", short_time(&detail.created))
                ),
                paint(
                    theme::text_dimmed(),
                    format!("updated {}", short_time(&detail.updated))
                ),
            ]
            .join(&format!(" {} ", paint(theme::text_dimmed(), "·")))
        ),
        "  ",
    );
    if detail.stats.sessions > 0 {
        line(
            format!(
                "  {}",
                paint(theme::text_dimmed(), item_stats(&detail.stats))
            ),
            "  ",
        );
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
                    paint_actor(&comment.by)
                )
            })
            .collect();
        line(
            format!(
                "  {} {}",
                paint(theme::warning(), format!("{} open:", detail.open.len())),
                open.join(&format!(" {} ", paint(theme::text_dimmed(), "·")))
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
                    item_label(&link.id, &link.item_type, link.state, None, ""),
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
            Some(comment) => format!("{} on ", paint(theme::text_dimmed(), comment)),
            None => String::new(),
        };
        line(
            format!(
                "  ↳ references {what}{} \"{}\"",
                item_label(&link.id, &link.item_type, link.state, None, ""),
                link.title,
            ),
            LINK_INDENT,
        );
    }
    for backlink in &detail.referenced_by {
        let what = match &backlink.comment {
            Some(comment) => format!("{} ", paint(theme::text_dimmed(), comment)),
            None => String::new(),
        };
        line(
            format!(
                "  ↳ {what}referenced by {} \"{}\"",
                id_label(&backlink.item),
                backlink.title
            ),
            LINK_INDENT,
        );
    }
    if !detail.mentions.is_empty() {
        let mentions: Vec<String> = detail.mentions.iter().map(paint_actor).collect();
        line(format!("  ↳ mentions {}", mentions.join(" ")), LINK_INDENT);
    }
    out.push_str(&notices(detail, width, format));
    if !detail.body.is_empty() {
        out.push('\n');
        out.push_str(&body(&detail.body, width, format));
    }
    if !detail.activity.is_empty() {
        out.push_str(&activity(&detail.activity, width, format));
    }
    out
}

/// Where a wrapped `↳` line of an item's header continues: under the text after `↳ `.
const LINK_INDENT: &str = "    ";

/// What about an item waits on the principal and what needs attention, in full. Styled, a block
/// under a bar: yellow for what waits, red for failed sessions and the item's flags, each entry
/// under a heading. Raw, one `!` line per entry with what it points to indented below.
fn notices(detail: &ItemDetail, width: Option<usize>, format: Format) -> String {
    let mut out = String::new();
    match format {
        Format::Styled => {
            let mut entries: Vec<Notice> = detail
                .waiting
                .iter()
                .map(|waiting| {
                    let style = match waiting.kind {
                        InboxKind::Recovery | InboxKind::MachineFault => theme::error(),
                        InboxKind::Blocker
                        | InboxKind::Proposal
                        | InboxKind::PrincipalReview
                        | InboxKind::Merge
                        | InboxKind::Permission => theme::attention(),
                    };
                    let paragraphs = std::iter::once(&waiting.what)
                        .chain(&waiting.points_to)
                        .cloned()
                        .collect();
                    (style, waiting.heading.as_str(), paragraphs)
                })
                .collect();
            if !detail.flags.is_empty() {
                let flags: Vec<String> = detail
                    .flags
                    .iter()
                    .map(|flag| format!("- {flag}"))
                    .collect();
                entries.push((theme::error(), "needs attention", vec![flags.join("\n")]));
            }
            if entries.is_empty() {
                return out;
            }
            out.push('\n');
            out.push_str(&notice_box(entries, width));
        }
        Format::Raw => {
            for waiting in &detail.waiting {
                let line = format!("! {}: {}", waiting.heading, waiting.what);
                let _ = writeln!(
                    out,
                    "  {}",
                    paint(theme::warning(), fill(&line, width, "    "))
                );
                for paragraph in &waiting.points_to {
                    let _ = writeln!(out, "{}", indented(paragraph, "    "));
                }
            }
            for flag in &detail.flags {
                let _ = writeln!(
                    out,
                    "  {}",
                    paint(theme::warning(), fill(&format!("! {flag}"), width, "    "))
                );
            }
        }
    }
    out
}

/// An entry of a notice box: the style of its bar, its heading, and its paragraphs in markdown.
type Notice<'a> = (anstyle::Style, &'a str, Vec<String>);

/// Notices under a bar in their style, each under its heading, the paragraphs wrapped to `width`
/// and never cut.
fn notice_box(entries: Vec<Notice>, width: Option<usize>) -> String {
    let mut out = String::new();
    // The bar and the space after it.
    let inner = width.map(|width| width.saturating_sub(4));
    for (index, (style, heading, paragraphs)) in entries.into_iter().enumerate() {
        let bar = paint(style, "┃");
        if index > 0 {
            let _ = writeln!(out, "  {bar}");
        }
        let heading = paint(style.bold(), heading);
        let _ = writeln!(out, "  {bar} {heading}");
        for paragraph in paragraphs {
            for line in markdown::render(&paragraph, inner, "")
                .trim_end_matches('\n')
                .lines()
            {
                let _ = writeln!(out, "  {bar} {line}");
            }
        }
    }
    out
}

/// A flag of `wf status` alone, in the notice box of an item: its heading, what is wrong, and the
/// command that fixes it on a line of its own.
pub fn flag_detail_for(flag: &Flag, width: Option<usize>) -> String {
    let mut paragraphs = vec![flag.what()];
    paragraphs.extend(flag.command().map(|command| format!("`{command}`")));
    notice_box(vec![(theme::error(), flag.heading(), paragraphs)], width)
}

pub fn item_stats_list(views: &[ItemStatsView]) -> String {
    if views.is_empty() {
        return "no items were worked on\n".to_string();
    }
    let mut table = table(&[
        "ITEM",
        "SESSIONS",
        "SUBMISSIONS",
        "CHANGE REQUESTS",
        "TO DONE",
        "TITLE",
    ]);
    for view in views {
        let suffix = if view.archived { " · archived" } else { "" };
        table.add_row([
            item_label(
                &view.item.id,
                &view.item.item_type,
                view.item.state,
                None,
                suffix,
            ),
            view.stats.sessions.to_string(),
            view.stats.submissions.to_string(),
            view.stats.change_requests.to_string(),
            view.stats
                .lead_ms
                .map_or_else(|| paint(theme::text_dimmed(), "-"), duration),
            view.item.title.clone(),
        ]);
    }
    format!("{table}\n")
}

/// `3 sessions · 2 submissions · 4 change requests · 2d to done`.
pub fn item_stats(stats: &ItemStats) -> String {
    let count = |count: usize, one: &str, many: &str| {
        format!("{count} {}", if count == 1 { one } else { many })
    };
    let mut parts = vec![
        count(stats.sessions, "session", "sessions"),
        count(stats.submissions, "submission", "submissions"),
        count(stats.change_requests, "change request", "change requests"),
    ];
    if let Some(lead) = stats.lead_ms {
        parts.push(format!("{} to done", duration(lead)));
    }
    parts.join(" · ")
}

/// `14m`, `3h05m`, `2d`.
pub fn duration(millis: u64) -> String {
    let minutes = millis / 60_000;
    match minutes {
        0..60 => format!("{minutes}m"),
        60..1440 => format!("{}h{:02}m", minutes / 60, minutes % 60),
        _ => format!("{}d", minutes / 1440),
    }
}

pub fn agent_detail(detail: &AgentDetail) -> String {
    agent_detail_for(detail, text_width(), text_format(), jiff::Timestamp::now())
}

/// [`agent_detail`] with prose wrapped to `width`, or not at all, its markdown in `format`, and
/// the ages of its sessions as of `now`.
pub fn agent_detail_for(
    detail: &AgentDetail,
    width: Option<usize>,
    format: Format,
    now: jiff::Timestamp,
) -> String {
    let agent = &detail.definition;
    let separator = format!(" {} ", paint(theme::text_dimmed(), "·"));
    let mut out = String::new();
    let mut line = |text: String, indent: &str| {
        let _ = writeln!(out, "{}", fill(&text, width, indent));
    };

    let label = paint_actor(&ActorView {
        kind: ActorKind::Agent,
        label: agent.id.clone(),
    });
    let runs_on = match &agent.model {
        Some(model) => format!("{} · {model}", agent.harness),
        None => agent.harness.clone(),
    };
    let header = format!("{label}  {}", paint(theme::text_dimmed(), runs_on));
    let load = agent_load(detail);
    let fits = |text: &str| width.is_none_or(|width| textwrap::core::display_width(text) <= width);
    // The load stays whole: on the header's line if it fits there, else on its own.
    let header_and_load = format!("{header}  {load}");
    if fits(&anstream::adapter::strip_str(&header_and_load).to_string()) {
        line(header_and_load, "  ");
    } else {
        line(header, "  ");
        line(format!("  {load}"), "  ");
    }

    let field = |name: &str, value: &str| format!("{} {value}", paint(theme::text_dimmed(), name));
    let mut fields = Vec::new();
    if !agent.lanes.is_empty() {
        fields.push(field("lanes", &agent.lanes.join(", ")));
    }
    if !agent.capabilities.is_empty() {
        fields.push(field("can", &agent.capabilities.join(", ")));
    }
    fields.push(field("starts", &agent.start));
    fields.push(field("permissions", &agent.permissions));
    line(format!("  {}", fields.join(&separator)), "  ");
    let mut limits = vec![
        field("nudges", &agent.nudges.to_string()),
        field("parks for", &agent.park_timeout),
    ];
    if let Some(max) = &agent.max_duration {
        limits.push(field("at most", max));
    }
    line(format!("  {}", limits.join(&separator)), "  ");
    let hosts: Vec<String> = detail
        .hosts
        .iter()
        .map(|host| {
            let this = if host.this_device { " (this)" } else { "" };
            format!(
                "{}{this} {}",
                host.device,
                paint(theme::text_dimmed(), format!("up to {}", host.sessions))
            )
        })
        .collect();
    if hosts.is_empty() {
        line(
            format!("  ↳ {}", paint(theme::text_dimmed(), "hosted by no device")),
            LINK_INDENT,
        );
    } else {
        line(
            format!("  ↳ hosted by {}", hosts.join(&separator)),
            LINK_INDENT,
        );
    }

    if !detail.sessions.is_empty() {
        let stats = &detail.stats;
        let mut counts = vec![
            stats.sessions.to_string(),
            format!("{} live", stats.live),
            format!("{} finished", stats.finished),
            format!("{} failed", stats.failed),
        ];
        if let Some(median) = stats.median_duration_ms {
            counts.push(format!("median {}", duration(median)));
        }
        line(String::new(), "");
        line(
            format!(
                "{} {}",
                paint(theme::text_bold(), "sessions"),
                paint(theme::text_dimmed(), format!("· {}", counts.join(" · ")))
            ),
            "  ",
        );
        for session in &detail.sessions {
            line(agent_session(session, now, &separator), LINK_INDENT);
        }
        if detail.older_sessions > 0 {
            line(
                format!(
                    "  {}",
                    paint(
                        theme::text_dimmed(),
                        format!("… {} earlier", detail.older_sessions)
                    )
                ),
                "  ",
            );
        }
    }

    if let Some(instructions) = &agent.instructions {
        let _ = writeln!(
            out,
            "\n{}\n{}",
            paint(theme::text_bold(), "instructions"),
            markdown_text(instructions, width, "  ", format)
        );
    }
    if !detail.activity.is_empty() {
        out.push_str(&activity(&detail.activity, width, format));
    }
    out
}

/// `◆ 1/2 busy`: how many sessions of the agent are live, of how many this machine runs at once,
/// or that it does not run it.
fn agent_load(detail: &AgentDetail) -> String {
    let live = detail.stats.live;
    let icon = if live > 0 {
        paint(theme::live(), "◆")
    } else {
        paint(theme::text_dimmed(), "·")
    };
    let load = match detail.hosts.iter().find(|host| host.this_device) {
        Some(host) if live >= host.sessions as usize => {
            paint(theme::live(), format!("{live}/{} busy", host.sessions))
        }
        Some(host) if live > 0 => paint(theme::live(), format!("{live}/{} live", host.sessions)),
        Some(host) => paint(
            theme::text_dimmed(),
            format!("{live}/{} idle", host.sessions),
        ),
        None => paint(
            theme::text_dimmed(),
            format!("{live} live · not hosted here"),
        ),
    };
    format!("{icon} {load}")
}

/// A session of an agent: `◆ #k3j1ab Parse config lazily · work 12m · @ada/coder/s4k2ab`, how
/// long ago it started, or how it ended and how long ago, as of `now`.
fn agent_session(session: &SessionView, now: jiff::Timestamp, separator: &str) -> String {
    let age = |timestamp: &str| {
        timestamp.parse::<jiff::Timestamp>().map_or_else(
            |_| timestamp.to_string(),
            |time| {
                let millis = now.as_millisecond().saturating_sub(time.as_millisecond());
                duration(u64::try_from(millis).unwrap_or(0))
            },
        )
    };
    let (icon, state) = match (session.status.as_str(), &session.ended) {
        ("live", None) => (
            paint(theme::live(), "◆"),
            format!("{} {}", session.role, age(&session.started)),
        ),
        ("parked", None) => (
            paint(theme::live(), "◇"),
            format!("{} parked {}", session.role, age(&session.started)),
        ),
        (reason, ended) => (
            paint(theme::text_dimmed(), "·"),
            format!(
                "{} {} ago",
                reason.replace('_', " "),
                age(ended.as_deref().unwrap_or(&session.started))
            ),
        ),
    };
    let label = paint_actor(&ActorView {
        kind: ActorKind::Agent,
        label: format!("{}/{}", session.agent, short(&session.id)),
    });
    format!(
        "  {icon} {} {}{separator}{}{separator}{label}",
        id_label(&session.item),
        session.title,
        paint(theme::text_dimmed(), state),
    )
}

/// The activity of an item: one line per thing that happened, and the text that goes with it
/// below. The date heads the section and is repeated when the day changes.
fn activity(entries: &[crate::view::ActivityView], width: Option<usize>, format: Format) -> String {
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
                        paint(theme::text_bold(), "activity"),
                        paint(theme::text_dimmed(), format!("· {date}"))
                    );
                }
                Some(_) => {
                    let _ = writeln!(
                        out,
                        "  {}",
                        paint(theme::text_dimmed(), format!("· {date}"))
                    );
                }
            }
            day = Some(date);
        }
        let (line, text) = activity_entry(
            entry,
            &format!("  {}  ", paint(theme::text_dimmed(), time)),
            width,
            format,
        );
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
/// it, if it has any, its markdown in `format`. Both are wrapped to `width` under
/// [`ACTIVITY_TEXT_INDENT`].
fn activity_entry(
    entry: &crate::view::ActivityView,
    lead: &str,
    width: Option<usize>,
    format: Format,
) -> (String, Option<String>) {
    let mut parts = vec![paint_actor(&entry.by)];
    match &entry.comment {
        Some(comment) if entry.kind == ActivityKind::Comment => {
            match comment.kind {
                CommentKindView::Blocker | CommentKindView::ChangeRequest => parts.push(paint(
                    theme::warning(),
                    format!(
                        "{} {}",
                        comment_marker(comment.kind),
                        comment_kind_label(comment.kind)
                    ),
                )),
                CommentKindView::Comment if comment.resolves.is_empty() => {
                    parts.push(paint(theme::text_bold(), "commented"))
                }
                CommentKindView::Comment => {}
            }
            if !comment.resolves.is_empty() {
                parts.push(paint(
                    theme::text_bold(),
                    format!("resolved {}", comment.resolves.join(", ")),
                ));
            }
            if let Some(by) = &comment.resolved_by {
                parts.push(paint(theme::text_dimmed(), format!("resolved by {by}")));
            }
            match &comment.step {
                Some(StepRefView {
                    number: Some(number),
                    ..
                }) => parts.push(paint(theme::text_dimmed(), format!("on step {number}"))),
                Some(StepRefView { .. }) => {
                    parts.push(paint(theme::text_dimmed(), "on a step since edited"))
                }
                None => {}
            }
            parts.push(paint(theme::text_dimmed(), &comment.short));
        }
        Some(_) | None => parts.push(paint(theme::text_bold(), &entry.summary)),
    }
    let line = format!(
        "{lead}{}",
        parts.join(&format!(" {} ", paint(theme::text_dimmed(), "·")))
    );
    let line = fill(&line, width, ACTIVITY_TEXT_INDENT);
    let text = entry
        .text
        .as_deref()
        .filter(|text| !text.trim().is_empty())
        .map(|text| markdown_text(text, width, ACTIVITY_TEXT_INDENT, format));
    (line, text)
}

/// One line of `wf log --follow`: `14:02  #k3f7ab  @ada/coder · moved to done`, and the text that goes
/// with it below.
pub fn event_line(line: &crate::view::EventLineView) -> String {
    event_line_for(line, text_width(), text_format())
}

/// [`event_line`] wrapped to `width`, or not at all, its markdown in `format`.
pub fn event_line_for(
    line: &crate::view::EventLineView,
    width: Option<usize>,
    format: Format,
) -> String {
    let (_, time) = local_date_and_time(&line.activity.at);
    let item = match &line.item {
        Some(item) => format!("{}  ", id_label(item)),
        None => String::new(),
    };
    let lead = format!("{}  {item}", paint(theme::text_dimmed(), time));
    let (entry, text) = activity_entry(&line.activity, &lead, width, format);
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

/// The body, indented by two columns, with each step's number before its text:
/// `- [ ] 1. write parser`. Styled, the markdown is rendered and wrapped to `width` and a step's
/// text continues under its text.
fn body(parts: &[BodyPartView], width: Option<usize>, format: Format) -> String {
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
            BodyPartView::Markdown { text } => {
                out.push_str(&markdown_text(text, width, "  ", format));
            }
            BodyPartView::Step { number, done, text } => {
                let checkbox = if *done { "[x]" } else { "[ ]" };
                let number = paint(theme::text_dimmed(), format!("{number}."));
                let marker = format!("- {checkbox} {number} ");
                let step = match format {
                    Format::Styled => markdown::render_after(&marker, text, width, "  ")
                        .trim_end_matches('\n')
                        .to_string(),
                    Format::Raw => indented(&format!("{marker}{text}"), "  "),
                };
                out.push_str(&step);
            }
        }
        previous_was_step = is_step;
    }
    out.push('\n');
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
            if agent.harness_startable_here {
                agent.harness.clone()
            } else {
                format!("{} (not startable here)", agent.harness)
            },
            agent.model.clone().unwrap_or_else(|| "default".to_string()),
            agent.capabilities.join(", "),
            agent.lanes.join(", "),
            agent.start.clone(),
            agent.permissions.clone(),
        ]);
    }
    format!("{table}\n")
}

/// Device ids are shown with 8 characters, as before ids got their own short length.
const DEVICE_ID_LEN: usize = 8;

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
            device
                .device
                .chars()
                .take(DEVICE_ID_LEN)
                .collect::<String>(),
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

pub fn log(events: &[EventView]) -> String {
    let mut table = table(&["EVENT", "DEVICE", "SEQ", "AT", "KIND", "ACTOR", "SUMMARY"]);
    for event in events {
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
    let verified = events.iter().filter(|event| event.verified).count();
    format!("{table}\n{verified}/{} events verified\n", events.len())
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
        "items: {} ready, {} archived",
        view.ready_items, view.archived_items,
    );
    let _ = writeln!(out, "waiting on you: {}", view.waiting_on_you);
    for line in &view.after_failures {
        let _ = writeln!(out, "waits for you: {line}");
    }
    for flag in &view.flagged {
        let _ = writeln!(out, "! {flag}");
    }
    if let Some(next) = &view.next {
        let _ = writeln!(out, "next: {next}");
    }
    if let Some(caches) = &view.caches {
        let _ = writeln!(
            out,
            "caches: {} of {}",
            bytesize::ByteSize::b(caches.used_bytes).display().si(),
            bytesize::ByteSize::b(caches.budget_bytes).display().si()
        );
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
    fn the_device_table_prints_its_real_headers() {
        let header = devices(&[]);
        assert!(header.contains("PRINCIPAL"), "{header}");
        assert!(!header.contains("theme::"), "{header}");
    }

    #[test]
    fn status_prints_what_waits_on_the_principal_apart_from_items() {
        let view = StatusView {
            name: "demo".to_string(),
            id: "abc".to_string(),
            members: vec!["@eulervoid (owner)".to_string()],
            agents: 1,
            events: 10,
            pending: 0,
            heads: 1,
            archived_items: 3,
            forked_from: None,
            devices: Vec::new(),
            daemon: true,
            ready_items: 6,
            live_sessions: 2,
            parked_sessions: 0,
            after_failures: Vec::new(),
            waiting_on_you: 2,
            flagged: Vec::new(),
            next: None,
            refolded: false,
            caches: None,
        };
        let output = status(&view);
        let lines: Vec<&str> = output.lines().collect();
        assert!(lines.contains(&"items: 6 ready, 3 archived"), "{output}");
        assert!(lines.contains(&"waiting on you: 2"), "{output}");
        let items_line = lines.iter().find(|line| line.starts_with("items:")).unwrap();
        assert!(!items_line.contains("waiting"), "{output}");
    }

    #[test]
    fn an_item_that_references_this_one_several_times_is_listed_once() {
        use wf_core::id::{CommentId, HashId, ItemId};
        use wf_core::item::ItemView;
        use wf_core::testing::*;

        let fixture = Fixture::new(1);
        let mut state = fixture.state();
        let mut now = 2_000;
        let mut apply = |state: &mut wf_core::state::State, body| {
            now += 1_000;
            fixture.apply(state, 0, now, body)
        };
        let target: ItemId = apply(&mut state, create("task", "Target")).id().into();
        let note = apply(&mut state, comment(target, "a note to point at"));
        let note = CommentId::from(note.id()).short();
        let mention = format!("see #{}", target.short());
        let mut referencing = |title: &str, body: Option<&str>, comments: &[&str]| -> ItemId {
            let created = match body {
                Some(body) => create_with_body("task", title, body),
                None => create("task", title),
            };
            let item: ItemId = apply(&mut state, created).id().into();
            for text in comments {
                apply(&mut state, comment(item, text));
            }
            item
        };
        let body_only = referencing("Body only", Some(&mention), &[]);
        let everywhere = referencing("Everywhere", Some(&mention), &[&mention, &mention]);
        let one_comment = referencing("One comment", None, &[&mention]);
        let comments_only = referencing("Comments only", None, &[&mention, &mention]);
        let note_and_item = referencing(
            "Note and item",
            Some(&mention),
            &[&format!("see #{}/{note}", target.short())],
        );

        let ItemView::Live(item) = state.item(target).unwrap() else {
            panic!()
        };
        let detail = crate::view::item_detail(&state, item, &[]);
        let shown =
            anstream::adapter::strip_str(&item_detail_for(&detail, None, Format::Raw)).to_string();
        let mut backlinks: Vec<&str> = shown
            .lines()
            .filter(|line| line.contains("referenced by"))
            .map(str::trim)
            .collect();
        backlinks.sort();
        let line = |item: ItemId, rest: &str| format!("↳ referenced by #{}{rest}", item.short());
        let mut expected = vec![
            line(body_only, " \"Body only\""),
            line(everywhere, " \"Everywhere\""),
            line(one_comment, " \"One comment\""),
            line(comments_only, " \"Comments only\""),
            line(note_and_item, " \"Note and item\""),
            format!(
                "↳ {note} referenced by #{} \"Note and item\"",
                note_and_item.short()
            ),
        ];
        expected.sort();
        assert_eq!(backlinks, expected);
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
        let (line, text) = activity_entry(&entry, "  00:27  ", Some(44), Format::Styled);
        assert_eq!(
            anstream::adapter::strip_str(&line).to_string(),
            "  00:27  @ada/coder/bh2m · checked step 1"
        );
        assert_eq!(
            anstream::adapter::strip_str(&text.unwrap()).to_string(),
            "           wrap long activity lines under\n           their indent"
        );
        let (line, text) = activity_entry(&entry, "  00:27  ", None, Format::Raw);
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
        let (line, _) = activity_entry(&entry, "  00:27  ", Some(40), Format::Styled);
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
                state: ItemState::Open,
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
            stats: Default::default(),
            waiting: Vec::new(),
            requires: Vec::new(),
            depends_on: Vec::new(),
            required_by: Vec::new(),
            supersedes: None,
            references: vec![crate::view::ReferenceView {
                item: crate::view::LinkView {
                    id: "kvb7vjaaaaaa".to_string(),
                    item_type: "task".to_string(),
                    title: referenced.to_string(),
                    state: ItemState::Open,
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
        let shown =
            anstream::adapter::strip_str(&item_detail_for(&detail, Some(40), Format::Styled))
                .to_string();
        assert_eq!(
            shown,
            "○ #dm3q4j (task:open) Wrap the header of\n\
             \x20                     wf item show under\n\
             \x20                     its indent\n\
             \x20 @ada/coder/bh2m · created not a\n\
             \x20 timestamp · updated not a timestamp\n\
             \x20 ↳ references ○ #kvb7vj (task:open)\n\
             \x20   \"Wrap long activity lines under\n\
             \x20   their indent\"\n"
        );
        let shown =
            anstream::adapter::strip_str(&item_detail_for(&detail, None, Format::Raw)).to_string();
        assert_eq!(
            shown,
            "○ #dm3q4j (task:open) Wrap the header of wf item show under its indent\n\
             \x20 @ada/coder/bh2m · created not a timestamp · updated not a timestamp\n\
             \x20 ↳ references ○ #kvb7vj (task:open) \"Wrap long activity lines under their indent\"\n"
        );
    }

    const BODY: &str = "## Plan\n\nKeep **bold** and `#k3f9` as links.\n\n- one\n- two\n\n```\nlet value = 1;\n```\n\n| who | what it does to the item |\n|-----|--------------------------|\n| ada | renders it               |";

    fn detail_with_a_body() -> ItemDetail {
        let mut detail = detail_with_a_reference("Render", "Other");
        detail.references.clear();
        detail.body = vec![
            BodyPartView::Markdown {
                text: BODY.to_string(),
            },
            BodyPartView::Step {
                number: 1,
                done: true,
                text: "render the **body**".to_string(),
            },
            BodyPartView::Step {
                number: 2,
                done: false,
                text: "render the comments under their line".to_string(),
            },
        ];
        detail
    }

    fn body_of(shown: &str) -> String {
        let plain = anstream::adapter::strip_str(shown).to_string();
        let (_, body) = plain.split_once("\n\n").unwrap();
        body.to_string()
    }

    #[test]
    fn a_body_is_rendered_when_styled() {
        let shown = item_detail_for(&detail_with_a_body(), Some(30), Format::Styled);
        assert!(shown.contains(&paint(theme::text_bold(), "bold")));
        assert!(shown.contains(&paint(theme::text_dimmed(), "2.")));
        assert_eq!(
            body_of(&shown),
            "  Plan\n\
             \n\
             \x20 Keep bold and #k3f9 as\n\
             \x20 links.\n\
             \n\
             \x20 • one\n\
             \x20 • two\n\
             \n\
             \x20     let value = 1;\n\
             \n\
             \x20 | who | what it does to the item |\n\
             \x20 |-----|--------------------------|\n\
             \x20 | ada | renders it               |\n\
             \n\
             \x20 - [x] 1. render the body\n\
             \x20 - [ ] 2. render the comments\n\
             \x20          under their line\n"
        );
    }

    #[test]
    fn a_body_is_its_markdown_when_raw() {
        let shown = item_detail_for(&detail_with_a_body(), None, Format::Raw);
        let indented: String = BODY
            .lines()
            .map(|line| match line {
                "" => "\n".to_string(),
                line => format!("  {line}\n"),
            })
            .collect();
        assert_eq!(
            body_of(&shown),
            format!(
                "{indented}\n  - [x] 1. render the **body**\n  - [ ] 2. render the comments under their line\n"
            )
        );
    }

    #[test]
    fn a_comment_is_rendered_under_its_line_at_the_comment_indent() {
        let by = crate::view::ActorView {
            kind: crate::view::ActorKind::Agent,
            label: "@ada/coder/bh2m".to_string(),
        };
        let entry = crate::view::ActivityView {
            at: "not a timestamp".to_string(),
            by: by.clone(),
            kind: ActivityKind::Comment,
            summary: "commented".to_string(),
            comment: Some(crate::view::CommentView {
                id: "nn73nn73nn73".to_string(),
                short: "nn73".to_string(),
                kind: CommentKindView::Comment,
                by,
                at: "not a timestamp".to_string(),
                session: None,
                step: None,
                resolves: Vec::new(),
                resolved_by: None,
            }),
            text: Some("Chose **this**:\n\n- see `#k3f9`".to_string()),
        };
        let (line, text) = activity_entry(&entry, "  00:27  ", Some(60), Format::Styled);
        assert_eq!(
            anstream::adapter::strip_str(&line).to_string(),
            "  00:27  @ada/coder/bh2m · commented · nn73"
        );
        assert_eq!(
            anstream::adapter::strip_str(&text.unwrap()).to_string(),
            format!("{ACTIVITY_TEXT_INDENT}Chose this:\n\n{ACTIVITY_TEXT_INDENT}• see #k3f9")
        );
        let (_, text) = activity_entry(&entry, "  00:27  ", None, Format::Raw);
        assert_eq!(
            text.unwrap(),
            format!("{ACTIVITY_TEXT_INDENT}Chose **this**:\n\n{ACTIVITY_TEXT_INDENT}- see `#k3f9`")
        );
    }

    /// The detail of an approved item whose merge waits on a dirty checkout, which the daemon
    /// commented on twice.
    fn held_merge() -> ItemDetail {
        merge_pending(
            &[],
            &[
                "Waiting to merge: main kept moving.",
                "Waiting to merge: main is checked out in /src/app with changes to src/lib.rs, Cargo.toml. Commit or stash them.",
            ],
        )
    }

    /// The detail of an approved item whose merge is pending. With `earlier` comments, the daemon
    /// made them on an earlier submission, before the latest one; `after` it made on the latest.
    fn merge_pending(earlier: &[&str], after: &[&str]) -> ItemDetail {
        use wf_core::id::{ItemId, SessionId};
        use wf_core::identity::Actor;
        use wf_core::item::ItemView;
        use wf_core::session::{CommitId, EndReason, Verdict};
        use wf_core::testing::*;

        let fixture = Fixture::new(1);
        let mut state = fixture.state();
        let mut events = Vec::new();
        let mut now = 2_000;
        let mut apply = |state: &mut wf_core::state::State, actor: Actor, body| {
            now += 1_000;
            let event = fixture.author_as(state, 0, actor, now, body);
            state
                .receive(event.clone(), wf_core::state::Trust::Untrusted)
                .unwrap();
            events.push(event.clone());
            event
        };
        let principal = fixture.founder_actor();
        let daemon = Actor::Daemon(fixture.founder.key());
        let commit = CommitId::Git("75eba435".repeat(5).parse().unwrap());
        apply(&mut state, principal.clone(), define_agent("coder"));
        let item: ItemId = apply(
            &mut state,
            principal.clone(),
            create_reviewed("task", "Held", "standard"),
        )
        .id()
        .into();
        let submitted = EndReason::Submitted {
            commit: Some(commit),
        };
        if !earlier.is_empty() {
            let session: SessionId =
                apply(&mut state, principal.clone(), start_session("coder", item))
                    .id()
                    .into();
            apply(
                &mut state,
                principal.clone(),
                end_session(session, submitted),
            );
            for text in earlier {
                apply(&mut state, daemon.clone(), comment(item, text));
            }
        }
        let session: SessionId = apply(&mut state, principal.clone(), start_session("coder", item))
            .id()
            .into();
        apply(
            &mut state,
            principal.clone(),
            end_session(session, submitted),
        );
        apply(
            &mut state,
            principal.clone(),
            moved(item, ItemState::Review),
        );
        apply(
            &mut state,
            principal.clone(),
            verdict(item, commit, Verdict::Approve),
        );
        for text in after {
            apply(&mut state, daemon.clone(), comment(item, text));
        }
        let ItemView::Live(live) = state.item(item).unwrap() else {
            panic!()
        };
        crate::view::item_detail(&state, live, &events)
    }

    /// The lines of the notice box, without the bar.
    fn boxed(shown: &str) -> Vec<String> {
        anstream::adapter::strip_str(shown)
            .to_string()
            .lines()
            .filter_map(|line| line.strip_prefix("  ┃"))
            .map(|line| line.trim().to_string())
            .collect()
    }

    #[test]
    fn a_held_merge_shows_in_full_with_its_files_in_the_box() {
        let detail = held_merge();
        let what = &detail.waiting[0].what;
        assert!(what.contains("is ready to merge"), "{what}");
        let shown = item_detail_for(&detail, Some(40), Format::Styled);
        let plain = anstream::adapter::strip_str(&shown).to_string();
        assert!(
            plain
                .lines()
                .all(|line| textwrap::core::display_width(line) <= 40),
            "{plain}"
        );
        let boxed = boxed(&shown);
        assert_eq!(boxed[0], "merge waits");
        let text = boxed[1..].join(" ");
        assert!(text.contains(what.as_str()), "{text}");
        assert!(
            text.contains("with changes to src/lib.rs, Cargo.toml. Commit or stash them."),
            "{text}"
        );
        assert!(!text.contains("kept moving"), "only the latest: {text}");
        let body = plain.find("activity").unwrap();
        assert!(plain.find("merge waits").unwrap() < body, "{plain}");
    }

    #[test]
    fn a_daemon_comment_from_an_earlier_round_is_not_in_the_box() {
        let detail = merge_pending(
            &[
                "Merging into main conflicts in src/lib.rs. The next session merges main into the branch first.",
            ],
            &[],
        );
        let shown = item_detail_for(&detail, Some(60), Format::Styled);
        let boxed = boxed(&shown);
        assert_eq!(boxed[0], "merge waits");
        assert!(detail.waiting[0].points_to.is_empty(), "{boxed:?}");
        assert!(!boxed.join(" ").contains("conflicts"), "{boxed:?}");
    }

    #[test]
    fn an_item_with_nothing_waiting_has_no_box() {
        let shown = item_detail_for(&detail_with_a_body(), Some(40), Format::Styled);
        assert!(boxed(&shown).is_empty(), "{shown}");
    }

    #[test]
    fn piped_what_waits_and_the_flags_are_plain_lines() {
        let mut detail = held_merge();
        detail.flags = vec!["a task needs a priority".to_string()];
        let shown =
            anstream::adapter::strip_str(&item_detail_for(&detail, None, Format::Raw)).to_string();
        assert!(!shown.contains('┃'), "{shown}");
        let expected = format!(
            "  ! merge waits: {}\n    Waiting to merge: main is checked out in /src/app with changes to src/lib.rs, Cargo.toml. Commit or stash them.\n  ! a task needs a priority\n",
            detail.waiting[0].what
        );
        assert!(shown.contains(&expected), "{shown}");
    }
}

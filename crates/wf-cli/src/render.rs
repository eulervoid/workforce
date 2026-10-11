//! Plain text output, rendered from the same views `--json` prints.

use std::fmt::Write;

use comfy_table::presets::NOTHING;
use comfy_table::{Attribute, Cell, ContentArrangement, Table};

use crate::cli::ItemGrouping;
use crate::devices::{DeviceView, MemberView};
use crate::view::{
    AgentView, BodyPartView, ItemDetail, ItemGroup, ItemSummary, LogView, MentionsView, StatusView,
    StepRefView,
};
use wf_core::graph::Readiness;
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

fn origin(origin: &str) -> &str {
    if origin == "body" { "body" } else { "comment" }
}

/// Colors are always written as ANSI escapes; `anstream` strips them when output is not a terminal.
mod style {
    use anstyle::{AnsiColor, Style};

    pub const BOLD: Style = Style::new().bold();
    pub const DIM: Style = Style::new().dimmed();
    pub const ID: Style = Style::new().fg_color(Some(anstyle::Color::Ansi(AnsiColor::Cyan)));
    pub const PERSON: Style = Style::new().fg_color(Some(anstyle::Color::Ansi(AnsiColor::Magenta)));
    pub const WARNING: Style = Style::new()
        .bold()
        .fg_color(Some(anstyle::Color::Ansi(AnsiColor::Red)));

    pub fn color(color: AnsiColor) -> Style {
        Style::new().fg_color(Some(anstyle::Color::Ansi(color)))
    }

    pub fn paint(style: Style, text: impl std::fmt::Display) -> String {
        format!("{style}{text}{style:#}")
    }
}

fn state_style(state_category: Option<&str>) -> anstyle::Style {
    use anstyle::AnsiColor;
    match state_category {
        Some("open") => style::color(AnsiColor::Green),
        Some("active") => style::color(AnsiColor::Blue),
        Some("blocked") => style::color(AnsiColor::Red),
        Some("done") => style::color(AnsiColor::Magenta),
        Some("cancelled") => style::color(AnsiColor::BrightBlack),
        _ => style::DIM,
    }
}

/// The icon of a state category; `?` when the state is unknown under the current policy.
fn state_icon(state_category: Option<&str>) -> &'static str {
    match state_category {
        Some("inactive") => "◌",
        Some("open") => "○",
        Some("active") => "◐",
        Some("blocked") => "⊘",
        Some("done") => "✓",
        Some("cancelled") => "✗",
        _ => "?",
    }
}

/// `◐ in_progress`. Pickers take plain text, so the icon is not colored.
pub fn state_choice(name: &str, state_category: StateCategory) -> String {
    format!("{} {name}", state_icon(Some(&state_category.to_string())))
}

fn priority_style(priority: &str) -> anstyle::Style {
    match priority {
        "P0" => style::WARNING,
        "P1" => style::color(anstyle::AnsiColor::Yellow),
        "P2" => anstyle::Style::new(),
        _ => style::DIM,
    }
}

/// `2026-10-09 21:42` in the local time zone; text that is not a timestamp is kept as it is.
fn short_time(timestamp: &str) -> String {
    timestamp.parse::<jiff::Timestamp>().map_or_else(
        |_| timestamp.to_string(),
        |time| {
            time.to_zoned(jiff::tz::TimeZone::system())
                .strftime("%Y-%m-%d %H:%M")
                .to_string()
        },
    )
}

/// The icon of an item: its state's icon, or a warning icon when the item is stranded.
pub fn item_icon(state_category: Option<&str>, readiness: Option<Readiness>) -> &'static str {
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
        item_icon(summary.state_category.as_deref(), summary.readiness),
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
    state_category: Option<&str>,
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

const ITEM_COLUMNS: [&str; 4] = ["ITEM (TYPE:STATE)", "PRIORITY", "TITLE", "BLOCKING"];

fn item_row(item: &ItemSummary) -> [String; 4] {
    let suffix = if item.archived { " · archived" } else { "" };
    let priority = item.priority.as_deref().map_or_else(
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
            item.state_category.as_deref(),
            item.readiness,
            suffix,
        ),
        priority,
        item.title.clone(),
        blocking,
    ]
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
            state_icon(first.state_category.as_deref()),
            state_style(first.state_category.as_deref()),
        ),
        ItemGrouping::Readiness => match first.readiness {
            Some(Readiness::Ready) => ("○", state_style(Some("open"))),
            Some(Readiness::Claimed) => ("◐", state_style(Some("active"))),
            Some(Readiness::Blocked) => ("⊘", state_style(Some("blocked"))),
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
            table.add_row([""; 4]);
        }
        table.add_row([
            group_heading(grouping, group),
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

pub fn item_detail(detail: &ItemDetail) -> String {
    use style::{BOLD, DIM, PERSON, WARNING, paint};

    let summary = &detail.summary;
    let mut out = String::new();
    let archived = if summary.archived { "[archived]" } else { "" };
    let _ = writeln!(
        out,
        "{} {}",
        item_label(
            &summary.id,
            &summary.item_type,
            summary.state.as_deref(),
            summary.state_category.as_deref(),
            summary.readiness,
            archived
        ),
        paint(BOLD, &summary.title)
    );

    let mut status = Vec::new();
    if let Some(priority) = &summary.priority {
        status.push(paint(priority_style(priority), priority));
    }
    if let Some(lane) = &detail.lane {
        status.push(paint(DIM, format!("lane={lane}")));
    }
    if let Some(review) = &detail.review {
        status.push(paint(DIM, format!("review={review}")));
    }
    if !detail.labels.is_empty() {
        status.push(paint(DIM, format!("labels={}", detail.labels.join(","))));
    }
    if !detail.requires.is_empty() {
        status.push(paint(
            DIM,
            format!("requires={}", detail.requires.join(",")),
        ));
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
        let _ = writeln!(out, "  {}", status.join("  "));
    }

    let mut attributes = Vec::new();
    attributes.push(paint(PERSON, &detail.author));
    attributes.push(paint(
        DIM,
        format!(
            "created {} · updated {}",
            short_time(&detail.created),
            short_time(&detail.updated)
        ),
    ));
    let _ = writeln!(out, "  {}", attributes.join("  "));
    for flag in &detail.flags {
        let _ = writeln!(out, "  {}", paint(WARNING, format!("! {flag}")));
    }

    let links = |label: &str, links: &[crate::view::LinkView], out: &mut String| {
        for link in links {
            let _ = writeln!(
                out,
                "  ↳ {label} {} \"{}\"{}",
                item_label(
                    &link.id,
                    &link.item_type,
                    link.state.as_deref(),
                    link.state_category.as_deref(),
                    None,
                    ""
                ),
                link.title,
                if link.ignored {
                    " [ignored: cycle]"
                } else {
                    ""
                }
            );
        }
    };
    links("depends on", &detail.depends_on, &mut out);
    links("required by", &detail.required_by, &mut out);
    links("supersedes", detail.supersedes.as_slice(), &mut out);
    links("references", &detail.references, &mut out);
    for source in &detail.referenced_by {
        let _ = writeln!(
            out,
            "  ↳ referenced by {} \"{}\" ({})",
            id_label(&source.item),
            source.title,
            origin(&source.origin)
        );
    }
    if !detail.mentions.is_empty() {
        let mentions: Vec<String> = detail
            .mentions
            .iter()
            .map(|mention| paint(PERSON, mention))
            .collect();
        let _ = writeln!(out, "  ↳ mentions {}", mentions.join(" "));
    }
    if !detail.body.is_empty() {
        out.push('\n');
        for line in body(&detail.body).lines() {
            let _ = writeln!(out, "  {line}");
        }
    }
    if !detail.notes.is_empty() {
        let _ = writeln!(out, "\n{}", paint(BOLD, "discussion"));
        for note in &detail.notes {
            let kind = match &note.session {
                Some(session) => format!("({}: {session})", note.kind),
                None => format!("({})", note.kind),
            };
            let _ = writeln!(
                out,
                "  {} {} {}: {}",
                paint(DIM, short_time(&note.at)),
                paint(PERSON, &note.by),
                paint(DIM, kind),
                note.text
                    .as_deref()
                    .unwrap_or("[text not in the local log]")
            );
            match &note.step {
                Some(StepRefView {
                    number: Some(number),
                    text: Some(text),
                }) => {
                    let first_line = text.lines().next().unwrap_or_default();
                    let _ = writeln!(
                        out,
                        "    {}",
                        paint(DIM, format!("↳ step {number}: {first_line}"))
                    );
                }
                Some(StepRefView { .. }) => {
                    let _ = writeln!(out, "    {}", paint(DIM, "↳ a step since edited"));
                }
                None => {}
            }
        }
    }
    out
}

/// The body as markdown with each step's number after its checkbox: `- [ ](1) write parser`.
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
                let number = style::paint(style::DIM, format!("({number})"));
                let _ = write!(out, "- {checkbox}{number} {text}");
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
            origin(&source.origin).to_string(),
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
                device.last_seen.clone().unwrap_or_else(|| "-".to_string())
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
            event.at.to_string(),
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
    let _ = writeln!(out, "archived items: {}", view.archived_items);
    let devices: Vec<String> = view
        .devices
        .iter()
        .map(|device| match (&device.last_seen, device.this_device) {
            (_, true) => format!("{} (this)", device.name),
            (Some(seen), false) => format!("{} (seen {seen})", device.name),
            (None, false) => device.name.clone(),
        })
        .collect();
    let _ = writeln!(out, "devices: {}", devices.join(", "));
    for flag in &view.flagged {
        let _ = writeln!(out, "! {flag}");
    }
    if view.refolded {
        let _ = writeln!(out, "refolded from the log and wrote a fresh snapshot");
    }
    out
}

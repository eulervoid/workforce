//! Plain text output, rendered from the same views `--json` prints.

use std::fmt::Write;

use comfy_table::presets::NOTHING;
use comfy_table::{Attribute, Cell, ContentArrangement, Table};

use crate::view::{AgentView, ItemDetail, ItemSummary, LogView, MentionsView, StatusView};

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

/// `open · archived`
fn state_label(summary: &ItemSummary) -> String {
    let mut label = summary
        .state
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    if summary.archived {
        label.push_str(" · archived");
    }
    if summary.category.is_none() && !summary.archived {
        label.push_str(" · unknown state");
    }
    label
}

/// `#q7x2 (question, open)`, one per line
fn blockers(summary: &ItemSummary) -> String {
    summary
        .blocked_by
        .iter()
        .map(|blocker| {
            format!(
                "#{} ({}, {})",
                short(&blocker.id),
                blocker.item_type,
                blocker.state.as_deref().unwrap_or("unknown")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn item_list(items: &[ItemSummary]) -> String {
    if items.is_empty() {
        return "no items\n".to_string();
    }
    let mut table = table(&["ID", "Type", "Pri", "Title", "State", "Blocked by"]);
    for item in items {
        table.add_row([
            format!("#{}", short(&item.id)),
            item.item_type.clone(),
            item.priority.clone().unwrap_or_else(|| "-".to_string()),
            item.title.clone(),
            state_label(item),
            blockers(item),
        ]);
    }
    format!("{table}\n")
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

fn state_style(category: Option<&str>) -> anstyle::Style {
    use anstyle::AnsiColor;
    match category {
        Some("open") => style::color(AnsiColor::Yellow),
        Some("active") => style::color(AnsiColor::Blue),
        Some("blocked") => style::color(AnsiColor::Red),
        Some("done") => style::color(AnsiColor::Green),
        _ => style::DIM,
    }
}

fn priority_style(priority: &str) -> anstyle::Style {
    match priority {
        "P0" => style::WARNING,
        "P1" => style::color(anstyle::AnsiColor::Yellow),
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

/// `● #q7x2 (task:open) `, the icon and state colored by category. `suffix` follows the state.
fn item_label(
    id: &str,
    item_type: &str,
    state: Option<&str>,
    category: Option<&str>,
    suffix: &str,
) -> String {
    let state_style = state_style(category);
    let icon = if category.is_some() { "●" } else { "?" };
    format!(
        "{} {} {}{}{}{}",
        style::paint(state_style, icon),
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

pub fn item_detail(detail: &ItemDetail) -> String {
    use style::{BOLD, DIM, PERSON, WARNING, paint};

    let summary = &detail.summary;
    let mut out = String::new();
    let archived = if summary.archived { " · archived" } else { "" };
    let _ = writeln!(
        out,
        "{} {}",
        item_label(
            &summary.id,
            &summary.item_type,
            summary.state.as_deref(),
            summary.category.as_deref(),
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
    if !detail.labels.is_empty() {
        attributes.push(paint(DIM, format!("labels={}", detail.labels.join(","))));
    }
    if !detail.requires.is_empty() {
        attributes.push(paint(
            DIM,
            format!("requires={}", detail.requires.join(",")),
        ));
    }
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
                    link.category.as_deref(),
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
    if !detail.body.trim().is_empty() {
        out.push('\n');
        for line in detail.body.trim_end().lines() {
            let _ = writeln!(out, "  {line}");
        }
    }
    if !detail.notes.is_empty() {
        let _ = writeln!(out, "\n{}", paint(BOLD, "discussion"));
        for note in &detail.notes {
            let _ = writeln!(
                out,
                "  {} {} {}: {}",
                paint(DIM, short_time(&note.at)),
                paint(PERSON, &note.by),
                paint(DIM, format!("({})", note.kind)),
                note.text
                    .as_deref()
                    .unwrap_or("[text not in the local log]")
            );
        }
    }
    out
}

pub fn mentions(view: &MentionsView) -> String {
    if view.sources.is_empty() {
        return format!("{} is not mentioned anywhere\n", view.target);
    }
    let mut table = table(&["ID", "Title", "In"]);
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
    let mut table = table(&["Agent", "Harness", "Model", "Capabilities", "Lanes"]);
    for agent in agents {
        table.add_row([
            agent.id.clone(),
            agent.harness.clone(),
            agent.model.clone().unwrap_or_else(|| "default".to_string()),
            agent.capabilities.join(", "),
            agent.lanes.join(", "),
        ]);
    }
    format!("{table}\n")
}

pub fn log(view: &LogView) -> String {
    let mut table = table(&["Event", "Device", "Seq", "At", "Kind", "Actor", "Summary"]);
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
    let _ = writeln!(out, "members: {}", view.members.join(", "));
    let _ = writeln!(
        out,
        "events: {}  heads: {}  pending: {}  agents: {}",
        view.events, view.heads, view.pending, view.agents
    );
    let categories: Vec<String> = view
        .items_by_category
        .iter()
        .map(|(category, count)| format!("{count} {category}"))
        .collect();
    let _ = writeln!(
        out,
        "items: {} ({} archived)",
        if categories.is_empty() {
            "none".to_string()
        } else {
            categories.join(", ")
        },
        view.archived_items
    );
    for flag in &view.flagged {
        let _ = writeln!(out, "! {flag}");
    }
    if view.refolded {
        let _ = writeln!(out, "refolded from the log and wrote a fresh snapshot");
    }
    out
}

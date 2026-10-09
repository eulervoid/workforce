//! Plain text output, rendered from the same views `--json` prints.

use std::fmt::Write;

use crate::view::{AgentView, ItemDetail, ItemSummary, LogView, MentionsView, StatusView};

fn short(id: &str) -> &str {
    &id[..id.len().min(wf_core::id::SHORT_ID_LEN)]
}

/// `open · blocked by #q7x2 (question, open)`
pub fn effective_status(summary: &ItemSummary) -> String {
    let mut status = summary
        .state
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    if summary.archived {
        status.push_str(" · archived");
    }
    if summary.category.is_none() && !summary.archived {
        status.push_str(" · unknown state");
    }
    if !summary.blocked_by.is_empty() {
        let blockers: Vec<String> = summary
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
            .collect();
        let _ = write!(status, " · blocked by {}", blockers.join(", "));
    }
    status
}

pub fn item_list(items: &[ItemSummary]) -> String {
    if items.is_empty() {
        return "no items\n".to_string();
    }
    let mut out = String::new();
    for item in items {
        let _ = writeln!(
            out,
            "#{}  {:<9} {:<3} {}  [{}]",
            short(&item.id),
            item.item_type,
            item.priority.as_deref().unwrap_or("-"),
            item.title,
            effective_status(item)
        );
    }
    out
}

pub fn item_detail(detail: &ItemDetail) -> String {
    let summary = &detail.summary;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "#{}  {}  {}",
        short(&summary.id),
        summary.item_type,
        summary.priority.as_deref().unwrap_or("")
    );
    let _ = writeln!(out, "{}", summary.title);
    let _ = writeln!(out, "{}", effective_status(summary));
    let _ = writeln!(
        out,
        "by {} · created {} · updated {}",
        detail.author, detail.created, detail.updated
    );
    let mut attributes = Vec::new();
    if let Some(lane) = &detail.lane {
        attributes.push(format!("lane: {lane}"));
    }
    if !detail.labels.is_empty() {
        attributes.push(format!("labels: {}", detail.labels.join(", ")));
    }
    if !detail.requires.is_empty() {
        attributes.push(format!("requires: {}", detail.requires.join(", ")));
    }
    if !attributes.is_empty() {
        let _ = writeln!(out, "{}", attributes.join("  "));
    }
    for flag in &detail.flags {
        let _ = writeln!(out, "! {flag}");
    }
    let links = |label: &str, links: &[crate::view::LinkView], out: &mut String| {
        for link in links {
            let _ = writeln!(
                out,
                "{label}: #{} {} \"{}\" ({}){}",
                short(&link.id),
                link.item_type,
                link.title,
                link.state.as_deref().unwrap_or("unknown"),
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
            "referenced by: #{} \"{}\" ({})",
            short(&source.item),
            source.title,
            if source.origin == "body" {
                "body"
            } else {
                "comment"
            }
        );
    }
    if !detail.mentions.is_empty() {
        let _ = writeln!(out, "mentions: {}", detail.mentions.join(" "));
    }
    if !detail.body.trim().is_empty() {
        let _ = writeln!(out, "\n{}", detail.body.trim_end());
    }
    if !detail.notes.is_empty() {
        let _ = writeln!(out, "\ndiscussion:");
        for note in &detail.notes {
            let _ = writeln!(
                out,
                "  {} {} ({}): {}",
                note.at,
                note.by,
                note.kind,
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
    let mut out = String::new();
    for source in &view.sources {
        let _ = writeln!(
            out,
            "#{} \"{}\" ({})",
            short(&source.item),
            source.title,
            if source.origin == "body" {
                "body"
            } else {
                "comment"
            }
        );
    }
    out
}

pub fn agents(agents: &[AgentView]) -> String {
    if agents.is_empty() {
        return "no agents defined\n".to_string();
    }
    let mut out = String::new();
    for agent in agents {
        let _ = writeln!(
            out,
            "{}  harness={}  model={}  capabilities=[{}]  lanes=[{}]",
            agent.id,
            agent.harness,
            agent.model.as_deref().unwrap_or("default"),
            agent.capabilities.join(", "),
            agent.lanes.join(", ")
        );
    }
    out
}

pub fn log(view: &LogView) -> String {
    let mut out = String::new();
    for event in &view.events {
        let _ = writeln!(
            out,
            "{}  {}#{:<3} {}  {:<16} {}  {}",
            short(&event.id),
            event.device,
            event.seq,
            event.at,
            event.kind,
            event.actor,
            event.summary
        );
    }
    let verified = view.events.iter().filter(|event| event.verified).count();
    let _ = writeln!(out, "{verified}/{} events verified", view.events.len());
    out
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

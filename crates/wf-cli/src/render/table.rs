use comfy_table::presets::NOTHING;
use comfy_table::{Attribute, Cell, ContentArrangement, Table};

use super::item::{item_label, steps_label};
use super::text::*;
use crate::devices::{DeviceView, MemberView};
use crate::theme;
use crate::view::*;
use wf_core::id::HashId;

/// The one place where every table's look is defined.
pub(super) fn table(header: &[&str]) -> Table {
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
                view.item.id,
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
                .map_or_else(|| paint(theme::text_dimmed(), "-"), crate::text::duration),
            view.item.title.clone(),
        ]);
    }
    format!("{table}\n")
}

pub fn mentions(view: &MentionsView) -> String {
    if view.sources.is_empty() {
        return format!("{} is not mentioned anywhere\n", view.target);
    }
    let mut table = table(&["ITEM", "TITLE", "IN"]);
    for source in &view.sources {
        table.add_row([
            format!("#{}", source.item.short()),
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
            match device.presence {
                DevicePresence::ThisDevice => format!("{name} (this)"),
                DevicePresence::Gone { .. } => format!("{name} (gone)"),
                DevicePresence::Unknown | DevicePresence::There { .. } => name,
            },
            device.principal.clone(),
            device.hardware.clone().unwrap_or_else(|| "-".to_string()),
            device
                .max_sessions
                .map_or_else(|| "-".to_string(), |sessions| sessions.to_string()),
            match device.presence {
                DevicePresence::ThisDevice => "now".to_string(),
                DevicePresence::Unknown => "-".to_string(),
                DevicePresence::There { last_seen } | DevicePresence::Gone { last_seen } => {
                    last_seen.map_or_else(|| "-".to_string(), short_time)
                }
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
            member.role.name().to_string(),
            member.devices.join(", "),
        ]);
    }
    format!("{table}\n")
}

pub fn log(events: &[EventView]) -> String {
    let mut table = table(&["EVENT", "DEVICE", "SEQ", "AT", "KIND", "ACTOR", "SUMMARY"]);
    for event in events {
        table.add_row([
            event.id.to_string(),
            event.device.to_string(),
            event.seq.to_string(),
            local_time(event.at, "%Y-%m-%d %H:%M:%S"),
            event.kind.to_string(),
            event.actor.to_string(),
            event.summary.to_string(),
        ]);
    }
    let verified = events.iter().filter(|event| event.verified).count();
    format!("{table}\n{verified}/{} events verified\n", events.len())
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
            session.id.short().to_string(),
            session.agent.clone(),
            session.model.clone().unwrap_or_default(),
            format!("#{}", session.item.short()),
            session.title.clone(),
            steps_label(session.steps),
            session.role.name().to_string(),
            session.status.name().to_string(),
            short_time(session.started),
        ]);
    }
    format!("{table}\n")
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
                .map_or_else(String::new, |item| format!("#{}", item.short())),
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
}

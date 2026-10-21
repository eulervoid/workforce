use std::fmt::Write;

use super::activity::activity;
use super::text::*;
use crate::theme;
use crate::transcript::Format;
use crate::view::*;
use wf_core::id::HashId;

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
            counts.push(format!("median {}", crate::text::duration(median)));
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
pub(super) fn agent_load(detail: &AgentDetail) -> String {
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
pub(super) fn agent_session(
    session: &SessionView,
    now: jiff::Timestamp,
    separator: &str,
) -> String {
    let age = |timestamp: jiff::Timestamp| {
        let millis = now
            .as_millisecond()
            .saturating_sub(timestamp.as_millisecond());
        crate::text::duration(u64::try_from(millis).unwrap_or(0))
    };
    let role = session.role.name();
    let (icon, state) = match session.status {
        SessionStatus::Live => (
            paint(theme::live(), "◆"),
            format!("{role} {}", age(session.started)),
        ),
        SessionStatus::Parked => (
            paint(theme::live(), "◇"),
            format!("{role} parked {}", age(session.started)),
        ),
        SessionStatus::Ended { reason, at } => (
            paint(theme::text_dimmed(), "·"),
            format!("{} {} ago", reason.label(), age(at)),
        ),
    };
    let label = paint_actor(&ActorView {
        kind: ActorKind::Agent,
        label: format!("{}/{}", session.agent, session.id.short()),
    });
    format!(
        "  {icon} {} {}{separator}{}{separator}{label}",
        id_label(session.item),
        session.title,
        paint(theme::text_dimmed(), state),
    )
}

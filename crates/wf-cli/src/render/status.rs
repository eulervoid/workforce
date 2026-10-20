use std::fmt::Write;

use super::text::*;
use crate::view::*;
use wf_core::id::HashId;

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
            (Some(seen), false) => format!("{} (seen {})", device.name, short_time(*seen)),
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
    for line in &view.awaiting_commits {
        let _ = writeln!(out, "waiting: {line}");
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

/// `a1b2c3 coder on #7ph6ds (live)  Title`. Pickers take plain text, so nothing is colored.
pub fn session_choice(session: &SessionView) -> String {
    format!(
        "{} {} on #{} ({})  {}",
        session.id.short(),
        session.agent,
        session.item.short(),
        session.status.name(),
        session.title
    )
}

pub fn session(session: &SessionView) -> String {
    let mut out = format!(
        "session {} of {} on #{} {}\n",
        session.id.short(),
        session.agent,
        session.item.short(),
        session.title
    );
    let _ = writeln!(out, "  role     {}", session.role.name());
    if let Some(model) = &session.model {
        let _ = writeln!(out, "  model    {model}");
    }
    let _ = writeln!(out, "  status   {}", session.status.name());
    let _ = writeln!(out, "  started  {}", short_time(session.started));
    if let SessionStatus::Ended { at, .. } = session.status {
        let _ = writeln!(out, "  ended    {}", short_time(at));
    }
    if let Some(commit) = &session.commit {
        let _ = writeln!(out, "  commit   {commit}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
            awaiting_commits: Vec::new(),
        };
        let output = status(&view);
        let lines: Vec<&str> = output.lines().collect();
        assert!(lines.contains(&"items: 6 ready, 3 archived"), "{output}");
        assert!(lines.contains(&"waiting on you: 2"), "{output}");
        let items_line = lines
            .iter()
            .find(|line| line.starts_with("items:"))
            .unwrap();
        assert!(!items_line.contains("waiting"), "{output}");
    }
}

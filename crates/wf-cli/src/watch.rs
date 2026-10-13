//! `wf watch`: the dashboard at a terminal; otherwise the events of the workforce as they
//! arrive, one line each.

use std::time::Duration;

use anyhow::bail;
use wf_core::event::Event;

use crate::cli::WatchArgs;
use crate::commands::emit_line;
use crate::interactive::Mode;
use crate::workspace::Workspace;
use crate::{render, view};

/// How often the daemon is asked for what is new.
const POLL_EVERY: Duration = Duration::from_millis(500);

/// How many of the latest events are shown when watching starts.
const BACKLOG: usize = 10;

pub fn watch(
    json: bool,
    mode: Mode,
    workspace: &mut Workspace,
    args: WatchArgs,
) -> anyhow::Result<()> {
    if !workspace.follows_daemon() {
        bail!("`wf watch` follows the daemon; start it with `wf daemon start`");
    }
    let only = args
        .item
        .as_deref()
        .map(|text| workspace.resolve_item(text))
        .transpose()?;
    if mode == Mode::Interactive {
        return crate::dashboard::run(workspace, only);
    }
    let mut events = workspace.events()?;
    let mut backlog: Vec<Event> = events
        .iter()
        .rev()
        .filter(|event| only.is_none() || workspace.state().item_of(event) == only)
        .take(BACKLOG)
        .cloned()
        .collect();
    backlog.reverse();
    show(json, workspace, &events, &backlog)?;
    loop {
        std::thread::sleep(POLL_EVERY);
        workspace.refresh()?;
        let new = workspace.events_since(events.len() as u64)?;
        if new.is_empty() {
            continue;
        }
        events.extend(new.iter().cloned());
        let shown: Vec<Event> = new
            .into_iter()
            .filter(|event| only.is_none() || workspace.state().item_of(event) == only)
            .collect();
        show(json, workspace, &events, &shown)?;
    }
}

fn show(json: bool, workspace: &Workspace, events: &[Event], new: &[Event]) -> anyhow::Result<()> {
    for line in view::watch_lines(workspace.state(), events, new) {
        emit_line(json, &line, render::watch_line)?;
    }
    Ok(())
}

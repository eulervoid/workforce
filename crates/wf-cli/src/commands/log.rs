//! `wf log`: the events of the workforce, and with `--follow` those that arrive.

use std::time::Duration;

use anyhow::bail;
use wf_core::event::Event;
use wf_core::id::ItemId;
use wf_core::state::{State, Trust};

use super::{emit_line, emit_paged};
use crate::cli::LogArgs;
use crate::interactive::Mode;
use crate::render;
use crate::view;
use crate::workspace::Workspace;

/// How often the daemon is asked for what is new.
const POLL_EVERY: Duration = Duration::from_millis(500);

pub(super) fn log(
    json: bool,
    mode: Mode,
    workspace: &mut Workspace,
    args: LogArgs,
) -> anyhow::Result<()> {
    let only = args
        .item
        .as_deref()
        .map(|text| workspace.resolve_item(text))
        .transpose()?;
    if args.follow {
        return follow(json, workspace, only);
    }
    let state = workspace.state();
    // Replay in the order the events were applied into an empty state with full verification:
    // signatures, feeds and authority. A fork takes in its history newest event first, so this
    // is not the order they are shown in.
    let mut replay = match state.workforce_id() {
        Some(id) => State::expecting(id),
        None => State::new(),
    };
    let events = workspace.events()?;
    let comments = view::comment_texts(&events);
    let mut views = Vec::new();
    for event in events {
        let verified = event.verify_signature().is_ok()
            && replay.receive(event.clone(), Trust::Untrusted).is_ok();
        if !concerns(state, &event, only) {
            continue;
        }
        views.push((
            (event.content.hlc, event.id()),
            view::event_view(state, &event, verified, &comments),
        ));
    }
    if replay.pending_events().next().is_some() || replay.history_incomplete() {
        bail!("the log is incomplete: some events are missing their parents");
    }
    views.sort_by_key(|(order, _)| *order);
    let views: Vec<_> = views.into_iter().map(|(_, view)| view).collect();
    emit_paged(json, mode, &views, |views| render::log(views))
}

/// Prints the whole log, then the events as they arrive, until interrupted.
fn follow(json: bool, workspace: &mut Workspace, only: Option<ItemId>) -> anyhow::Result<()> {
    if !workspace.follows_daemon() {
        bail!("`wf log --follow` follows the daemon; start it with `wf daemon start`");
    }
    let mut events = workspace.events()?;
    let shown: Vec<Event> = events
        .iter()
        .filter(|event| concerns(workspace.state(), event, only))
        .cloned()
        .collect();
    show(json, workspace, &events, &shown)?;
    loop {
        std::thread::sleep(POLL_EVERY);
        let new = workspace.events_since(events.len() as u64)?;
        if new.is_empty() {
            continue;
        }
        workspace.refresh()?;
        events.extend(new.iter().cloned());
        let shown: Vec<Event> = new
            .into_iter()
            .filter(|event| concerns(workspace.state(), event, only))
            .collect();
        show(json, workspace, &events, &shown)?;
    }
}

/// Whether `event` is shown: every event without `--item`, otherwise those of the item.
fn concerns(state: &State, event: &Event, only: Option<ItemId>) -> bool {
    only.is_none() || state.item_of(event) == only
}

fn show(json: bool, workspace: &Workspace, events: &[Event], new: &[Event]) -> anyhow::Result<()> {
    for line in view::event_lines(workspace.state(), &|_| events, new) {
        emit_line(json, &line, render::event_line)?;
    }
    Ok(())
}

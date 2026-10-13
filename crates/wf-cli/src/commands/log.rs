//! `wf log`: the events of the workforce.

use anyhow::bail;
use wf_core::state::{State, Trust};

use super::emit_paged;
use crate::interactive::Mode;
use crate::render;
use crate::view::{self, LogView};
use crate::workspace::Workspace;

pub(super) fn log(json: bool, mode: Mode, workspace: &Workspace) -> anyhow::Result<()> {
    let state = workspace.state();
    // Replay in the order the events were applied into an empty state with full verification:
    // signatures, feeds and authority. A fork takes in its history newest event first, so this
    // is not the order they are shown in.
    let mut replay = match state.workforce_id() {
        Some(id) => State::expecting(id),
        None => State::new(),
    };
    let mut views = Vec::new();
    for event in workspace.events()? {
        let verified = event.verify_signature().is_ok()
            && replay.receive(event.clone(), Trust::Untrusted).is_ok();
        views.push((
            (event.content.hlc, event.id()),
            view::event_view(state, &event, verified),
        ));
    }
    if replay.pending_events().next().is_some() || replay.history_incomplete() {
        bail!("the log is incomplete: some events are missing their parents");
    }
    views.sort_by_key(|(order, _)| *order);
    let views = views.into_iter().map(|(_, view)| view).collect();
    let log = LogView {
        workforce: state
            .workforce_id()
            .map(|id| id.to_string())
            .unwrap_or_default(),
        events: views,
    };
    emit_paged(json, mode, &log, render::log)
}

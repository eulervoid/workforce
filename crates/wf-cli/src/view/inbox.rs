//! `wf inbox`: what waits on the principal.

use serde::Serialize;
use wf_core::id::HashId;
use wf_core::inbox::{InboxKind, permission_waits};
use wf_core::state::State;
use wf_daemon::proto::HeldPermission;

#[derive(Serialize)]
pub struct InboxEntryView {
    pub kind: InboxKind,
    pub item: Option<String>,
    pub session: Option<String>,
    pub title: String,
    pub what: String,
    /// Permission requests only: why each decider of the machine passed it on, as `decider: why`.
    pub passed: Vec<String>,
}

/// What waits on the principal: what the state says, and the permission requests the daemon holds.
pub fn inbox_views(state: &State, held: &[HeldPermission]) -> Vec<InboxEntryView> {
    let mut entries: Vec<InboxEntryView> = state
        .inbox()
        .into_iter()
        .map(|entry| InboxEntryView {
            kind: entry.waiting.kind(),
            item: Some(entry.item.to_base32()),
            session: None,
            title: state.inbox_title(entry.item),
            what: state.what_waits(&entry),
            passed: Vec::new(),
        })
        .collect();
    entries.extend(held.iter().map(|request| {
        let session = state.session(request.session);
        InboxEntryView {
            kind: InboxKind::Permission,
            item: session.map(|session| session.item().to_base32()),
            session: Some(request.session.to_base32()),
            title: session.map_or_else(String::new, |session| state.inbox_title(session.item())),
            what: permission_waits(
                session.map(|session| session.agent().name.as_str()),
                &request.what,
                request.session,
            ),
            passed: request.passed.clone(),
        }
    }));
    entries
}

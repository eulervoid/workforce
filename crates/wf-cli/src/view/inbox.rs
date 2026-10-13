//! `wf inbox`: what waits on the principal.

use serde::Serialize;
use wf_core::id::{HashId, ItemId, SessionId};
use wf_core::inbox::{InboxEntry, Waiting};
use wf_core::item::{ItemView, Standing};
use wf_core::session::SessionRole;
use wf_core::state::State;
use wf_daemon::proto::HeldPermission;

#[derive(Serialize)]
pub struct InboxEntryView {
    /// `blocker`, `proposal`, `principal_review`, `merge`, `recovery` or `permission`.
    pub kind: String,
    pub item: Option<String>,
    pub session: Option<String>,
    pub title: String,
    pub what: String,
    /// Permission requests only: why each decider of the machine passed it on, as `decider: why`.
    pub passed: Vec<String>,
}

/// What a recovery step that waits on the principal says: who ended last and how, and what to do.
pub fn recovery_waits(
    state: &State,
    item: ItemId,
    role: SessionRole,
    failures: usize,
    last: SessionId,
) -> String {
    let sessions = match (role, failures) {
        (SessionRole::Work, 1) => "a session".to_string(),
        (SessionRole::Work, _) => format!("{failures} sessions"),
        (SessionRole::Review { .. }, 1) => "a review session".to_string(),
        (SessionRole::Review { .. }, _) => format!("{failures} review sessions"),
        (SessionRole::Supervise, 1) => "a supervise session".to_string(),
        (SessionRole::Supervise, _) => format!("{failures} supervise sessions"),
        (SessionRole::Diagnose, 1) => "a diagnose session".to_string(),
        (SessionRole::Diagnose, _) => format!("{failures} diagnose sessions"),
    };
    let ended = state
        .session(last)
        .and_then(|session| {
            let reason = session.ended()?.reason.name().replace('_', " ");
            Some(format!("@{} {reason}", session.agent().name))
        })
        .unwrap_or_else(|| "the last one ended".to_string());
    let next = match role {
        SessionRole::Work | SessionRole::Diagnose => format!(
            "comment with what to do differently, or `wf session start {}`",
            item.short()
        ),
        SessionRole::Review { .. } => {
            "review it yourself, or comment to have it tried again".into()
        }
        SessionRole::Supervise => "answer the blocker, or comment to have it tried again".into(),
    };
    format!("{sessions} did not finish (last: {ended}); {next}")
}

/// The kind of what waits, as `wf inbox --json` names it, and what it says to the principal.
pub fn what_waits(state: &State, entry: &InboxEntry) -> (&'static str, String) {
    let policy = &state.policy().expect("a workforce has a policy").policy;
    match entry.waiting {
        Waiting::Blocker { comment } => (
            "blocker",
            format!(
                "blocker {}: answer with `wf comment {} \"...\" --resolve {}`",
                comment.short(),
                entry.item.short(),
                comment.short()
            ),
        ),
        Waiting::Proposal => {
            let state_name = match state.item(entry.item) {
                Some(ItemView::Live(live)) => match live.standing(policy) {
                    Standing::Known { state, .. } => state.to_string(),
                    Standing::Unknown { state } => {
                        state.map_or_else(|| "an unknown state".to_string(), ToString::to_string)
                    }
                },
                Some(ItemView::Archived(_)) | None => "an unknown state".to_string(),
            };
            ("proposal", format!("{state_name}; `wf item move` it on"))
        }
        Waiting::PrincipalReview { commit } => (
            "principal_review",
            match commit {
                Some(commit) => format!(
                    "review of {}: `wf item diff`, then `wf review`",
                    &commit.to_string()[..8]
                ),
                None => "review of work without commits: read it, then `wf review`".to_string(),
            },
        ),
        Waiting::Merge { commit } => (
            "merge",
            format!(
                "{} is ready to merge, but {} is checked out with changes",
                &commit.to_string()[..8],
                policy.merge.target
            ),
        ),
        Waiting::Recovery {
            role,
            failures,
            last,
        } => (
            "recovery",
            recovery_waits(state, entry.item, role, failures, last),
        ),
    }
}

/// What waits on the principal: what the state says, and the permission requests the daemon holds.
pub fn inbox_views(state: &State, held: &[HeldPermission]) -> Vec<InboxEntryView> {
    let title_of = |item: ItemId| match state.item(item) {
        Some(ItemView::Live(live)) => live.title().to_string(),
        Some(ItemView::Archived(_)) | None => "(archived)".to_string(),
    };
    let mut entries: Vec<InboxEntryView> = state
        .inbox()
        .into_iter()
        .map(|entry| {
            let (kind, what) = what_waits(state, &entry);
            InboxEntryView {
                kind: kind.to_string(),
                item: Some(entry.item.to_base32()),
                session: None,
                title: title_of(entry.item),
                what,
                passed: Vec::new(),
            }
        })
        .collect();
    entries.extend(held.iter().map(|request| {
        let session = state.session(request.session);
        InboxEntryView {
            kind: "permission".to_string(),
            item: session.map(|session| session.item().to_base32()),
            session: Some(request.session.to_base32()),
            title: session.map_or_else(String::new, |session| title_of(session.item())),
            what: format!(
                "{} asks to {}; `wf session allow` or `deny {}`",
                session.map_or_else(
                    || "an agent".to_string(),
                    |session| session.agent().name.to_string()
                ),
                request.what,
                request.session.short()
            ),
            passed: request.passed.clone(),
        }
    }));
    entries
}

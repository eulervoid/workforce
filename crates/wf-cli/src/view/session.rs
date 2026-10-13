//! Sessions and agents, as `wf session list` and `wf agent list` show them.

use serde::Serialize;
use wf_core::id::HashId;
use wf_core::item::ItemView;
use wf_core::resolve::MentionTarget;
use wf_core::session::{EndReason, Session, SessionRole};
use wf_core::state::State;

use super::item::{StepsView, steps_view};
use super::{mention_label, timestamp};

pub(super) fn session_label(session: &Session) -> String {
    format!(
        "{} {} {}",
        session.agent().name,
        role_name(session.role()),
        session.id().short()
    )
}

fn role_name(role: SessionRole) -> &'static str {
    match role {
        SessionRole::Work => "work",
        SessionRole::Review { .. } => "review",
        SessionRole::Supervise => "supervise",
        SessionRole::Diagnose => "diagnose",
    }
}

#[derive(Serialize)]
pub struct AgentView {
    pub id: String,
    pub harness: String,
    pub model: Option<String>,
    pub capabilities: Vec<String>,
    pub lanes: Vec<String>,
    pub start: String,
    pub permissions: String,
    pub nudges: u32,
    pub park_timeout: String,
    pub max_duration: Option<String>,
    pub instructions: Option<String>,
}

pub fn agent_views(state: &State) -> Vec<AgentView> {
    state
        .agents()
        .map(|(id, definition)| AgentView {
            id: mention_label(state, &MentionTarget::Agent(id.clone())),
            harness: definition.harness.to_string(),
            model: definition.model.clone(),
            capabilities: definition
                .capabilities
                .iter()
                .map(ToString::to_string)
                .collect(),
            lanes: definition.lanes.iter().map(ToString::to_string).collect(),
            start: definition.autonomy.start.to_string(),
            permissions: definition.autonomy.permissions.to_string(),
            nudges: definition.autonomy.nudges,
            park_timeout: humantime::format_duration(definition.autonomy.park_timeout).to_string(),
            max_duration: definition
                .autonomy
                .max_duration
                .map(|duration| humantime::format_duration(duration).to_string()),
            instructions: definition.instructions.clone(),
        })
        .collect()
}

#[derive(Serialize)]
pub struct SessionView {
    pub id: String,
    pub agent: String,
    pub item: String,
    pub title: String,
    /// `work` or `review`.
    pub role: String,
    /// `live`, `parked` while it waits for an answer, or why it ended.
    pub status: String,
    pub started: String,
    pub ended: Option<String>,
    /// What the session submitted, or the commit a review session reviews.
    pub commit: Option<String>,
    /// How many steps of the item are checked off.
    pub steps: Option<StepsView>,
}

/// Whether `session` is a live work session whose item waits on a dependency or on an open
/// blocker, such as its own question.
pub(super) fn is_parked(
    state: &State,
    graph: &wf_core::graph::DependencyGraph,
    session: &wf_core::session::Session,
) -> bool {
    let blocked = match state.item(session.item()) {
        Some(ItemView::Live(item)) => item.has_open_blocker(),
        Some(ItemView::Archived(_)) | None => false,
    };
    session.is_live()
        && session.role() == SessionRole::Work
        && (blocked
            || !state
                .unfinished_dependencies(graph, session.item())
                .is_empty())
}

/// Live sessions first, then the others, each the newest first.
pub fn session_views(state: &State) -> Vec<SessionView> {
    let graph = state.dependency_graph();
    let mut sessions: Vec<_> = state.sessions().collect();
    sessions.sort_by_key(|session| (!session.is_live(), std::cmp::Reverse(session.started())));
    sessions
        .into_iter()
        .map(|session| {
            let title = match state.item(session.item()) {
                Some(ItemView::Live(item)) => item.title().to_string(),
                Some(ItemView::Archived(_)) | None => "(archived)".to_string(),
            };
            let commit = match (session.role(), session.ended().map(|ended| ended.reason)) {
                (SessionRole::Review { commit }, _) => commit.map(|commit| commit.to_string()),
                (SessionRole::Work, Some(EndReason::Submitted { commit })) => {
                    commit.map(|commit| commit.to_string())
                }
                (SessionRole::Work | SessionRole::Supervise | SessionRole::Diagnose, _) => None,
            };
            let steps = match state.item(session.item()) {
                Some(ItemView::Live(item)) => steps_view(&item.body()),
                Some(ItemView::Archived(_)) | None => None,
            };
            let parked = is_parked(state, &graph, session);
            SessionView {
                id: session.id().to_base32(),
                agent: mention_label(state, &MentionTarget::Agent(session.agent().clone())),
                item: session.item().to_base32(),
                title,
                role: role_name(session.role()).to_string(),
                status: match session.ended() {
                    Some(ended) => ended.reason.name().to_string(),
                    None if parked => "parked".to_string(),
                    None => "live".to_string(),
                },
                started: timestamp(session.started().hlc),
                ended: session.ended().map(|ended| timestamp(ended.at())),
                commit,
                steps,
            }
        })
        .collect()
}

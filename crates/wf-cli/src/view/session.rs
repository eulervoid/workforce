//! Sessions and agents, as `wf session list` and `wf agent list` show them.

use serde::Serialize;
use wf_core::graph::DependencyGraph;
use wf_core::id::{HashId, SessionId};
use wf_core::identity::AgentId;
use wf_core::item::ItemView;
use wf_core::resolve::MentionTarget;
use wf_core::session::{EndReason, Session, SessionRole};
use wf_core::state::{AgentDefinition, State};

use super::item::{StepsView, steps_view};
use super::{mention_label, timestamp};

pub(super) fn session_label(session: &Session) -> String {
    format!(
        "{} {} {}",
        session.agent().name,
        session.role().name(),
        session.id().short()
    )
}

#[derive(Serialize)]
pub struct AgentView {
    pub id: String,
    pub harness: String,
    /// This machine can start the harness; another member's machine may still.
    pub harness_startable_here: bool,
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
        .map(|(id, definition)| agent_view(state, id, definition))
        .collect()
}

pub(super) fn agent_view(state: &State, id: &AgentId, definition: &AgentDefinition) -> AgentView {
    AgentView {
        id: mention_label(state, &MentionTarget::Agent(id.clone())),
        harness: definition.harness.to_string(),
        harness_startable_here: crate::harness::is_startable(definition.harness.as_str()),
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
    }
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
    /// The model the session runs with, as the agent reported it; `None` when that is not known
    /// on this machine.
    pub model: Option<String>,
    /// What the session submitted, or the commit a review session reviews.
    pub commit: Option<String>,
    /// How many steps of the item are checked off.
    pub steps: Option<StepsView>,
}

/// Live sessions first, then the others, each the newest first. `model_of` tells the model a
/// session runs with, where this machine knows it.
pub fn session_views(
    state: &State,
    model_of: impl Fn(SessionId) -> Option<String>,
) -> Vec<SessionView> {
    let graph = state.dependency_graph();
    let mut sessions: Vec<_> = state.sessions().collect();
    sessions.sort_by_key(|session| (!session.is_live(), std::cmp::Reverse(session.started())));
    sessions
        .into_iter()
        .map(|session| session_view(state, &graph, session, model_of(session.id())))
        .collect()
}

/// One session, as `wf session list` shows it, running with `model` where that is known.
pub(super) fn session_view(
    state: &State,
    graph: &DependencyGraph,
    session: &Session,
    model: Option<String>,
) -> SessionView {
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
    let parked = state.session_waits(session.id(), graph);
    SessionView {
        id: session.id().to_base32(),
        agent: mention_label(state, &MentionTarget::Agent(session.agent().clone())),
        item: session.item().to_base32(),
        title,
        role: session.role().name().to_string(),
        status: match session.ended() {
            Some(ended) => ended.reason.name().to_string(),
            None if parked => "parked".to_string(),
            None => "live".to_string(),
        },
        started: timestamp(session.started().hlc),
        ended: session.ended().map(|ended| timestamp(ended.at())),
        model,
        commit,
        steps,
    }
}

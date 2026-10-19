//! Sessions and agents, as `wf session list` and `wf agent list` show them.

use jiff::Timestamp;
use serde::{Serialize, Serializer};
use wf_core::graph::DependencyGraph;
use wf_core::id::{HashId, ItemId, SessionId};
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
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

pub fn agent_view(state: &State, id: &AgentId, definition: &AgentDefinition) -> AgentView {
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

/// Where a session is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionStatus {
    Live,
    /// Live, but its item waits on a dependency or an open blocker.
    Parked,
    Ended {
        reason: EndReason,
        at: Timestamp,
    },
}

impl SessionStatus {
    pub fn is_live(&self) -> bool {
        match self {
            SessionStatus::Live | SessionStatus::Parked => true,
            SessionStatus::Ended { .. } => false,
        }
    }

    /// `live`, `parked` or why the session ended.
    pub fn name(&self) -> &'static str {
        match self {
            SessionStatus::Live => "live",
            SessionStatus::Parked => "parked",
            SessionStatus::Ended { reason, .. } => reason.name(),
        }
    }
}

fn serialize_role_name<S: Serializer>(
    role: &SessionRole,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(role.name())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionView {
    pub id: SessionId,
    pub agent: String,
    pub item: ItemId,
    pub title: String,
    /// Written as its name: `work`, `review`, `supervise` or `diagnose`.
    pub role: SessionRole,
    /// Written as `status`, and as `ended` after `started`.
    pub status: SessionStatus,
    pub started: Timestamp,
    /// The model the session runs with, as the agent reported it; `None` when that is not known
    /// on this machine.
    pub model: Option<String>,
    /// What the session submitted, or the commit a review session reviews.
    pub commit: Option<String>,
    /// How many steps of the item are checked off.
    pub steps: Option<StepsView>,
}

impl Serialize for SessionView {
    /// `status` is `live`, `parked` or why the session ended; `ended` says when, and is null
    /// while the session lives.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Wire<'a> {
            id: SessionId,
            agent: &'a str,
            item: ItemId,
            title: &'a str,
            #[serde(serialize_with = "serialize_role_name")]
            role: SessionRole,
            status: &'static str,
            started: Timestamp,
            ended: Option<Timestamp>,
            model: &'a Option<String>,
            commit: &'a Option<String>,
            steps: &'a Option<StepsView>,
        }
        let ended = match self.status {
            SessionStatus::Live | SessionStatus::Parked => None,
            SessionStatus::Ended { at, .. } => Some(at),
        };
        Wire {
            id: self.id,
            agent: &self.agent,
            item: self.item,
            title: &self.title,
            role: self.role,
            status: self.status.name(),
            started: self.started,
            ended,
            model: &self.model,
            commit: &self.commit,
            steps: &self.steps,
        }
        .serialize(serializer)
    }
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
pub fn session_view(
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
        Some(ItemView::Live(item)) => steps_view(item),
        Some(ItemView::Archived(_)) | None => None,
    };
    SessionView {
        id: session.id(),
        agent: mention_label(state, &MentionTarget::Agent(session.agent().clone())),
        item: session.item(),
        title,
        role: session.role(),
        status: session_status(state, graph, session),
        started: timestamp(session.started().hlc),
        model,
        commit,
        steps,
    }
}

pub(super) fn session_status(
    state: &State,
    graph: &DependencyGraph,
    session: &Session,
) -> SessionStatus {
    match session.ended() {
        Some(ended) => SessionStatus::Ended {
            reason: ended.reason,
            at: timestamp(ended.at()),
        },
        None if state.session_waits(session.id(), graph) => SessionStatus::Parked,
        None => SessionStatus::Live,
    }
}

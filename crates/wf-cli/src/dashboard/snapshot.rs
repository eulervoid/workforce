//! What the dashboard shows at one refresh, taken from the workspace.

use wf_core::graph::Readiness;
use wf_core::policy::StateCategory;
use wf_core::state::State;

/// Everything the dashboard draws, as plain data: the loop takes one at each refresh, and tests
/// build them by hand.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub name: String,
    pub counts: Counts,
    /// What waits on the principal: `wf inbox`.
    pub waiting: usize,
    /// The latest event, as `wf watch` prints it.
    pub latest: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub live_sessions: usize,
    pub open_items: usize,
    pub blocked_items: usize,
    pub devices: usize,
}

impl Counts {
    pub fn of(state: &State) -> Self {
        let policy = &state.policy().expect("a workforce has a policy").policy;
        let graph = state.dependency_graph();
        let mut counts = Counts {
            live_sessions: state.live_sessions().count(),
            devices: state.devices().count(),
            ..Counts::default()
        };
        for item in state.live_items() {
            if item.standing(policy).state_category() == Some(StateCategory::Open) {
                counts.open_items += 1;
            }
            if state.readiness(&graph, item) == Some(Readiness::Blocked) {
                counts.blocked_items += 1;
            }
        }
        counts
    }
}

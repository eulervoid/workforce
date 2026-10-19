//! Which agent takes which item: the selection the dispatcher makes, and `wf item list --ready
//! --for` shows. Pure functions of the state and the device the agent runs on.

use std::cmp::Reverse;
use std::collections::BTreeSet;

use crate::autonomy::StartMode;
use crate::crdt::Lww;
use crate::event::DeviceProfile;
use crate::identity::{AgentId, DeviceKey};
use crate::item::Item;
use crate::name::Requirement;
use crate::review::ReviewStatus;
use crate::session::SessionRole;
use crate::state::{AgentDefinition, State};

/// How far the work on an item got, in the order the dispatcher takes items of one priority:
/// started work is finished before new work starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Progress {
    /// The latest review requested changes and change requests are still open.
    SentBack,
    /// Work sessions ran on it, and no review sent it back: released, or moved back to open
    /// after a merge conflict.
    Started,
    NeverStarted,
}

impl State {
    /// What the device says about itself, if it has said anything.
    pub fn device_profile(&self, device: DeviceKey) -> Option<&DeviceProfile> {
        let workforce = self.workforce.as_ref()?;
        if !workforce.devices.contains_key(&device) {
            return None;
        }
        workforce.profiles.get(&device).map(Lww::value)
    }

    /// The agents of the principal of `device` that the device hosts and that the dispatcher
    /// starts by itself.
    pub fn auto_agents(&self, device: DeviceKey) -> Vec<(AgentId, &AgentDefinition)> {
        let (Some(principal), Some(profile)) =
            (self.principal_of(&device), self.device_profile(device))
        else {
            return Vec::new();
        };
        profile
            .agents
            .keys()
            .filter_map(|name| {
                let definition = self.agent(name, principal)?;
                (definition.autonomy.start == StartMode::Auto).then(|| {
                    (
                        AgentId {
                            principal,
                            name: name.clone(),
                        },
                        definition,
                    )
                })
            })
            .collect()
    }

    /// Whether the device and the agent can take another session. Parked sessions count.
    pub fn has_free_slot(&self, device: DeviceKey, agent: &AgentId) -> bool {
        let Some(profile) = self.device_profile(device) else {
            return false;
        };
        let Some(agent_limit) = profile.agents.get(&agent.name) else {
            return false;
        };
        let live =
            |session: &&crate::session::Session| session.is_live() && session.device() == device;
        let on_device = self.sessions().filter(live).count();
        let of_agent = self
            .sessions()
            .filter(live)
            .filter(|session| session.agent() == agent)
            .count();
        on_device < profile.max_sessions as usize && of_agent < *agent_limit as usize
    }

    /// What `agent` brings to an item on `device`: its capabilities and the hardware of the device.
    pub fn offered(&self, device: DeviceKey, agent: &AgentDefinition) -> BTreeSet<Requirement> {
        let mut offered: BTreeSet<Requirement> = agent.capabilities.iter().cloned().collect();
        if let Some(profile) = self.device_profile(device) {
            offered.extend(profile.hardware_requirements());
        }
        offered
    }

    /// How far the work on `item` got.
    pub fn progress(&self, item: &Item) -> Progress {
        let changes_open = item
            .comments()
            .values()
            .any(|comment| comment.kind.is_change_request() && comment.is_open());
        if changes_open && self.review_status(item.id()) == Some(ReviewStatus::ChangesRequested) {
            return Progress::SentBack;
        }
        let worked_on = self
            .sessions_of(item.id())
            .any(|session| session.role() == SessionRole::Work);
        if worked_on {
            Progress::Started
        } else {
            Progress::NeverStarted
        }
    }

    /// The order the dispatcher takes ready items in: priority (`P0` first, none last), then
    /// [`Progress`], then fewer unchecked steps, then, for started work, more checked steps, so
    /// what finishes soonest goes first; then creation time and id.
    pub fn dispatch_order(&self, item: &Item) -> impl Ord + use<> {
        let progress = self.progress(item);
        let checked = item.steps().filter(|step| step.done).count();
        let unchecked = item.steps().count() - checked;
        let more_checked = match progress {
            Progress::Started => checked,
            Progress::SentBack | Progress::NeverStarted => 0,
        };
        (
            item.priority().is_none(),
            item.priority(),
            progress,
            unchecked,
            Reverse(more_checked),
            item.created(),
            item.id(),
        )
    }

    /// The ready items `agent` may take on `device`: in a lane of the agent, with every
    /// requirement met by its capabilities or the hardware of the device, and not waiting for the
    /// principal after sessions that did not finish. They come in [`State::dispatch_order`]: most
    /// urgent first, and within a priority work sent back by review, then other started work,
    /// then new work, each with the fewest unchecked steps first.
    pub fn dispatchable(&self, device: DeviceKey, agent: &AgentId) -> Vec<&Item> {
        let Some(definition) = self.agent(&agent.name, agent.principal) else {
            return Vec::new();
        };
        let Some(policy) = self.policy().map(|document| &document.policy) else {
            return Vec::new();
        };
        let offered = self.offered(device, definition);

        let mut dispatchable: Vec<&Item> = self
            .ready_items()
            .into_iter()
            .filter(|item| {
                item.effective_lane(policy)
                    .is_some_and(|lane| definition.lanes.contains(lane))
            })
            .filter(|item| item.requires().values().all(|need| offered.contains(need)))
            .filter(|item| self.recovery(item.id(), SessionRole::Work).allows_attempt())
            .collect();
        dispatchable.sort_by_cached_key(|item| self.dispatch_order(item));
        dispatchable
    }
}

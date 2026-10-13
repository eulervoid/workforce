//! Which agent takes which item: the selection the dispatcher makes, and `wf item list --ready
//! --for` shows. Pure functions of the state and the device the agent runs on.

use std::collections::BTreeSet;

use crate::autonomy::StartMode;
use crate::event::DeviceProfile;
use crate::identity::{AgentId, DeviceKey};
use crate::item::Item;
use crate::name::Requirement;
use crate::session::SessionRole;
use crate::state::{AgentDefinition, State};

impl State {
    /// What the device says about itself, if it has said anything.
    pub fn device_profile(&self, device: DeviceKey) -> Option<&DeviceProfile> {
        self.devices()
            .find(|info| info.device == device)
            .and_then(|info| info.profile)
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

    /// The ready items `agent` may take on `device`, most urgent first: in a lane of the agent,
    /// with every requirement met by its capabilities or the hardware of the device, and not
    /// waiting for the principal after sessions that did not finish.
    pub fn dispatchable(&self, device: DeviceKey, agent: &AgentId) -> Vec<&Item> {
        let Some(definition) = self.agent(&agent.name, agent.principal) else {
            return Vec::new();
        };
        let Some(policy) = self.policy().map(|document| &document.policy) else {
            return Vec::new();
        };
        let offered = self.offered(device, definition);

        self.ready_items()
            .into_iter()
            .filter(|item| {
                item.effective_lane(policy)
                    .is_some_and(|lane| definition.lanes.contains(lane))
            })
            .filter(|item| item.requires().values().all(|need| offered.contains(need)))
            .filter(|item| self.recovery(item.id(), SessionRole::Work).allows_attempt())
            .collect()
    }
}

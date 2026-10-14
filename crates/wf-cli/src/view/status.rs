//! `wf status`: whether the workforce is working, and what is next.

use std::collections::BTreeSet;

use serde::Serialize;
use wf_core::autonomy::StartMode;
use wf_core::graph::Readiness;
use wf_core::id::HashId;
use wf_core::identity::Actor;
use wf_core::inbox::Waiting;
use wf_core::item::ItemView;
use wf_core::state::State;

use super::session::is_parked;
use super::{device_label, timestamp_ms};

#[derive(Serialize)]
pub struct StatusView {
    pub name: String,
    pub id: String,
    pub members: Vec<String>,
    pub agents: usize,
    pub events: usize,
    pub pending: usize,
    pub heads: usize,
    pub archived_items: usize,
    pub forked_from: Option<String>,
    pub devices: Vec<DeviceStatus>,
    /// Whether this command was answered by the daemon of this machine, which starts the agents.
    pub daemon: bool,
    pub ready_items: usize,
    pub live_sessions: usize,
    /// Live work sessions whose item waits on a dependency or an open blocker.
    pub parked_sessions: usize,
    /// Items whose sessions did not finish and whose recovery level leaves them to the principal.
    pub after_failures: Vec<String>,
    /// What waits on the principal, as `wf inbox` lists it.
    pub waiting_on_you: usize,
    pub flagged: Vec<String>,
    /// What to do next, if anything stands out.
    pub next: Option<String>,
    pub refolded: bool,
}

/// What `wf status` knows besides the state.
pub struct StatusContext<'a> {
    pub own: Option<wf_core::identity::DeviceKey>,
    pub last_seen: &'a std::collections::BTreeMap<wf_core::identity::DeviceKey, u64>,
    pub daemon: bool,
    pub waiting_on_you: usize,
    pub refolded: bool,
}

#[derive(Serialize)]
pub struct DeviceStatus {
    pub name: String,
    pub this_device: bool,
    /// Only a running daemon knows.
    pub last_seen: Option<String>,
}

pub fn status(state: &State, context: StatusContext) -> StatusView {
    let StatusContext {
        own,
        last_seen,
        daemon,
        waiting_on_you,
        refolded,
    } = context;
    let policy = &state.policy().expect("a workforce has a policy").policy;
    let graph = state.dependency_graph();
    let mut flagged = BTreeSet::new();
    let mut archived_items = 0;
    for view in state.items() {
        match view {
            ItemView::Archived(_) => archived_items += 1,
            ItemView::Live(item) => {
                let config = policy.type_config(item.item_type());
                if let Some(name) = item.unknown_state() {
                    flagged.insert(format!(
                        "#{} \"{}\" is in the unknown state `{name}`, so it counts as a draft",
                        item.id().short(),
                        item.title()
                    ));
                } else if config.is_none() {
                    flagged.insert(format!(
                        "#{} \"{}\" has the unknown type `{}`",
                        item.id().short(),
                        item.title(),
                        item.item_type()
                    ));
                } else if config.is_some_and(|config| !config.uses(item.state(policy))) {
                    flagged.insert(format!(
                        "#{} \"{}\" is `{}`, a state its type no longer uses",
                        item.id().short(),
                        item.title(),
                        item.state(policy)
                    ));
                } else if state.readiness(&graph, item) == Some(Readiness::Stranded) {
                    flagged.insert(format!(
                        "#{} \"{}\" is stranded: a dependency was cancelled",
                        item.id().short(),
                        item.title()
                    ));
                }
            }
        }
    }
    for item in state.unstaffed_reviews() {
        flagged.insert(format!(
            "the review of #{} waits for an agent step that no agent started on this workforce can take",
            item.short()
        ));
    }
    for (id, definition) in state.agents() {
        if definition.autonomy.start != StartMode::Auto {
            continue;
        }
        if definition.lanes.is_empty() {
            flagged.insert(format!(
                "agent {0} starts automatically but has no lane, so it never takes an item; `wf agent edit {0} --lane default`",
                id.name
            ));
        }
        let hosted = state.devices().any(|device| {
            device.principal == id.principal
                && device
                    .profile
                    .is_some_and(|profile| profile.agents.contains_key(&id.name))
        });
        if !hosted {
            flagged.insert(format!(
                "agent {0} starts automatically but no device hosts it; `wf device edit --agent {0}=1`",
                id.name
            ));
        }
    }
    if let Some(error) = state.unreadable_policy() {
        flagged.insert(format!(
            "this version of wf cannot read the policy set last ({}), so it runs on the last one it could read; update wf, or replace the policy with `wf policy edit`",
            wf_core::error_chain(&error)
        ));
    }
    for edge in graph.ignored() {
        flagged.insert(format!(
            "dependency #{} -> #{} ignored: concurrent links formed a cycle",
            edge.from.short(),
            edge.to.short()
        ));
    }
    for (device, seq) in state.equivocations() {
        flagged.insert(format!(
            "device {} wrote two different events with seq {seq}; it was cloned or restored from a snapshot",
            device.fmt_short()
        ));
    }
    for info in state.devices().filter(|info| info.invite_redeemed_twice) {
        flagged.insert(format!(
            "device {} was bound with an invite that two devices used; `wf fork --exclude` removes one",
            device_label(state, info.device)
        ));
    }
    let devices = state
        .devices()
        .map(|info| DeviceStatus {
            name: info
                .profile
                .map_or_else(|| info.device.fmt_short(), |profile| profile.name.clone()),
            this_device: Some(info.device) == own,
            last_seen: last_seen
                .get(&info.device)
                .map(|millis| timestamp_ms(*millis)),
        })
        .collect();
    for announcement in state.announcements() {
        flagged.insert(format!(
            "{} forked this workforce to {}; `wf fork accept {}` follows it",
            state.actor_label(&Actor::Principal(announcement.by)),
            announcement.to,
            announcement.to
        ));
    }
    let live: Vec<_> = state
        .sessions()
        .filter(|session| session.is_live())
        .collect();
    let parked_sessions = live
        .iter()
        .filter(|session| is_parked(state, &graph, session))
        .count();
    let after_failures: Vec<String> = state
        .inbox()
        .into_iter()
        .filter_map(|entry| match entry.waiting {
            Waiting::Recovery {
                role,
                failures,
                last,
            } => {
                let title = match state.item(entry.item) {
                    Some(ItemView::Live(item)) => item.title().to_string(),
                    Some(ItemView::Archived(_)) | None => "(archived)".to_string(),
                };
                Some(format!(
                    "#{} \"{title}\": {}",
                    entry.item.short(),
                    state.recovery_waits(entry.item, role, failures, last)
                ))
            }
            Waiting::Blocker { .. }
            | Waiting::Proposal
            | Waiting::PrincipalReview { .. }
            | Waiting::Merge { .. } => None,
        })
        .collect();
    let ready_items = state.ready_items().len();
    let auto_agents = state
        .agents()
        .any(|(_, definition)| definition.autonomy.start == StartMode::Auto);
    let an_agent_starts_here = own.is_some_and(|device| {
        state
            .auto_agents(device)
            .iter()
            .any(|(agent, _)| !state.dispatchable(device, agent).is_empty())
    });
    let next = if state.agents().next().is_none() {
        Some("add an agent: `wf agent add`".to_string())
    } else if !daemon && auto_agents {
        Some("start the daemon, which starts the agents: `wf daemon start`".to_string())
    } else if waiting_on_you > 0 {
        Some(format!("{waiting_on_you} waiting on you: `wf inbox`"))
    } else if !live.is_empty() {
        Some("follow the agents: `wf`".to_string())
    } else if ready_items == 0 {
        Some("add an item: `wf item add`".to_string())
    } else if !an_agent_starts_here {
        let items = if ready_items == 1 { "item" } else { "items" };
        Some(format!(
            "{ready_items} ready {items}, but no agent starts on its own: `wf agent edit <name> --start auto`"
        ))
    } else {
        None
    };
    StatusView {
        name: state.name().unwrap_or_default().to_string(),
        id: state
            .workforce_id()
            .map(|id| id.to_string())
            .unwrap_or_default(),
        members: state
            .members()
            .map(|(_, member)| format!("@{} ({:?})", member.handle, member.role).to_lowercase())
            .collect(),
        agents: state.agents().count(),
        events: state.event_count(),
        pending: state.pending_events().count(),
        heads: state.heads().len(),
        archived_items,
        forked_from: state.fork_point().map(|(parent, _)| parent.to_string()),
        devices,
        daemon,
        ready_items,
        live_sessions: live.len(),
        parked_sessions,
        after_failures,
        waiting_on_you,
        flagged: flagged.into_iter().collect(),
        next,
        refolded,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use wf_core::autonomy::{Autonomy, StartMode};
    use wf_core::event::{AgentDefined, DeviceProfile, EventBody};
    use wf_core::name::{AgentName, HarnessName, LaneName};
    use wf_core::testing::*;

    use super::*;

    fn agent(start: StartMode) -> EventBody {
        EventBody::AgentDefined(AgentDefined {
            name: AgentName::new("coder").unwrap(),
            harness: HarnessName::new("opencode").unwrap(),
            model: None,
            instructions: None,
            capabilities: vec![],
            lanes: vec![LaneName::new("default").unwrap()],
            autonomy: Autonomy {
                start,
                ..Autonomy::default()
            },
        })
    }

    fn hosting_coder() -> EventBody {
        EventBody::DeviceProfile(DeviceProfile {
            name: "laptop".into(),
            hardware: String::new(),
            agents: BTreeMap::from([(AgentName::new("coder").unwrap(), 1)]),
            max_sessions: 1,
        })
    }

    fn next(fixture: &Fixture, state: &State) -> Option<String> {
        status(
            state,
            StatusContext {
                own: Some(fixture.devices[0].key()),
                last_seen: &BTreeMap::new(),
                daemon: true,
                waiting_on_you: 0,
                refolded: false,
            },
        )
        .next
    }

    #[test]
    fn ready_items_that_no_agent_starts_on_its_own_are_pointed_out() {
        let fixture = Fixture::new(1);
        let mut state = fixture.state();
        fixture.apply(&mut state, 0, 2_000, agent(StartMode::Manual));
        fixture.apply(&mut state, 0, 3_000, hosting_coder());
        fixture.apply(&mut state, 0, 4_000, create("task", "first"));
        fixture.apply(&mut state, 0, 5_000, create("task", "second"));
        assert_eq!(
            next(&fixture, &state).as_deref(),
            Some(
                "2 ready items, but no agent starts on its own: `wf agent edit <name> --start auto`"
            )
        );

        fixture.apply(&mut state, 0, 6_000, agent(StartMode::Auto));
        assert_eq!(next(&fixture, &state), None);
    }
}

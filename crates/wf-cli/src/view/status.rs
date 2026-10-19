//! `wf status`: whether the workforce is working, and what is next.

use std::fmt;

use jiff::Timestamp;
use serde::Serialize;
use wf_core::autonomy::StartMode;
use wf_core::graph::{DependencyGraph, Readiness};
use wf_core::id::{HashId, ItemId};
use wf_core::identity::Actor;
use wf_core::inbox::Waiting;
use wf_core::item::ItemEntry;
use wf_core::name::AgentName;
use wf_core::session::HarnessFault;
use wf_core::state::State;
use wf_daemon::MachineFaults;
use wf_daemon::caches::CacheUsage;

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
    pub flagged: Vec<Flag>,
    /// What to do next, if anything stands out.
    pub next: Option<String>,
    pub refolded: bool,
    /// What the caches in the worktrees take, when the machine config sets a budget.
    pub caches: Option<CacheUsage>,
}

/// Something wrong with the workforce that needs the principal. `wf status` prints it as one line,
/// and its JSON has that line; the dashboard shows its parts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Flag {
    UnknownState {
        item: ItemId,
        title: String,
        state: String,
    },
    UnknownType {
        item: ItemId,
        title: String,
        item_type: String,
    },
    /// The item is in a state its type no longer uses.
    UnusedState {
        item: ItemId,
        title: String,
        state: String,
    },
    Stranded {
        item: ItemId,
        title: String,
    },
    /// The review of the item waits for a step no agent that starts by itself can take.
    UnstaffedReview {
        item: ItemId,
    },
    AgentWithoutLane {
        agent: AgentName,
    },
    AgentWithoutDevice {
        agent: AgentName,
    },
    UnreadablePolicy {
        error: String,
    },
    /// A dependency of `from` on `to` that would close a cycle.
    IgnoredDependency {
        from: ItemId,
        to: ItemId,
    },
    /// A device wrote two different events with one sequence number.
    Equivocation {
        device: String,
        seq: u64,
    },
    InviteUsedTwice {
        device: String,
    },
    Fork {
        by: String,
        to: String,
    },
}

impl Flag {
    /// What kind of trouble it is, in a few words.
    pub fn heading(&self) -> &'static str {
        match self {
            Flag::UnknownState { .. } => "unknown state",
            Flag::UnknownType { .. } => "unknown type",
            Flag::UnusedState { .. } => "state no longer used",
            Flag::Stranded { .. } => "stranded",
            Flag::UnstaffedReview { .. } => "review no agent can take",
            Flag::AgentWithoutLane { .. } => "agent without a lane",
            Flag::AgentWithoutDevice { .. } => "agent no device hosts",
            Flag::UnreadablePolicy { .. } => "unreadable policy",
            Flag::IgnoredDependency { .. } => "dependency cycle",
            Flag::Equivocation { .. } => "equivocation",
            Flag::InviteUsedTwice { .. } => "invite used twice",
            Flag::Fork { .. } => "fork announced",
        }
    }

    /// What is wrong, without the command that fixes it.
    pub fn what(&self) -> String {
        match self {
            Flag::UnknownState { item, title, state } => format!(
                "#{} \"{title}\" is in the unknown state `{state}`, so it counts as a draft",
                item.short()
            ),
            Flag::UnknownType {
                item,
                title,
                item_type,
            } => format!(
                "#{} \"{title}\" has the unknown type `{item_type}`",
                item.short()
            ),
            Flag::UnusedState { item, title, state } => format!(
                "#{} \"{title}\" is `{state}`, a state its type no longer uses",
                item.short()
            ),
            Flag::Stranded { item, title } => format!(
                "#{} \"{title}\" is stranded: a dependency was cancelled",
                item.short()
            ),
            Flag::UnstaffedReview { item } => format!(
                "the review of #{} waits for an agent step that no agent started on this workforce can take",
                item.short()
            ),
            Flag::AgentWithoutLane { agent } => format!(
                "agent {agent} starts automatically but has no lane, so it never takes an item"
            ),
            Flag::AgentWithoutDevice { agent } => {
                format!("agent {agent} starts automatically but no device hosts it")
            }
            Flag::UnreadablePolicy { error } => format!(
                "this version of wf cannot read the policy set last ({error}), so it runs on the last one it could read; update wf, or replace the policy"
            ),
            Flag::IgnoredDependency { from, to } => format!(
                "dependency #{} -> #{} ignored: concurrent links formed a cycle",
                from.short(),
                to.short()
            ),
            Flag::Equivocation { device, seq } => format!(
                "device {device} wrote two different events with seq {seq}; it was cloned or restored from a snapshot"
            ),
            Flag::InviteUsedTwice { device } => {
                format!("device {device} was bound with an invite that two devices used")
            }
            Flag::Fork { by, to } => format!("{by} forked this workforce to {to}"),
        }
    }

    /// The command that fixes it, where there is one.
    pub fn command(&self) -> Option<String> {
        match self {
            Flag::AgentWithoutLane { agent } => {
                Some(format!("wf agent edit {agent} --lane default"))
            }
            Flag::AgentWithoutDevice { agent } => Some(format!("wf device edit --agent {agent}=1")),
            Flag::UnreadablePolicy { .. } => Some("wf policy edit".to_string()),
            Flag::InviteUsedTwice { .. } => Some("wf fork --exclude".to_string()),
            Flag::Fork { to, .. } => Some(format!("wf fork accept {to}")),
            Flag::UnknownState { .. }
            | Flag::UnknownType { .. }
            | Flag::UnusedState { .. }
            | Flag::Stranded { .. }
            | Flag::UnstaffedReview { .. }
            | Flag::IgnoredDependency { .. }
            | Flag::Equivocation { .. } => None,
        }
    }

    /// The item it is about, where it is about one.
    pub fn item(&self) -> Option<ItemId> {
        match self {
            Flag::UnknownState { item, .. }
            | Flag::UnknownType { item, .. }
            | Flag::UnusedState { item, .. }
            | Flag::Stranded { item, .. }
            | Flag::UnstaffedReview { item }
            | Flag::IgnoredDependency { from: item, .. } => Some(*item),
            Flag::AgentWithoutLane { .. }
            | Flag::AgentWithoutDevice { .. }
            | Flag::UnreadablePolicy { .. }
            | Flag::Equivocation { .. }
            | Flag::InviteUsedTwice { .. }
            | Flag::Fork { .. } => None,
        }
    }
}

/// The line `wf status` prints: what is wrong, then how the command fixes it.
impl fmt::Display for Flag {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        let what = self.what();
        let Some(command) = self.command() else {
            return write!(formatter, "{what}");
        };
        match self {
            Flag::UnreadablePolicy { .. } => write!(formatter, "{what} with `{command}`"),
            Flag::InviteUsedTwice { .. } => write!(formatter, "{what}; `{command}` removes one"),
            Flag::Fork { .. } => write!(formatter, "{what}; `{command}` follows it"),
            Flag::AgentWithoutLane { .. }
            | Flag::AgentWithoutDevice { .. }
            | Flag::UnknownState { .. }
            | Flag::UnknownType { .. }
            | Flag::UnusedState { .. }
            | Flag::Stranded { .. }
            | Flag::UnstaffedReview { .. }
            | Flag::IgnoredDependency { .. }
            | Flag::Equivocation { .. } => write!(formatter, "{what}; `{command}`"),
        }
    }
}

impl Serialize for Flag {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// What `wf status` knows besides the state.
pub struct StatusContext<'a> {
    pub own: Option<wf_core::identity::DeviceKey>,
    pub last_seen: &'a std::collections::BTreeMap<wf_core::identity::DeviceKey, u64>,
    pub daemon: bool,
    pub waiting_on_you: usize,
    pub refolded: bool,
    pub caches: Option<CacheUsage>,
    /// What is wrong with the machine, as far as the daemon says.
    pub machine_faults: &'a MachineFaults,
}

#[derive(Serialize)]
pub struct DeviceStatus {
    pub name: String,
    pub this_device: bool,
    /// Only a running daemon knows.
    pub last_seen: Option<Timestamp>,
}

pub fn status(state: &State, graph: &DependencyGraph, context: StatusContext) -> StatusView {
    let StatusContext {
        own,
        last_seen,
        daemon,
        waiting_on_you,
        refolded,
        caches,
        machine_faults,
    } = context;
    let policy = &state.policy().expect("a workforce has a policy").policy;
    let mut flagged = Vec::new();
    let mut archived_items = 0;
    for view in state.items() {
        match view {
            ItemEntry::Archived(_) => archived_items += 1,
            ItemEntry::Live(item) => {
                let config = policy.type_config(item.item_type());
                let id = item.id();
                let title = item.title().to_string();
                if let Some(name) = item.unknown_state() {
                    flagged.push(Flag::UnknownState {
                        item: id,
                        title,
                        state: name.to_string(),
                    });
                } else if config.is_none() {
                    flagged.push(Flag::UnknownType {
                        item: id,
                        title,
                        item_type: item.item_type().to_string(),
                    });
                } else if config.is_some_and(|config| !config.uses(item.state(policy))) {
                    flagged.push(Flag::UnusedState {
                        item: id,
                        title,
                        state: item.state(policy).to_string(),
                    });
                } else if state.readiness(graph, item) == Some(Readiness::Stranded) {
                    flagged.push(Flag::Stranded { item: id, title });
                }
            }
        }
    }
    for item in state.unstaffed_reviews() {
        flagged.push(Flag::UnstaffedReview { item });
    }
    for (id, definition) in state.agents() {
        if definition.autonomy.start != StartMode::Auto {
            continue;
        }
        if definition.lanes.is_empty() {
            flagged.push(Flag::AgentWithoutLane {
                agent: id.name.clone(),
            });
        }
        let hosted = state.devices().any(|device| {
            device.principal == id.principal
                && device
                    .profile
                    .is_some_and(|profile| profile.agents.contains_key(&id.name))
        });
        if !hosted {
            flagged.push(Flag::AgentWithoutDevice {
                agent: id.name.clone(),
            });
        }
    }
    if let Some(error) = state.unreadable_policy() {
        flagged.push(Flag::UnreadablePolicy {
            error: wf_core::error_chain(&error),
        });
    }
    for edge in graph.ignored() {
        flagged.push(Flag::IgnoredDependency {
            from: edge.from,
            to: edge.to,
        });
    }
    for (device, seq) in state.equivocations() {
        flagged.push(Flag::Equivocation {
            device: device.fmt_short(),
            seq,
        });
    }
    for info in state.devices().filter(|info| info.invite_redeemed_twice) {
        flagged.push(Flag::InviteUsedTwice {
            device: device_label(state, info.device),
        });
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
        flagged.push(Flag::Fork {
            by: state.actor_label(&Actor::Principal(announcement.by)),
            to: announcement.to.to_string(),
        });
    }
    // In the order of their text, as `wf status` lists them.
    flagged.sort_by_cached_key(ToString::to_string);
    flagged.dedup();
    let live: Vec<_> = state
        .sessions()
        .filter(|session| session.is_live())
        .collect();
    let parked_sessions = live
        .iter()
        .filter(|session| state.session_waits(session.id(), graph))
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
                    Some(ItemEntry::Live(item)) => item.title().to_string(),
                    Some(ItemEntry::Archived(_)) | None => "(archived)".to_string(),
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
    let harness_unknown = |harness: &wf_core::name::HarnessName| {
        machine_faults
            .harnesses
            .get(harness)
            .is_some_and(|at_fault| at_fault.fault == HarnessFault::Unknown)
    };
    // Whether an auto agent of this device with an unknown harness (or with a known one) has
    // something to take.
    let takes_here = |device, unknown: bool| {
        state.auto_agents(device).iter().any(|(agent, definition)| {
            harness_unknown(&definition.harness) == unknown
                && !state.dispatchable(device, agent).is_empty()
        })
    };
    let an_agent_starts_here = own.is_some_and(|device| takes_here(device, false));
    let unknown_harness_stops_some = own.is_some_and(|device| takes_here(device, true));
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
        if unknown_harness_stops_some {
            Some(format!(
                "{ready_items} ready {items}, but the harness of every agent that could take them is unknown: `wf agent edit <name> --harness …`"
            ))
        } else {
            Some(format!(
                "{ready_items} ready {items}, but no agent starts on its own: `wf agent edit <name> --start auto`"
            ))
        }
    } else if unknown_harness_stops_some {
        Some(
            "some agents that could take ready items have an unknown harness: `wf agent edit <name> --harness …`"
                .to_string(),
        )
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
            .map(|(_, member)| format!("@{} ({})", member.handle, member.role.name()))
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
        flagged,
        next,
        refolded,
        caches,
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
        next_with(fixture, state, &MachineFaults::default())
    }

    fn next_with(
        fixture: &Fixture,
        state: &State,
        machine_faults: &MachineFaults,
    ) -> Option<String> {
        status(
            state,
            &state.dependency_graph(),
            StatusContext {
                machine_faults,
                own: Some(fixture.devices[0].key()),
                last_seen: &BTreeMap::new(),
                daemon: true,
                waiting_on_you: 0,
                refolded: false,
                caches: None,
            },
        )
        .next
    }

    #[test]
    fn a_flag_prints_and_serializes_as_one_line_with_its_command() {
        let flag = Flag::AgentWithoutDevice {
            agent: AgentName::new("barbara").unwrap(),
        };
        let line = "agent barbara starts automatically but no device hosts it; `wf device edit --agent barbara=1`";
        assert_eq!(flag.to_string(), line);
        assert_eq!(serde_json::to_value(&flag).unwrap(), line);
        let fork = Flag::Fork {
            by: "@ada".to_string(),
            to: "w2".to_string(),
        };
        assert_eq!(
            fork.to_string(),
            "@ada forked this workforce to w2; `wf fork accept w2` follows it"
        );
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

    #[test]
    fn ready_items_whose_agents_have_an_unknown_harness_are_pointed_out() {
        let fixture = Fixture::new(1);
        let mut state = fixture.state();
        fixture.apply(&mut state, 0, 2_000, agent(StartMode::Auto));
        fixture.apply(&mut state, 0, 3_000, hosting_coder());
        fixture.apply(&mut state, 0, 4_000, create("task", "first"));
        let opencode = HarnessName::new("opencode").unwrap();
        let faults = MachineFaults {
            harnesses: BTreeMap::from([(
                opencode,
                wf_daemon::HarnessAtFault {
                    fault: HarnessFault::Unknown,
                    since_ms: 0,
                    last_ms: 0,
                    what: "used by coder".to_string(),
                },
            )]),
            ..MachineFaults::default()
        };
        assert_eq!(
            next_with(&fixture, &state, &faults).as_deref(),
            Some(
                "1 ready item, but the harness of every agent that could take them is unknown: `wf agent edit <name> --harness …`"
            )
        );
    }

    #[test]
    fn ready_items_are_pointed_out_when_only_some_agents_have_an_unknown_harness() {
        let fixture = Fixture::new(1);
        let mut state = fixture.state();
        let defined = |name: &str, harness: &str| {
            EventBody::AgentDefined(AgentDefined {
                name: AgentName::new(name).unwrap(),
                harness: HarnessName::new(harness).unwrap(),
                model: None,
                instructions: None,
                capabilities: vec![],
                lanes: vec![LaneName::new("default").unwrap()],
                autonomy: Autonomy {
                    start: StartMode::Auto,
                    ..Autonomy::default()
                },
            })
        };
        fixture.apply(&mut state, 0, 2_000, defined("coder", "opencode"));
        fixture.apply(&mut state, 0, 2_500, defined("tester", "claud-code"));
        fixture.apply(
            &mut state,
            0,
            3_000,
            EventBody::DeviceProfile(DeviceProfile {
                name: "laptop".into(),
                hardware: String::new(),
                agents: BTreeMap::from([
                    (AgentName::new("coder").unwrap(), 1),
                    (AgentName::new("tester").unwrap(), 1),
                ]),
                max_sessions: 2,
            }),
        );
        fixture.apply(&mut state, 0, 4_000, create("task", "first"));
        let faults = MachineFaults {
            harnesses: BTreeMap::from([(
                HarnessName::new("claud-code").unwrap(),
                wf_daemon::HarnessAtFault {
                    fault: HarnessFault::Unknown,
                    since_ms: 0,
                    last_ms: 0,
                    what: "used by tester".to_string(),
                },
            )]),
            ..MachineFaults::default()
        };
        assert_eq!(
            next_with(&fixture, &state, &faults).as_deref(),
            Some(
                "some agents that could take ready items have an unknown harness: `wf agent edit <name> --harness …`"
            )
        );
    }
}

//! One agent in full, as `wf agent show` shows it: its definition, the devices that host it, its
//! sessions and what was done to it.

use serde::Serialize;
use wf_core::event::{AgentDefined, Event};
use wf_core::identity::{AgentId, DeviceKey};
use wf_core::session::Session;
use wf_core::state::{AgentChange, State};
use wf_core::stats::AgentStats;

use super::session::{AgentView, agent_view, session_view};
use super::{ActivityKind, ActivityView, SessionView, actor_view, timestamp};

/// How many sessions of an agent that ended are shown, besides the live ones.
pub const ENDED_SESSIONS_SHOWN: usize = 10;

#[derive(Serialize)]
pub struct AgentDetail {
    #[serde(flatten)]
    pub definition: AgentView,
    /// The devices that run its sessions.
    pub hosts: Vec<HostView>,
    pub stats: AgentStats,
    /// Its live sessions, then the latest that ended, each the newest first.
    pub sessions: Vec<SessionView>,
    /// How many sessions that ended are older than those in `sessions`.
    pub older_sessions: usize,
    /// When it was defined, changed and removed, and by whom.
    pub activity: Vec<ActivityView>,
}

/// A device that runs an agent, as its profile says.
#[derive(Serialize)]
pub struct HostView {
    pub device: String,
    /// How many sessions of the agent it runs at once.
    pub sessions: u32,
    pub this_device: bool,
}

/// The detail of a defined agent; `None` when `agent` is not defined. `events` is the log, for
/// its activity; `own` is this device, if it has one.
pub fn agent_detail(
    state: &State,
    agent: &AgentId,
    events: &[Event],
    own: Option<DeviceKey>,
) -> Option<AgentDetail> {
    let definition = state.agent(&agent.name, agent.principal)?;
    let hosts = state
        .devices()
        .filter(|info| info.principal == agent.principal)
        .filter_map(|info| {
            let profile = info.profile?;
            Some(HostView {
                device: profile.name.clone(),
                sessions: *profile.agents.get(&agent.name)?,
                this_device: Some(info.device) == own,
            })
        })
        .collect();
    let mut sessions: Vec<&Session> = state
        .sessions()
        .filter(|session| session.agent() == agent)
        .collect();
    sessions.sort_by_key(|session| (!session.is_live(), std::cmp::Reverse(session.started())));
    let live = sessions.iter().filter(|session| session.is_live()).count();
    let shown = sessions.len().min(live + ENDED_SESSIONS_SHOWN);
    let graph = state.dependency_graph();
    Some(AgentDetail {
        definition: agent_view(state, agent, definition),
        hosts,
        stats: state.agent_stats(agent),
        older_sessions: sessions.len() - shown,
        sessions: sessions[..shown]
            .iter()
            .map(|session| session_view(state, &graph, session, None))
            .collect(),
        activity: agent_activity(state, agent, events),
    })
}

/// What was done to an agent: defined, changed, removed, each with what it changed.
fn agent_activity(state: &State, agent: &AgentId, events: &[Event]) -> Vec<ActivityView> {
    let mut previous: Option<&AgentDefined> = None;
    let mut defined_before = false;
    state
        .agent_history(agent, events)
        .into_iter()
        .map(|(event, change)| {
            let summary = match change {
                AgentChange::Defined(defined) => {
                    let summary = match previous {
                        Some(before) => changes(before, defined),
                        None if defined_before => "defined it again".to_string(),
                        None => "defined it".to_string(),
                    };
                    previous = Some(defined);
                    defined_before = true;
                    summary
                }
                AgentChange::Removed => {
                    previous = None;
                    "removed it".to_string()
                }
            };
            ActivityView {
                at: timestamp(event.content.hlc),
                by: actor_view(state, &event.content.actor),
                kind: ActivityKind::Workforce,
                summary,
                comment: None,
                text: None,
            }
        })
        .collect()
}

/// What a definition changed of the one before: `set the model to opus, changed the instructions`.
fn changes(before: &AgentDefined, after: &AgentDefined) -> String {
    fn list(values: &[impl ToString]) -> String {
        if values.is_empty() {
            "none".to_string()
        } else {
            values
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        }
    }
    let duration = |duration: std::time::Duration| humantime::format_duration(duration).to_string();
    let mut changes = Vec::new();
    if before.harness != after.harness {
        changes.push(format!("set the harness to {}", after.harness));
    }
    if before.model != after.model {
        changes.push(match &after.model {
            Some(model) => format!("set the model to {model}"),
            None => "left the model to the harness".to_string(),
        });
    }
    if before.lanes != after.lanes {
        changes.push(format!("set the lanes to {}", list(&after.lanes)));
    }
    if before.capabilities != after.capabilities {
        changes.push(format!(
            "set its capabilities to {}",
            list(&after.capabilities)
        ));
    }
    let (was, is) = (&before.autonomy, &after.autonomy);
    if was.start != is.start {
        changes.push(format!("set it to start {}", is.start));
    }
    if was.permissions != is.permissions {
        changes.push(format!("set permissions to {}", is.permissions));
    }
    if was.nudges != is.nudges {
        changes.push(format!("set nudges to {}", is.nudges));
    }
    if was.park_timeout != is.park_timeout {
        changes.push(format!("set it to park for {}", duration(is.park_timeout)));
    }
    if was.max_duration != is.max_duration {
        changes.push(match is.max_duration {
            Some(max) => format!("limited it to {}", duration(max)),
            None => "lifted its time limit".to_string(),
        });
    }
    if before.instructions != after.instructions {
        changes.push(match after.instructions {
            Some(_) => "changed the instructions".to_string(),
            None => "removed the instructions".to_string(),
        });
    }
    if changes.is_empty() {
        "defined it again, unchanged".to_string()
    } else {
        changes.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use wf_core::event::{AgentRemoved, DeviceProfile, EventBody};
    use wf_core::id::HashId;
    use wf_core::name::AgentName;
    use wf_core::session::EndReason;
    use wf_core::testing::*;

    use super::*;

    fn coder() -> AgentName {
        AgentName::new("coder").unwrap()
    }

    fn deep_coder() -> EventBody {
        let EventBody::AgentDefined(mut defined) = define_agent("coder") else {
            unreachable!("`define_agent` makes an `AgentDefined`")
        };
        defined.model = Some("deep".to_string());
        defined.instructions = Some("Work in **small** commits.".to_string());
        EventBody::AgentDefined(defined)
    }

    #[test]
    fn an_agent_shows_what_was_done_to_it_its_sessions_and_its_hosts() {
        let fixture = Fixture::new(1);
        let mut state = fixture.state();
        let mut events = Vec::new();
        let mut now = 1_000;
        let mut apply = |state: &mut State, body| {
            now += 1_000;
            let event = fixture.apply(state, 0, now, body);
            events.push(event.clone());
            event
        };
        apply(&mut state, define_agent("coder"));
        apply(&mut state, deep_coder());
        apply(
            &mut state,
            EventBody::AgentRemoved(AgentRemoved { name: coder() }),
        );
        apply(&mut state, define_agent("coder"));
        apply(&mut state, define_agent("coder"));
        apply(&mut state, define_agent("other"));
        apply(
            &mut state,
            EventBody::DeviceProfile(DeviceProfile {
                name: "laptop".into(),
                hardware: String::new(),
                agents: BTreeMap::from([(coder(), 2)]),
                max_sessions: 2,
            }),
        );
        let first = apply(&mut state, create("task", "First")).id().into();
        let second = apply(&mut state, create("task", "Second")).id().into();
        let ended = apply(&mut state, start_session("coder", first)).id().into();
        apply(
            &mut state,
            end_session(ended, EndReason::Submitted { commit: None }),
        );
        let live: wf_core::id::SessionId = apply(&mut state, start_session("coder", second))
            .id()
            .into();
        apply(&mut state, start_session("other", first));

        let agent = AgentId {
            principal: fixture.founder.key(),
            name: coder(),
        };
        let own = fixture.devices[0].key();
        let detail = agent_detail(&state, &agent, &events, Some(own)).unwrap();
        let summaries: Vec<&str> = detail
            .activity
            .iter()
            .map(|entry| entry.summary.as_str())
            .collect();
        assert_eq!(
            summaries,
            [
                "defined it",
                "set the model to deep, changed the instructions",
                "removed it",
                "defined it again",
                "defined it again, unchanged",
            ]
        );
        let sessions: Vec<(&str, &str)> = detail
            .sessions
            .iter()
            .map(|session| (session.id.as_str(), session.status.as_str()))
            .collect();
        assert_eq!(
            sessions,
            [
                (live.to_base32().as_str(), "live"),
                (ended.to_base32().as_str(), "submitted")
            ]
        );
        assert_eq!(detail.stats.sessions, 2);
        assert_eq!(detail.older_sessions, 0);
        let hosts: Vec<(&str, u32, bool)> = detail
            .hosts
            .iter()
            .map(|host| (host.device.as_str(), host.sessions, host.this_device))
            .collect();
        assert_eq!(hosts, [("laptop", 2, true)]);
        // Without a device of its own, no host is this one.
        assert!(!agent_detail(&state, &agent, &events, None).unwrap().hosts[0].this_device);
    }
}

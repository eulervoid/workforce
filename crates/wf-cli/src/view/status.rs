//! `wf status`: whether the workforce is working, and what is next.

use std::fmt;

use jiff::Timestamp;
use serde::Serialize;
use wf_core::autonomy::StartMode;
use wf_core::brief::speaker;
use wf_core::claims::Presence;
use wf_core::commits::{AwaitedCommit, Fetching, Naming};
use wf_core::graph::{DependencyGraph, Readiness};
use wf_core::id::{HashId, ItemId};
use wf_core::identity::{Actor, DeviceKey};
use wf_core::inbox::Waiting;
use wf_core::item::ItemEntry;
use wf_core::name::{AgentName, ReviewStepName};
use wf_core::session::HarnessFault;
use wf_core::state::State;
use wf_daemon::MachineFaults;
use wf_daemon::caches::CacheUsage;
use wf_vcs::TargetStanding;

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
    /// What the caches in the agent repositories take, when the machine config sets a budget.
    pub caches: Option<CacheUsage>,
    /// What this device waits for before a session, a review or a merge can start, one line each.
    pub awaiting_commits: Vec<String>,
    /// How the target branch of the principal's checkout differs from wf's, if it does.
    pub checkout_target: Option<CheckoutTarget>,
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
        /// The named steps nobody can take.
        steps: Vec<ReviewStepName>,
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

/// What a flag or an item says the review waits for: the named steps, or just an agent step.
pub fn unstaffed_steps(steps: &[ReviewStepName]) -> String {
    match steps {
        [] => "an agent step".to_string(),
        [only] => format!("the agent step `{only}`"),
        several => format!(
            "the agent steps {}",
            several
                .iter()
                .map(|step| format!("`{step}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
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
            Flag::UnstaffedReview { item, steps } => format!(
                "the review of #{} waits for {} that no agent started on this workforce can take",
                item.short(),
                unstaffed_steps(steps)
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
            | Flag::UnstaffedReview { item, .. }
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

/// How the target branch of the principal's checkout differs from the target in wf's repository,
/// which agents start from. `wf status` prints it as one line, and its JSON has that line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckoutTarget {
    branch: String,
    standing: CheckoutStanding,
}

/// [`TargetStanding`] without `Same`, which says nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CheckoutStanding {
    Ahead { commits: usize },
    Behind { commits: usize },
    Diverged,
}

impl CheckoutTarget {
    /// What there is to say about `branch` standing as `standing`; nothing when it is the same.
    pub fn new(branch: &str, standing: TargetStanding) -> Option<Self> {
        let standing = match standing {
            TargetStanding::Same => return None,
            TargetStanding::Ahead { commits } => CheckoutStanding::Ahead { commits },
            TargetStanding::Behind { commits } => CheckoutStanding::Behind { commits },
            TargetStanding::Diverged => CheckoutStanding::Diverged,
        };
        Some(Self {
            branch: branch.to_string(),
            standing,
        })
    }

    /// Whether agents miss commits of the checkout, which every command warns of. Being behind
    /// is normal after merges, and only `wf status` and the dashboard say so.
    pub fn warns(&self) -> bool {
        match self.standing {
            CheckoutStanding::Ahead { .. } | CheckoutStanding::Diverged => true,
            CheckoutStanding::Behind { .. } => false,
        }
    }

    /// What is wrong in a few words, as the header of the dashboard has it: `main 2 ahead of wf`.
    pub fn brief(&self) -> String {
        let (branch, remote) = (&self.branch, wf_vcs::REMOTE);
        match self.standing {
            CheckoutStanding::Ahead { commits } => format!("{branch} {commits} ahead of {remote}"),
            CheckoutStanding::Behind { commits } => {
                format!("{branch} {commits} behind {remote}")
            }
            CheckoutStanding::Diverged => format!("{branch} diverged from {remote}"),
        }
    }

    /// What is wrong, without the command that fixes it.
    pub fn what(&self) -> String {
        let branch = &self.branch;
        let commits = |count: usize| match count {
            1 => "1 commit".to_string(),
            count => format!("{count} commits"),
        };
        match self.standing {
            CheckoutStanding::Ahead { commits: count } => format!(
                "{branch} here has {} that wf's repository lacks, and agents start from wf's",
                commits(count)
            ),
            CheckoutStanding::Behind { commits: count } => {
                format!("{branch} here is {} behind wf's repository", commits(count))
            }
            CheckoutStanding::Diverged => format!(
                "{branch} here and in wf's repository have diverged, so a push is refused until it is rebased onto {}/{branch}",
                wf_vcs::REMOTE
            ),
        }
    }

    /// The command that fixes it.
    pub fn command(&self) -> String {
        let (remote, branch) = (wf_vcs::REMOTE, &self.branch);
        match self.standing {
            CheckoutStanding::Ahead { .. } => format!("git push {remote} {branch}"),
            CheckoutStanding::Behind { .. } => "git pull".to_string(),
            CheckoutStanding::Diverged => format!("git pull --rebase {remote} {branch}"),
        }
    }
}

/// The line `wf status` prints: what is wrong, then the command that fixes it.
impl fmt::Display for CheckoutTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        write!(formatter, "{}: `{}`", self.what(), self.command())
    }
}

impl Serialize for CheckoutTarget {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// What came of bringing the target of the principal's checkout and wf's in step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TargetFollowed {
    /// One fast-forwarded the other, and they are at the same commit now.
    Moved(CheckoutMove),
    /// They still stand apart, as this says.
    Apart(CheckoutTarget),
}

/// What wf moved to bring the target of the principal's checkout and wf's in step, said in one
/// line: `main: pushed 2 commits to wf`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckoutMove {
    branch: String,
    direction: MoveDirection,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MoveDirection {
    /// The checkout's commits went to wf's repository.
    Pushed { commits: usize },
    /// The checkout took the commits merged in wf's repository.
    FastForwarded { commits: usize },
}

impl CheckoutMove {
    /// `commits` of the checkout's `branch` were pushed to wf's repository.
    pub fn pushed(branch: &str, commits: usize) -> Self {
        Self {
            branch: branch.to_string(),
            direction: MoveDirection::Pushed { commits },
        }
    }

    /// The checkout's `branch` was fast-forwarded by `commits` of wf's repository.
    pub fn fast_forwarded(branch: &str, commits: usize) -> Self {
        Self {
            branch: branch.to_string(),
            direction: MoveDirection::FastForwarded { commits },
        }
    }
}

impl fmt::Display for CheckoutMove {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        let (branch, remote) = (&self.branch, wf_vcs::REMOTE);
        let plural = |count: usize| if count == 1 { "" } else { "s" };
        match self.direction {
            MoveDirection::Pushed { commits } => write!(
                formatter,
                "{branch}: pushed {commits} commit{} to {remote}",
                plural(commits)
            ),
            MoveDirection::FastForwarded { commits } => write!(
                formatter,
                "{branch}: fast-forwarded {commits} merged commit{} from {remote}",
                plural(commits)
            ),
        }
    }
}

/// What `wf status` knows besides the state.
pub struct StatusContext<'a> {
    /// How the target branch of the principal's checkout differs from wf's, if it does.
    pub checkout_target: Option<CheckoutTarget>,
    pub own: Option<DeviceKey>,
    /// What the daemon knows of which devices are there; `None` without a daemon.
    pub presence: Option<&'a Presence>,
    pub now_ms: u64,
    pub daemon: bool,
    pub waiting_on_you: usize,
    pub refolded: bool,
    pub caches: Option<CacheUsage>,
    /// What is wrong with the machine, as far as the daemon says.
    pub machine_faults: &'a MachineFaults,
    /// The commits the log names that this device has not got yet.
    pub awaited: &'a [AwaitedCommit],
}

/// What the commits wait for, as `wf status` and the dashboard say it: the item, the commit, what
/// the log names it for and the device it is to come from, or since when no device had it.
pub fn awaiting_lines(state: &State, awaited: &[AwaitedCommit]) -> Vec<String> {
    awaited
        .iter()
        .map(|AwaitedCommit { named, fetching }| {
            let (waiting, what) = match named.naming {
                Naming::Head { item, session } => {
                    let what = match state.session(session) {
                        Some(session) => {
                            format!("the work of {}", speaker(state, &session.actor()))
                        }
                        None => "the work of a session".to_string(),
                    };
                    (item_label(state, item), what)
                }
                Naming::Submitted { item } => (
                    item_label(state, item),
                    "the commit under review".to_string(),
                ),
                Naming::Target => ("the target".to_string(), "its tip".to_string()),
            };
            let (commit, device) = (named.commit.short(), device_label(state, named.author));
            match fetching {
                Fetching::Underway => {
                    format!("{waiting}: waiting for commit {commit} ({what}) from {device}")
                }
                Fetching::Unavailable { since_ms } => format!(
                    "{waiting}: commit {commit} ({what}) is unavailable: {device} named \
                     it, and no device has had it since {}",
                    super::timestamp_ms(*since_ms)
                ),
            }
        })
        .collect()
}

/// `#abc123 "Title"`, as a line of what waits for commits names the item.
fn item_label(state: &State, item: ItemId) -> String {
    let title = match state.item(item) {
        Some(ItemEntry::Live(live)) => live.title().to_string(),
        Some(ItemEntry::Archived(_)) | None => "(archived)".to_string(),
    };
    format!("#{} \"{title}\"", item.short())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DeviceStatus {
    pub name: String,
    #[serde(flatten)]
    pub presence: DevicePresence,
}

/// Whether a device is there, as far as this machine knows. Written as `presence` with its name,
/// and `last_seen` beside it where there is one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "presence", rename_all = "snake_case")]
pub enum DevicePresence {
    /// The device that asks.
    ThisDevice,
    /// No daemon runs here, and only a running daemon hears from devices.
    Unknown,
    /// Heard from within the lease, or the daemon has not run for a lease yet. `last_seen` is
    /// `None` while it was not heard from since the daemon started.
    There { last_seen: Option<Timestamp> },
    /// Silent for longer than the lease: another device of its principal takes over what its
    /// sessions claim.
    Gone { last_seen: Option<Timestamp> },
}

impl DevicePresence {
    /// Of `device` at `now_ms`, read from what the daemon knows, `presence`, against the lease of
    /// the policy.
    pub fn of(
        state: &State,
        device: DeviceKey,
        own: Option<DeviceKey>,
        presence: Option<&Presence>,
        now_ms: u64,
    ) -> Self {
        if Some(device) == own {
            return DevicePresence::ThisDevice;
        }
        let Some(presence) = presence else {
            return DevicePresence::Unknown;
        };
        let last_seen = presence
            .heard_ms()
            .find(|(heard, _)| *heard == device)
            .map(|(_, at)| timestamp_ms(at));
        if presence.gone(device, state.lease(), now_ms) {
            DevicePresence::Gone { last_seen }
        } else {
            DevicePresence::There { last_seen }
        }
    }
}

pub fn status(state: &State, graph: &DependencyGraph, context: StatusContext) -> StatusView {
    let StatusContext {
        own,
        presence,
        now_ms,
        daemon,
        waiting_on_you,
        refolded,
        caches,
        machine_faults,
        awaited,
        checkout_target,
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
    for (item, steps) in state.unstaffed_reviews() {
        flagged.push(Flag::UnstaffedReview { item, steps });
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
            name: super::device_name(state, info.device),
            presence: DevicePresence::of(state, info.device, own, presence, now_ms),
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
                    state.recovery_waits(entry.item, &role, failures, last)
                ))
            }
            Waiting::Blocker { .. }
            | Waiting::Proposal
            | Waiting::PrincipalReview { .. }
            | Waiting::Mention { .. } => None,
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
                && !state
                    .dispatchable(device, agent, &Default::default())
                    .is_empty()
        })
    };
    let an_agent_starts_here = own.is_some_and(|device| takes_here(device, false));
    let unknown_harness_stops_some = own.is_some_and(|device| takes_here(device, true));
    let warned = checkout_target.as_ref().filter(|target| target.warns());
    let next = if let Some(target) = warned {
        Some(target.to_string())
    } else if state.agents().next().is_none() {
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
        checkout_target.as_ref().map(ToString::to_string)
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
        awaiting_commits: awaiting_lines(state, awaited),
        checkout_target,
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
                awaited: &[],
                own: Some(fixture.devices[0].key()),
                presence: None,
                now_ms: 0,
                daemon: true,
                waiting_on_you: 0,
                refolded: false,
                caches: None,
                checkout_target: None,
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
                    retry: wf_daemon::Retry::Never,
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
                    retry: wf_daemon::Retry::Never,
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

    #[test]
    fn what_waits_for_a_commit_says_which_item_commit_and_device() {
        let fixture = Fixture::new(2);
        let mut state = fixture.state();
        fixture.apply(&mut state, 0, 2_000, define_agent("coder"));
        let item: wf_core::id::ItemId = fixture
            .apply(&mut state, 0, 2_001, create("task", "Parse it"))
            .id()
            .into();
        let started = fixture.apply(&mut state, 1, 2_002, start_session("coder", item));
        let head = wf_core::session::CommitId::Git(
            "0123456789abcdef0123456789abcdef01234567".parse().unwrap(),
        );
        fixture.apply(
            &mut state,
            1,
            2_003,
            end_session_at(
                started.id().into(),
                wf_core::session::EndReason::Released,
                head,
            ),
        );
        let named = state.named_commits();
        assert_eq!(named.len(), 1, "{named:?}");
        let awaited = |fetching| AwaitedCommit {
            named: named[0],
            fetching,
        };
        let device = device_label(&state, fixture.devices[1].key());
        let lines = awaiting_lines(&state, &[awaited(Fetching::Underway)]);
        let line = &lines[0];
        assert!(
            line.contains("\"Parse it\": waiting for commit 0123456"),
            "{line}"
        );
        assert!(line.contains("(the work of"), "{line}");
        assert!(line.contains(&device), "{line}");

        let lines = awaiting_lines(
            &state,
            &[awaited(Fetching::Unavailable {
                since_ms: 1_700_000_000_000,
            })],
        );
        let line = &lines[0];
        assert!(line.contains("commit 0123456"), "{line}");
        assert!(line.contains("is unavailable"), "{line}");
        assert!(line.contains(&format!("{device} named it")), "{line}");
        assert!(line.contains("since 2023-11-14"), "{line}");
        assert!(!line.contains("waiting"), "{line}");
    }

    #[test]
    fn a_device_is_there_until_its_lease_runs_out_and_unknown_without_a_daemon() {
        let fixture = Fixture::new(2);
        let state = fixture.state();
        let (own, other) = (fixture.devices[0].key(), fixture.devices[1].key());
        let lease = u64::try_from(state.lease().as_millis()).unwrap();
        let started = 1_700_000_000_000;
        let heard = started + 1_000;
        let presence = Presence::new(started, [(other, heard)]);
        let of = |device, presence, now_ms| {
            DevicePresence::of(&state, device, Some(own), presence, now_ms)
        };
        assert_eq!(of(own, Some(&presence), heard), DevicePresence::ThisDevice);
        assert_eq!(of(other, None, heard), DevicePresence::Unknown);
        let last_seen = Some(timestamp_ms(heard));
        assert_eq!(
            of(other, Some(&presence), heard + lease),
            DevicePresence::There { last_seen }
        );
        assert_eq!(
            of(other, Some(&presence), heard + lease + 1),
            DevicePresence::Gone { last_seen }
        );
        let never_heard = Presence::new(started, []);
        assert_eq!(
            of(other, Some(&never_heard), started + lease),
            DevicePresence::There { last_seen: None },
            "the daemon has not run for a lease"
        );
        assert_eq!(
            of(other, Some(&never_heard), started + lease + 1),
            DevicePresence::Gone { last_seen: None }
        );
        let json = serde_json::to_value(DeviceStatus {
            name: "vm".into(),
            presence: DevicePresence::Gone { last_seen },
        })
        .unwrap();
        assert_eq!(json["presence"], "gone");
        assert_eq!(json["last_seen"], "2023-11-14T22:13:21Z");
    }
}

//! `wf inbox`: what waits on the principal.

use serde::Serialize;
use wf_core::id::{ItemId, SessionId};
use wf_core::identity::DeviceKey;
use wf_core::inbox::{InboxKind, permission_waits};
use wf_core::session::HarnessFault;
use wf_core::state::State;
use wf_core::target::TargetMove;
use wf_daemon::proto::HeldPermission;
use wf_daemon::{MachineFaults, Retry};

#[derive(Serialize)]
pub struct InboxEntryView {
    pub kind: InboxKind,
    pub item: Option<ItemId>,
    pub session: Option<SessionId>,
    pub title: String,
    pub what: String,
    /// Permission requests only: why each decider of the machine passed it on, as `decider: why`.
    pub passed: Vec<String>,
}

/// What waits on the principal of `own`: what the state says, the permission requests the
/// daemon holds, and what is wrong with the machine.
pub fn inbox_views(
    state: &State,
    own: DeviceKey,
    held: &[HeldPermission],
    machine_faults: &MachineFaults,
) -> Vec<InboxEntryView> {
    let mut entries: Vec<InboxEntryView> = state
        .inbox()
        .into_iter()
        .map(|entry| InboxEntryView {
            kind: entry.waiting.kind(),
            item: Some(entry.item),
            session: None,
            title: state.inbox_title(entry.item),
            what: state.what_waits(&entry),
            passed: Vec::new(),
        })
        .collect();
    entries.extend(
        lost_pushes(state, own)
            .iter()
            .map(|push| lost_push_view(state, push)),
    );
    entries.extend(held.iter().map(|request| {
        let session = state.session(request.session);
        InboxEntryView {
            kind: InboxKind::Permission,
            item: session.map(|session| session.item()),
            session: Some(request.session),
            title: session.map_or_else(String::new, |session| state.inbox_title(session.item())),
            what: permission_waits(
                session.map(|session| session.agent().name.as_str()),
                &request.what,
                request.session,
            ),
            passed: request.passed.clone(),
        }
    }));
    entries.extend(machine_fault_views(machine_faults));
    entries
}

/// How many entries [`inbox_views`] has, without building them.
pub fn inbox_count(
    state: &State,
    own: DeviceKey,
    held: &[HeldPermission],
    machine_faults: &MachineFaults,
) -> usize {
    state.inbox().len() + lost_pushes(state, own).len() + held.len() + machine_faults.count()
}

/// The pushes of the principal of `own` that lost a fork of the target.
pub fn lost_pushes(state: &State, own: DeviceKey) -> Vec<TargetMove> {
    state
        .principal_of(&own)
        .map_or_else(Vec::new, |principal| state.lost_pushes(principal))
}

/// The entry for a push that lost a fork of the target: another device moved it from the same
/// commit first, so the pushed commit is not in it.
pub fn lost_push_view(state: &State, push: &TargetMove) -> InboxEntryView {
    let target = state.policy().map_or("the target", |document| {
        document.policy.merge.target.as_str()
    });
    InboxEntryView {
        kind: InboxKind::LostPush,
        item: None,
        session: None,
        title: format!("The push of {} to {target} lost", push.commit.short()),
        what: format!(
            "rebase it onto {target}, such as with `git pull --rebase`, and push again: another \
             device moved {target} from {} before the push from {}",
            push.onto.short(),
            super::device_name(state, push.device)
        ),
        passed: Vec::new(),
    }
}

/// One entry for each thing wrong with the machine, however many sessions it ended: what it is
/// and what the principal does about it.
pub fn machine_fault_views(faults: &MachineFaults) -> Vec<InboxEntryView> {
    let entry = |title: String, what: String| InboxEntryView {
        kind: InboxKind::MachineFault,
        item: None,
        session: None,
        title,
        what,
        passed: Vec::new(),
    };
    let mut entries: Vec<InboxEntryView> = faults
        .out_of_space
        .iter()
        .map(|full| {
            entry(
                "The disk is full".to_string(),
                format!(
                    "free space on this machine, such as with `cargo clean` in old worktrees; \
                     nothing is started until there is. Since {}, the daemon could not write {}",
                    timestamp(full.since_ms),
                    full.what
                ),
            )
        })
        .collect();
    entries.extend(faults.harnesses.iter().map(|(harness, fault)| {
        let (title, todo) = match fault.fault {
            HarnessFault::LoginLapsed => (
                format!("The login of {harness} has lapsed"),
                format!("log in to {harness} again on this machine"),
            ),
            HarnessFault::UsageLimit => (
                format!("{harness} hit its usage limit"),
                format!("wait for the usage limit of {harness} to reset, or raise it"),
            ),
            HarnessFault::Unknown => (
                format!("{harness} is not a harness wf knows how to start"),
                format!(
                    "give its agents a known harness with `wf agent edit <name> --harness …`, or \
                     add a command for {harness} to the machine config"
                ),
            ),
            HarnessFault::Unreachable => (
                format!("{harness} cannot be reached"),
                format!("check that {harness} is installed and starts on this machine"),
            ),
        };
        entry(
            title,
            format!(
                "{todo}; {}. Since {}: {}",
                match fault.retry {
                    Retry::Never => "its agents do not start until then".to_string(),
                    Retry::At(at_ms) =>
                        format!("its agents are tried again at {}", timestamp(at_ms)),
                },
                timestamp(fault.since_ms),
                fault.what
            ),
        )
    }));
    entries
}

fn timestamp(at_ms: u64) -> String {
    jiff::Timestamp::from_millisecond(i64::try_from(at_ms).unwrap_or(i64::MAX))
        .map_or_else(|_| at_ms.to_string(), |at| at.to_string())
}

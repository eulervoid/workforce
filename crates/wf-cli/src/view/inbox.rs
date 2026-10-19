//! `wf inbox`: what waits on the principal.

use serde::Serialize;
use wf_core::id::{ItemId, SessionId};
use wf_core::inbox::{InboxKind, permission_waits};
use wf_core::session::HarnessFault;
use wf_core::state::State;
use wf_daemon::proto::HeldPermission;
use wf_daemon::{MachineFaults, RETRY_HARNESS};

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

/// What waits on the principal: what the state says, the permission requests the daemon holds,
/// and what is wrong with the machine.
pub fn inbox_views(
    state: &State,
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
    held: &[HeldPermission],
    machine_faults: &MachineFaults,
) -> usize {
    state.inbox().len() + held.len() + machine_faults.count()
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
    let retried = humantime::format_duration(RETRY_HARNESS);
    let mut entries: Vec<InboxEntryView> = faults
        .out_of_space
        .iter()
        .map(|full| {
            entry(
                "The disk is full".to_string(),
                format!(
                    "free space on this machine, such as with `cargo clean` in old worktrees; \
                     nothing is started until there is. Since {}, the daemon could not write {}",
                    since(full.since_ms),
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
                match fault.fault {
                    HarnessFault::Unknown => "its agents do not start until then".to_string(),
                    HarnessFault::LoginLapsed | HarnessFault::Unreachable =>
                        format!("its agents are tried again every {retried}"),
                },
                since(fault.since_ms),
                fault.what
            ),
        )
    }));
    entries
}

fn since(at_ms: u64) -> String {
    jiff::Timestamp::from_millisecond(i64::try_from(at_ms).unwrap_or(i64::MAX))
        .map_or_else(|_| at_ms.to_string(), |at| at.to_string())
}

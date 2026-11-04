//! `wf inbox`: what waits on the principal.

use serde::Serialize;
use wf_core::id::{ItemId, SessionId};
use wf_core::identity::DeviceKey;
use wf_core::inbox::{InboxKind, permission_waits};
use wf_core::session::HarnessFault;
use wf_core::state::State;
use wf_core::target::TargetMove;
use wf_daemon::proto::HeldPermission;
use wf_daemon::{MachineFaults, Retry, UnrelatedTarget};

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
    entries.extend(
        machine_faults
            .unrelated_target
            .iter()
            .map(|unrelated| unrelated_target_view(state, unrelated)),
    );
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

fn target_name(state: &State) -> &str {
    state.policy().map_or("the target", |document| {
        document.policy.merge.target.as_str()
    })
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
    let target = target_name(state);
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

/// The entry for a target branch that shares no history with the tip the log names: no merge
/// starts until the principal says which is right.
pub fn unrelated_target_view(state: &State, unrelated: &UnrelatedTarget) -> InboxEntryView {
    let target = target_name(state);
    let (branch, tip) = (unrelated.branch.short(), unrelated.tip.short());
    InboxEntryView {
        kind: InboxKind::UnrelatedTarget,
        item: None,
        session: None,
        title: format!("{target} has no history in common with the log"),
        what: format!(
            "no merge starts: {target} of wf's repository is at {branch}, the log says {tip}, and \
             they share no history. If it was rewritten on purpose, `wf target adopt {branch}`; \
             if not, put it back: `git -C <wf>/git update-ref refs/heads/{target} {tip}`. Since {}",
            timestamp(unrelated.since_ms)
        ),
        passed: Vec::new(),
    }
}

/// What the daemon knows of one thing wrong with the machine: what the principal does about it,
/// what stands still until then, and what was said when it first happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FaultNotice {
    pub todo: String,
    pub retry: Retry,
    /// What stands still while `retry` is [`Retry::Never`].
    pub blocked: String,
    pub since_ms: u64,
    pub said: Said,
}

/// What a fault came with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Said {
    /// The error of the harness or the disk, as it was reported.
    Error(String),
    /// What wf found out itself.
    Note(String),
}

impl FaultNotice {
    /// In one line, as the inbox of the command line prints it.
    pub fn what(&self) -> String {
        let said = match &self.said {
            Said::Error(text) | Said::Note(text) => text,
        };
        format!(
            "{}; {}. Since {}: {said}",
            self.todo,
            self.next_try(&timestamp),
            timestamp(self.since_ms)
        )
    }

    /// In paragraphs of markdown, with times as `when` tells them and the error as code, broken
    /// into lines of at most `width` columns.
    pub fn paragraphs(&self, when: &dyn Fn(u64) -> String, width: usize) -> Vec<String> {
        let mut paragraphs = vec![
            format!("{}.", capitalized(&self.todo)),
            format!("{}.", capitalized(&self.next_try(when))),
        ];
        match &self.said {
            Said::Error(error) => {
                let options = textwrap::Options::new(width.max(1)).break_words(true);
                let lines = textwrap::wrap(error.trim(), options).join("\n");
                paragraphs.push(format!(
                    "Since {}, it failed with:\n\n~~~~\n{lines}\n~~~~",
                    when(self.since_ms)
                ));
            }
            Said::Note(note) => {
                paragraphs.push(format!("Since {}: {}", when(self.since_ms), note.trim()))
            }
        }
        paragraphs
    }

    fn next_try(&self, when: &dyn Fn(u64) -> String) -> String {
        match self.retry {
            Retry::Never => self.blocked.clone(),
            Retry::At(at_ms) => format!("its agents are tried again at {}", when(at_ms)),
        }
    }
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

/// One entry for each thing wrong with the machine, however many sessions it ended: what it is
/// and what the principal does about it.
pub fn machine_fault_views(faults: &MachineFaults) -> Vec<InboxEntryView> {
    machine_fault_notices(faults)
        .into_iter()
        .map(|(title, notice)| InboxEntryView {
            kind: InboxKind::MachineFault,
            item: None,
            session: None,
            title,
            what: notice.what(),
            passed: Vec::new(),
        })
        .collect()
}

/// Each thing wrong with the machine, under its title.
pub fn machine_fault_notices(faults: &MachineFaults) -> Vec<(String, FaultNotice)> {
    let mut entries: Vec<(String, FaultNotice)> = faults
        .out_of_space
        .iter()
        .map(|full| {
            (
                "The disk is full".to_string(),
                FaultNotice {
                    todo: "free space on this machine, such as with `cargo clean` in old agent \
                           repositories"
                        .to_string(),
                    retry: Retry::Never,
                    blocked: "nothing is started until there is".to_string(),
                    since_ms: full.since_ms,
                    said: Said::Error(format!("could not write {}", full.what)),
                },
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
        let said = match fault.fault {
            HarnessFault::Unknown => Said::Note(fault.what.clone()),
            HarnessFault::LoginLapsed | HarnessFault::UsageLimit | HarnessFault::Unreachable => {
                Said::Error(fault.what.clone())
            }
        };
        (
            title,
            FaultNotice {
                todo,
                retry: fault.retry,
                blocked: "its agents do not start until then".to_string(),
                since_ms: fault.since_ms,
                said,
            },
        )
    }));
    entries
}

fn timestamp(at_ms: u64) -> String {
    jiff::Timestamp::from_millisecond(i64::try_from(at_ms).unwrap_or(i64::MAX))
        .map_or_else(|_| at_ms.to_string(), |at| at.to_string())
}

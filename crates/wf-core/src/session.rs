//! Sessions: one attempt by one agent at one item, on one device. The types here are what the
//! events carry; [`Session`] is what the fold makes of them.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::crdt::Stamp;
use crate::id::{HashVisitor, ItemId, SessionId};
use crate::identity::{Actor, AgentId, DeviceKey};
use crate::name::ReviewStepName;

const GIT_OID_LEN: usize = 20;
const SHORT_COMMIT_LEN: usize = 8;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("a git object id is {} hexadecimal characters", GIT_OID_LEN * 2)]
pub struct GitOidError;

/// The SHA-1 id of a git object.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GitOid([u8; GIT_OID_LEN]);

impl GitOid {
    pub const fn from_bytes(bytes: [u8; GIT_OID_LEN]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; GIT_OID_LEN] {
        &self.0
    }
}

impl fmt::Display for GitOid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&data_encoding::HEXLOWER.encode(&self.0))
    }
}

impl fmt::Debug for GitOid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "GitOid({self})")
    }
}

impl FromStr for GitOid {
    type Err = GitOidError;

    fn from_str(text: &str) -> Result<Self, GitOidError> {
        let bytes = data_encoding::HEXLOWER
            .decode(text.to_ascii_lowercase().as_bytes())
            .map_err(|_| GitOidError)?;
        bytes.try_into().map(Self).map_err(|_| GitOidError)
    }
}

impl Serialize for GitOid {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if serializer.is_human_readable() {
            serializer.collect_str(self)
        } else {
            serializer.serialize_bytes(&self.0)
        }
    }
}

impl<'de> Deserialize<'de> for GitOid {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if deserializer.is_human_readable() {
            let text = String::deserialize(deserializer)?;
            text.parse().map_err(serde::de::Error::custom)
        } else {
            deserializer
                .deserialize_bytes(HashVisitor::<GIT_OID_LEN>)
                .map(GitOid)
        }
    }
}

/// A commit in the repository of the workforce. `Git` is the only version control system.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum CommitId {
    Git(GitOid),
}

impl CommitId {
    /// The first characters of the commit, enough to tell it apart on screen.
    pub fn short(&self) -> String {
        let text = self.to_string();
        short_commit(&text).to_string()
    }
}

/// The short form of a commit already written out as text.
pub fn short_commit(commit: &str) -> &str {
    &commit[..commit.len().min(SHORT_COMMIT_LEN)]
}

impl fmt::Display for CommitId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CommitId::Git(oid) => oid.fmt(formatter),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum SessionRole {
    Work,
    /// A review of what a work session submitted: its commit, or `None` for work submitted
    /// without commits. It takes the step of the review level that has this name, or the one
    /// without a name.
    Review {
        commit: Option<CommitId>,
        step: Option<ReviewStepName>,
    },
    /// Answers or escalates the blockers of the item that wait on a supervisor or on this agent.
    Supervise,
    /// Says what went wrong in the work sessions of the item that did not finish, for the next
    /// attempt: a step of the item's recovery level.
    Diagnose,
}

impl SessionRole {
    pub fn name(&self) -> &'static str {
        match self {
            SessionRole::Work => "work",
            SessionRole::Review { .. } => "review",
            SessionRole::Supervise => "supervise",
            SessionRole::Diagnose => "diagnose",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum EndReason {
    /// The work is done: the commit it hands in is the `head` of the end.
    Submitted,
    /// A review session gave its verdict.
    Reviewed,
    /// A supervise session resolved or escalated the blockers it was started for.
    Supervised,
    /// The agent gave up, or used up its nudges.
    Released,
    /// Parked for too long, or `max_duration` reached.
    TimedOut,
    /// The harness exited or failed, or the daemon could not run it.
    Failed,
    /// Stopped by the principal, or its item moved on.
    Cancelled,
    /// The daemon that ran it stopped, such as for a restart. Not a failure of the attempt.
    Interrupted,
    /// A diagnose session said what went wrong.
    Diagnosed,
    /// The machine could not carry the session. Not a failure of the attempt: the item is tried
    /// again once the machine is fine.
    MachineFault(MachineFault),
}

/// What was wrong with the machine a session ran on, as opposed to with its work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum MachineFault {
    /// The harness says its login has lapsed.
    LoginLapsed,
    /// The disk is full.
    NoSpace,
    /// The harness says its usage limit is reached.
    UsageLimit,
    /// The harness could not be started.
    HarnessUnreachable,
    /// No command is known that starts the harness.
    HarnessUnknown,
}

impl MachineFault {
    pub fn name(&self) -> &'static str {
        match self {
            MachineFault::LoginLapsed => "login_lapsed",
            MachineFault::UsageLimit => "usage_limit",
            MachineFault::NoSpace => "no_space",
            MachineFault::HarnessUnreachable => "harness_unreachable",
            MachineFault::HarnessUnknown => "harness_unknown",
        }
    }

    /// The name for reading: words, not an identifier.
    pub fn label(&self) -> String {
        self.name().replace('_', " ")
    }
}

/// What is wrong with a harness, so that none of its sessions can run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum HarnessFault {
    /// The harness says its login has lapsed.
    LoginLapsed,
    /// The harness says its usage limit is reached; it is fine again at the reset.
    UsageLimit,
    /// The harness could not be started.
    Unreachable,
    /// No command is known that starts the harness: neither the machine config nor wf has one.
    Unknown,
}

impl From<HarnessFault> for MachineFault {
    fn from(fault: HarnessFault) -> Self {
        match fault {
            HarnessFault::LoginLapsed => MachineFault::LoginLapsed,
            HarnessFault::UsageLimit => MachineFault::UsageLimit,
            HarnessFault::Unreachable => MachineFault::HarnessUnreachable,
            HarnessFault::Unknown => MachineFault::HarnessUnknown,
        }
    }
}

/// What an end reason makes of the session, for deciding what happens next and for counting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The work was handed in.
    Submitted,
    /// The session did what it was started for, other than handing in work.
    Finished,
    /// The attempt did not finish: the agent gave up, ran out of time or failed.
    Failed,
    /// Interrupted by a restart, cancelled by the principal or ended by a fault of the machine:
    /// no attempt either way.
    Neither,
}

impl EndReason {
    pub fn outcome(&self) -> Outcome {
        match self {
            EndReason::Submitted => Outcome::Submitted,
            EndReason::Reviewed | EndReason::Supervised | EndReason::Diagnosed => Outcome::Finished,
            EndReason::Released | EndReason::TimedOut | EndReason::Failed => Outcome::Failed,
            EndReason::Interrupted | EndReason::Cancelled | EndReason::MachineFault(_) => {
                Outcome::Neither
            }
        }
    }

    /// The reason's name; a fault of the machine goes by the fault's, so that a sessions list
    /// names it.
    pub fn name(&self) -> &'static str {
        match self {
            EndReason::MachineFault(fault) => fault.name(),
            EndReason::Submitted => "submitted",
            EndReason::Reviewed => "reviewed",
            EndReason::Released => "released",
            EndReason::Supervised => "supervised",
            EndReason::TimedOut => "timed_out",
            EndReason::Failed => "failed",
            EndReason::Cancelled => "cancelled",
            EndReason::Interrupted => "interrupted",
            EndReason::Diagnosed => "diagnosed",
        }
    }

    /// The name for reading: words, not an identifier.
    pub fn label(&self) -> String {
        self.name().replace('_', " ")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Verdict {
    Approve,
    RequestChanges,
    Escalate,
}

/// How a session ended. If several `SessionEnded` events name a session, the earliest counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ended {
    pub reason: EndReason,
    pub(crate) stamp: Stamp,
}

impl Ended {
    pub fn at(&self) -> crate::hlc::Hlc {
        self.stamp.hlc
    }
}

/// A folded session: live from its `SessionStarted` until its `SessionEnded`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub(crate) id: SessionId,
    pub(crate) agent: AgentId,
    pub(crate) item: ItemId,
    pub(crate) role: SessionRole,
    pub(crate) device: DeviceKey,
    pub(crate) started: Stamp,
    pub(crate) ended: Option<Ended>,
    /// The last head the log names for the session, by its comment or its end.
    pub(crate) head: Option<NamedHead>,
}

/// A head as the log names it, and when.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct NamedHead {
    pub(crate) commit: CommitId,
    pub(crate) stamp: Stamp,
}

impl Session {
    pub fn id(&self) -> SessionId {
        self.id
    }

    pub fn agent(&self) -> &AgentId {
        &self.agent
    }

    pub fn item(&self) -> ItemId {
        self.item
    }

    /// Who acts in the session: its agent, as this session.
    pub fn actor(&self) -> Actor {
        Actor::Agent {
            agent: self.agent.clone(),
            session: self.id,
        }
    }

    pub fn role(&self) -> SessionRole {
        self.role.clone()
    }

    /// The device the session runs on.
    pub fn device(&self) -> DeviceKey {
        self.device
    }

    pub fn started(&self) -> Stamp {
        self.started
    }

    pub fn ended(&self) -> Option<&Ended> {
        self.ended.as_ref()
    }

    /// Where the branch of a work session was when the log last named it; `None` if it never did.
    pub fn head(&self) -> Option<CommitId> {
        self.head.map(|named| named.commit)
    }

    /// When the log last named a head for the session.
    pub(crate) fn head_stamp(&self) -> Option<Stamp> {
        self.head.map(|named| named.stamp)
    }

    pub(crate) fn name_head(&mut self, commit: CommitId, stamp: Stamp) {
        if self.head.is_none_or(|named| named.stamp < stamp) {
            self.head = Some(NamedHead { commit, stamp });
        }
    }

    pub fn is_live(&self) -> bool {
        self.ended.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OID: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn git_oids_round_trip_through_text_and_postcard() {
        let oid: GitOid = OID.parse().unwrap();
        assert_eq!(oid.to_string(), OID);
        assert_eq!(OID.to_uppercase().parse::<GitOid>(), Ok(oid));
        let encoded = postcard::to_stdvec(&oid).unwrap();
        assert_eq!(encoded[0], 20);
        assert_eq!(postcard::from_bytes::<GitOid>(&encoded).unwrap(), oid);
        assert_eq!(serde_json::to_string(&oid).unwrap(), format!("\"{OID}\""));
    }

    #[test]
    fn malformed_git_oids_are_refused() {
        assert_eq!("abc".parse::<GitOid>(), Err(GitOidError));
        assert_eq!(OID[..38].parse::<GitOid>(), Err(GitOidError));
        assert_eq!(
            "zz23456789abcdef0123456789abcdef01234567".parse::<GitOid>(),
            Err(GitOidError)
        );
    }
}

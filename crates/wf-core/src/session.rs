//! Sessions: one attempt by one agent at one item, on one device. The types here are what the
//! events carry; [`Session`] is what the fold makes of them.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::crdt::Stamp;
use crate::id::{ItemId, SessionId};
use crate::identity::{AgentId, DeviceKey};

const GIT_OID_LEN: usize = 20;

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
            struct Visitor;

            impl serde::de::Visitor<'_> for Visitor {
                type Value = GitOid;

                fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                    write!(formatter, "{GIT_OID_LEN} bytes")
                }

                fn visit_bytes<E: serde::de::Error>(self, bytes: &[u8]) -> Result<GitOid, E> {
                    bytes
                        .try_into()
                        .map(GitOid)
                        .map_err(|_| E::invalid_length(bytes.len(), &self))
                }
            }

            deserializer.deserialize_bytes(Visitor)
        }
    }
}

/// A commit in the repository of the workforce. `Git` is the only version control system.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum CommitId {
    Git(GitOid),
}

impl fmt::Display for CommitId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CommitId::Git(oid) => oid.fmt(formatter),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum SessionRole {
    Work,
    Review { commit: CommitId },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum EndReason {
    /// The work is done. `None` when the branch has no commits.
    Submitted { commit: Option<CommitId> },
    /// A review session gave its verdict.
    Reviewed,
    /// The agent gave up, or used up its nudges.
    Released,
    /// The item entered a blocked state.
    Blocked,
    /// Parked for too long, or `max_duration` reached.
    TimedOut,
    /// The harness exited or failed, or the daemon restarted.
    Failed,
    /// Stopped by a person, or the claim went to another session.
    Cancelled,
}

impl EndReason {
    pub fn name(&self) -> &'static str {
        match self {
            EndReason::Submitted { .. } => "submitted",
            EndReason::Reviewed => "reviewed",
            EndReason::Released => "released",
            EndReason::Blocked => "blocked",
            EndReason::TimedOut => "timed_out",
            EndReason::Failed => "failed",
            EndReason::Cancelled => "cancelled",
        }
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

    pub fn role(&self) -> SessionRole {
        self.role
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

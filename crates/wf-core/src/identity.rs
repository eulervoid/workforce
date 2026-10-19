//! Principal and device keys, signing, actors and principal-signed statements.
//!
//! Principal keys sign authority statements; device keys sign ordinary events. They are
//! distinct types so a device key can never be used where the system requires a principal.

use std::fmt;
use std::str::FromStr;

pub use iroh_base::Signature;
use iroh_base::{PublicKey, SecretKey};
use serde::{Deserialize, Serialize};

use crate::id::{EventId, SessionId, WorkforceId};
use crate::name::{AgentName, Handle};

const EVENT_DOMAIN: &[u8] = b"wf/event/1\0";
const AUTHORITY_DOMAIN: &[u8] = b"wf/authority/1\0";
const INVITE_DOMAIN: &[u8] = b"wf/invite/1\0";
const HEADS_DOMAIN: &[u8] = b"wf/heads/1\0";

#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error("not a valid public key")]
    Malformed,
    #[error("a secret key is 32 bytes")]
    WrongSecretLength,
    #[error("signature does not match")]
    BadSignature,
    #[error("failed to encode signed content")]
    Encoding(#[from] crate::encoding::EncodingError),
}

macro_rules! key_pair {
    ($(#[$meta:meta])* $public:ident, $signer:ident) => {
        $(#[$meta])*
        ///
        /// The bytes are kept raw: decoding a key from a snapshot or an event must be cheap, and a
        /// key that is not a curve point simply never verifies a signature.
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $public([u8; 32]);

        impl $public {
            pub fn as_bytes(&self) -> &[u8; 32] {
                &self.0
            }

            /// Fails unless the bytes are a valid public key.
            pub fn from_bytes(bytes: &[u8; 32]) -> Result<Self, KeyError> {
                PublicKey::from_bytes(bytes)
                    .map(|key| Self(*key.as_bytes()))
                    .map_err(|_| KeyError::Malformed)
            }

            fn parsed(&self) -> Result<PublicKey, KeyError> {
                PublicKey::from_bytes(&self.0).map_err(|_| KeyError::Malformed)
            }

            pub fn fmt_short(&self) -> String {
                match self.parsed() {
                    Ok(key) => key.fmt_short().to_string(),
                    Err(_) => "invalid".to_string(),
                }
            }

            /// Verifies `message` under `domain`, which says what kind of message it is.
            pub(crate) fn verify(
                &self,
                domain: &[u8],
                message: &[u8],
                signature: &Signature,
            ) -> Result<(), KeyError> {
                let mut signed = Vec::with_capacity(domain.len() + message.len());
                signed.extend_from_slice(domain);
                signed.extend_from_slice(message);
                self.parsed()?
                    .verify(&signed, signature)
                    .map_err(|_| KeyError::BadSignature)
            }
        }

        impl Serialize for $public {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                if serializer.is_human_readable() {
                    serializer.collect_str(self)
                } else {
                    serializer.serialize_bytes(&self.0)
                }
            }
        }

        impl<'de> Deserialize<'de> for $public {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                if deserializer.is_human_readable() {
                    let text = String::deserialize(deserializer)?;
                    text.parse().map_err(serde::de::Error::custom)
                } else {
                    deserializer.deserialize_bytes(crate::id::HashVisitor).map(Self)
                }
            }
        }

        impl fmt::Display for $public {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                match self.parsed() {
                    Ok(key) => fmt::Display::fmt(&key, formatter),
                    Err(_) => formatter.write_str("invalid-key"),
                }
            }
        }

        impl fmt::Debug for $public {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, "{}({})", stringify!($public), self.fmt_short())
            }
        }

        impl FromStr for $public {
            type Err = KeyError;

            fn from_str(text: &str) -> Result<Self, KeyError> {
                let key: PublicKey = text.parse().map_err(|_| KeyError::Malformed)?;
                Ok(Self(*key.as_bytes()))
            }
        }

        #[derive(Clone)]
        pub struct $signer(SecretKey);

        impl $signer {
            /// A new key from 32 random bytes, which the caller draws; wf-core has no randomness.
            pub fn generate(random: [u8; 32]) -> Self {
                Self(SecretKey::from_bytes(&random))
            }

            pub fn from_bytes(bytes: &[u8]) -> Result<Self, KeyError> {
                let bytes: [u8; 32] = bytes.try_into().map_err(|_| KeyError::WrongSecretLength)?;
                Ok(Self(SecretKey::from_bytes(&bytes)))
            }

            pub fn to_bytes(&self) -> [u8; 32] {
                self.0.to_bytes()
            }

            pub fn key(&self) -> $public {
                $public(*self.0.public().as_bytes())
            }

            /// Signs `message` under `domain`, which says what kind of message it is.
            pub(crate) fn sign(&self, domain: &[u8], message: &[u8]) -> Signature {
                let mut signed = Vec::with_capacity(domain.len() + message.len());
                signed.extend_from_slice(domain);
                signed.extend_from_slice(message);
                self.0.sign(&signed)
            }
        }

        impl fmt::Debug for $signer {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, "{}({})", stringify!($signer), self.key().fmt_short())
            }
        }
    };
}

key_pair!(
    /// Identifies the principal. Signs authority statements: genesis, policy, owners, invites.
    PrincipalKey, PrincipalSigner
);
key_pair!(
    /// Identifies a machine. Signs every ordinary event. Never copied between machines.
    DeviceKey, DeviceSigner
);

impl DeviceSigner {
    pub(crate) fn sign_event(&self, encoded_content: &[u8]) -> Signature {
        self.sign(EVENT_DOMAIN, encoded_content)
    }
}

impl DeviceKey {
    pub(crate) fn verify_event(
        &self,
        encoded_content: &[u8],
        signature: &Signature,
    ) -> Result<(), KeyError> {
        self.verify(EVENT_DOMAIN, encoded_content, signature)
    }
}

/// What a device says when it announces its heads on a topic.
#[derive(Serialize)]
struct HeadsPayload<'a> {
    topic: &'a [u8; 32],
    digest: &'a [u8; 32],
    time: u64,
}

fn heads_message(topic: &[u8; 32], digest: &[u8; 32], time: u64) -> Result<Vec<u8>, KeyError> {
    Ok(crate::encoding::encode(&HeadsPayload {
        topic,
        digest,
        time,
    })?)
}

impl DeviceSigner {
    /// Signs an announcement of heads on `topic` at `time` (seconds since the Unix epoch), so
    /// that only this device can say it is online and it cannot be replayed on another topic.
    pub fn sign_heads(
        &self,
        topic: &[u8; 32],
        digest: &[u8; 32],
        time: u64,
    ) -> Result<Signature, KeyError> {
        Ok(self.sign(HEADS_DOMAIN, &heads_message(topic, digest, time)?))
    }
}

impl DeviceKey {
    pub fn verify_heads(
        &self,
        topic: &[u8; 32],
        digest: &[u8; 32],
        time: u64,
        signature: &Signature,
    ) -> Result<(), KeyError> {
        self.verify(HEADS_DOMAIN, &heads_message(topic, digest, time)?, signature)
    }
}

key_pair!(
    /// The secret half travels in an invite code; the public half is published in the log.
    InviteKey, InviteSigner
);

#[derive(Serialize)]
struct InviteProofPayload {
    workforce: WorkforceId,
    invite: EventId,
    device: DeviceKey,
}

impl InviteSigner {
    /// Proves that the holder of the invite secret wants `device` bound.
    pub fn prove(
        &self,
        workforce: WorkforceId,
        invite: EventId,
        device: DeviceKey,
    ) -> Result<Signature, KeyError> {
        let payload = crate::encoding::encode(&InviteProofPayload {
            workforce,
            invite,
            device,
        })?;
        Ok(self.sign(INVITE_DOMAIN, &payload))
    }
}

impl InviteKey {
    pub fn verify_proof(
        &self,
        workforce: WorkforceId,
        invite: EventId,
        device: DeviceKey,
        proof: &Signature,
    ) -> Result<(), KeyError> {
        let payload = crate::encoding::encode(&InviteProofPayload {
            workforce,
            invite,
            device,
        })?;
        self.verify(INVITE_DOMAIN, &payload, proof)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Owner,
    Member,
}

impl Role {
    pub fn name(&self) -> &'static str {
        match self {
            Role::Owner => "owner",
            Role::Member => "member",
        }
    }
}

/// An agent is named by its principal and role name: `@ada/coder`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AgentId {
    pub principal: PrincipalKey,
    pub name: AgentName,
}

/// Who an event is attributed to. The signing device states this truthfully (members are trusted).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Actor {
    Principal(PrincipalKey),
    Agent {
        agent: AgentId,
        session: SessionId,
    },
    /// The daemon of a device of the principal, for what it does by itself: starting and ending
    /// sessions, merging, moving items on. The event says which device.
    Daemon(PrincipalKey),
}

impl Actor {
    /// The accountable principal, whoever acts.
    pub fn principal(&self) -> PrincipalKey {
        match self {
            Actor::Principal(principal) | Actor::Daemon(principal) => *principal,
            Actor::Agent { agent, .. } => agent.principal,
        }
    }
}

/// A principal-signed authority statement. It travels inside an ordinary device-signed event so
/// feeds stay gapless; the signature also covers the workforce so it cannot be replayed elsewhere.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signed<T> {
    pub statement: T,
    pub signer: PrincipalKey,
    signature: Signature,
}

#[derive(Serialize)]
struct SignedPayload<'a, T> {
    workforce: Option<WorkforceId>,
    statement: &'a T,
}

impl<T: Serialize> Signed<T> {
    /// `workforce` is `None` only for genesis, which creates the workforce id.
    pub fn sign(
        signer: &PrincipalSigner,
        workforce: Option<WorkforceId>,
        statement: T,
    ) -> Result<Self, KeyError> {
        let payload = crate::encoding::encode(&SignedPayload {
            workforce,
            statement: &statement,
        })?;
        Ok(Self {
            signature: signer.sign(AUTHORITY_DOMAIN, &payload),
            signer: signer.key(),
            statement,
        })
    }

    pub fn verify(&self, workforce: Option<WorkforceId>) -> Result<(), KeyError> {
        let payload = crate::encoding::encode(&SignedPayload {
            workforce,
            statement: &self.statement,
        })?;
        self.signer.verify(AUTHORITY_DOMAIN, &payload, &self.signature)
    }
}

/// A member of the workforce, as seeded in genesis.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberSeed {
    pub principal: PrincipalKey,
    pub handle: Handle,
    pub devices: Vec<DeviceKey>,
    /// Principal keys this member used in the parent workforce of a fork, so events in the
    /// history keep resolving to the same handle.
    pub previous: Vec<PrincipalKey>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_verify_and_are_domain_separated() {
        let principal = PrincipalSigner::generate([1; 32]);
        let message = b"statement";
        let signature = principal.sign(AUTHORITY_DOMAIN, message);
        assert!(principal.key().verify(AUTHORITY_DOMAIN, message, &signature).is_ok());
        assert!(principal.key().verify(AUTHORITY_DOMAIN, b"other", &signature).is_err());

        let device = DeviceSigner::from_bytes(&principal.to_bytes()).unwrap();
        assert!(device.key().verify_event(message, &signature).is_err());
    }

    #[test]
    fn event_and_heads_signatures_do_not_verify_as_each_other() {
        let signer = DeviceSigner::generate([6; 32]);
        let key = signer.key();
        let (topic, digest) = ([1; 32], [2; 32]);

        let heads = signer.sign_heads(&topic, &digest, 7).unwrap();
        let payload = heads_message(&topic, &digest, 7).unwrap();
        assert!(key.verify_heads(&topic, &digest, 7, &heads).is_ok());
        assert!(key.verify_event(&payload, &heads).is_err());

        let event = signer.sign_event(&payload);
        assert!(key.verify_event(&payload, &event).is_ok());
        assert!(key.verify_heads(&topic, &digest, 7, &event).is_err());
    }

    #[test]
    fn signed_statements_are_bound_to_a_workforce() {
        let principal = PrincipalSigner::generate([2; 32]);
        let workforce = WorkforceId::from(EventId::of(b"one"));
        let other = WorkforceId::from(EventId::of(b"two"));
        let signed = Signed::sign(&principal, Some(workforce), "policy".to_string()).unwrap();
        assert!(signed.verify(Some(workforce)).is_ok());
        assert!(signed.verify(Some(other)).is_err());
        assert!(signed.verify(None).is_err());
    }

    #[test]
    fn signed_statements_detect_tampering() {
        let principal = PrincipalSigner::generate([3; 32]);
        let mut signed = Signed::sign(&principal, None, "policy".to_string()).unwrap();
        signed.statement = "forged".to_string();
        assert!(signed.verify(None).is_err());
    }

    #[test]
    fn secret_key_round_trip() {
        let signer = DeviceSigner::generate([4; 32]);
        let restored = DeviceSigner::from_bytes(&signer.to_bytes()).unwrap();
        assert_eq!(signer.key(), restored.key());
        assert!(DeviceSigner::from_bytes(&[0; 5]).is_err());
    }

    #[test]
    fn actor_resolves_to_principal() {
        let principal = PrincipalSigner::generate([5; 32]).key();
        let agent = Actor::Agent {
            agent: AgentId {
                principal,
                name: AgentName::new("coder").unwrap(),
            },
            session: SessionId::from(EventId::of(b"session")),
        };
        assert_eq!(agent.principal(), principal);
        assert_eq!(Actor::Principal(principal).principal(), principal);
        assert_eq!(Actor::Daemon(principal).principal(), principal);
    }
}

#[cfg(test)]
mod invite_tests {
    use super::*;

    #[test]
    fn proofs_cover_workforce_invite_and_device() {
        let invite = InviteSigner::generate([6; 32]);
        let workforce = WorkforceId::from(EventId::of(b"one"));
        let invite_id = EventId::of(b"invite");
        let device = DeviceSigner::generate([7; 32]).key();
        let proof = invite.prove(workforce, invite_id, device).unwrap();
        let key = invite.key();
        assert!(
            key.verify_proof(workforce, invite_id, device, &proof)
                .is_ok()
        );
        let other_device = DeviceSigner::generate([8; 32]).key();
        assert!(
            key.verify_proof(workforce, invite_id, other_device, &proof)
                .is_err()
        );
        assert!(
            key.verify_proof(workforce, EventId::of(b"x"), device, &proof)
                .is_err()
        );
        let other = WorkforceId::from(EventId::of(b"two"));
        assert!(key.verify_proof(other, invite_id, device, &proof).is_err());
    }
}

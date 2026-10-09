//! The signed event envelope and the M0 event bodies.

use iroh_base::Signature;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::crdt::SetChange;
use crate::encoding::{self, EncodingError};
use crate::hlc::Hlc;
use crate::id::{EventId, ItemId, WorkforceId};
use crate::identity::{Actor, DeviceKey, DeviceSigner, KeyError, MemberSeed, Signed};
use crate::item::Priority;
use crate::name::{AgentName, HarnessName, Label, LaneName, Requirement, StateName, TypeName};

pub const EVENT_VERSION: u16 = 1;

#[derive(Debug, thiserror::Error)]
pub enum EventError {
    #[error(transparent)]
    Encoding(#[from] EncodingError),
    #[error(transparent)]
    Key(#[from] KeyError),
    #[error("signer is not the author of this event")]
    SignerMismatch,
}

/// A frozen point in a parent workforce's history. Always `None` in genesis until `wf fork` (M1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkPoint {
    pub workforce: WorkforceId,
    pub heads: Vec<EventId>,
}

/// Creates a workforce. The founder holds `owner`; seeded members hold `member`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Genesis {
    pub name: String,
    pub founder: MemberSeed,
    pub members: Vec<MemberSeed>,
    /// Policy TOML, stored verbatim.
    pub policy: String,
    pub forked_from: Option<ForkPoint>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicySet {
    /// Policy TOML, stored verbatim.
    pub policy: String,
}

/// Defines or redefines an agent of the acting principal. The name is the identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDefined {
    pub name: AgentName,
    pub harness: HarnessName,
    pub model: Option<String>,
    pub capabilities: Vec<Requirement>,
    pub lanes: Vec<LaneName>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemCreated {
    pub item_type: TypeName,
    pub title: String,
    pub body: String,
    pub priority: Option<Priority>,
    pub lane: Option<LaneName>,
    pub labels: Vec<Label>,
    pub requires: Vec<Requirement>,
}

/// One edit to an item field.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Change {
    Title(String),
    Body(String),
    Priority(Option<Priority>),
    Lane(Option<LaneName>),
    Labels(SetChange<Label>),
    Requires(SetChange<Requirement>),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemEdited {
    pub item: ItemId,
    pub changes: Vec<Change>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemTransitioned {
    pub item: ItemId,
    pub to: StateName,
    pub note: Option<String>,
}

/// Relations that change behaviour. Informational relations are derived from text instead.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Link {
    DependsOn(SetChange<ItemId>),
    /// The item supersedes the target decision. Cannot be removed.
    Supersedes(ItemId),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemLinked {
    pub item: ItemId,
    pub link: Link,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemRef {
    pub item: ItemId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comment {
    pub item: ItemId,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventBody {
    Genesis(Signed<Genesis>),
    PolicySet(Signed<PolicySet>),
    AgentDefined(AgentDefined),
    ItemCreated(ItemCreated),
    ItemEdited(ItemEdited),
    ItemTransitioned(ItemTransitioned),
    ItemLinked(ItemLinked),
    ItemArchived(ItemRef),
    ItemUnarchived(ItemRef),
    Comment(Comment),
    /// A body from a newer peer. Stored and relayed, ignored by the fold.
    Unknown {
        tag: u16,
        payload: Vec<u8>,
    },
}

mod tag {
    pub const GENESIS: u16 = 1;
    pub const POLICY_SET: u16 = 2;
    pub const AGENT_DEFINED: u16 = 3;
    pub const ITEM_CREATED: u16 = 10;
    pub const ITEM_EDITED: u16 = 11;
    pub const ITEM_TRANSITIONED: u16 = 12;
    pub const ITEM_LINKED: u16 = 13;
    pub const ITEM_ARCHIVED: u16 = 14;
    pub const ITEM_UNARCHIVED: u16 = 15;
    pub const COMMENT: u16 = 16;
}

/// The wire form of a body: an explicit tag and a length-prefixed payload, so a peer can
/// carry bodies it does not understand without losing a byte.
#[derive(Serialize, Deserialize)]
struct RawBody {
    tag: u16,
    payload: Vec<u8>,
}

impl EventBody {
    pub fn name(&self) -> &'static str {
        match self {
            EventBody::Genesis(_) => "Genesis",
            EventBody::PolicySet(_) => "PolicySet",
            EventBody::AgentDefined(_) => "AgentDefined",
            EventBody::ItemCreated(_) => "ItemCreated",
            EventBody::ItemEdited(_) => "ItemEdited",
            EventBody::ItemTransitioned(_) => "ItemTransitioned",
            EventBody::ItemLinked(_) => "ItemLinked",
            EventBody::ItemArchived(_) => "ItemArchived",
            EventBody::ItemUnarchived(_) => "ItemUnarchived",
            EventBody::Comment(_) => "Comment",
            EventBody::Unknown { .. } => "Unknown",
        }
    }

    fn to_raw(&self) -> Result<RawBody, EncodingError> {
        let (tag, payload) = match self {
            EventBody::Genesis(body) => (tag::GENESIS, encoding::encode(body)?),
            EventBody::PolicySet(body) => (tag::POLICY_SET, encoding::encode(body)?),
            EventBody::AgentDefined(body) => (tag::AGENT_DEFINED, encoding::encode(body)?),
            EventBody::ItemCreated(body) => (tag::ITEM_CREATED, encoding::encode(body)?),
            EventBody::ItemEdited(body) => (tag::ITEM_EDITED, encoding::encode(body)?),
            EventBody::ItemTransitioned(body) => (tag::ITEM_TRANSITIONED, encoding::encode(body)?),
            EventBody::ItemLinked(body) => (tag::ITEM_LINKED, encoding::encode(body)?),
            EventBody::ItemArchived(body) => (tag::ITEM_ARCHIVED, encoding::encode(body)?),
            EventBody::ItemUnarchived(body) => (tag::ITEM_UNARCHIVED, encoding::encode(body)?),
            EventBody::Comment(body) => (tag::COMMENT, encoding::encode(body)?),
            EventBody::Unknown { tag, payload } => (*tag, payload.clone()),
        };
        Ok(RawBody { tag, payload })
    }

    fn from_raw(raw: RawBody) -> Result<Self, EncodingError> {
        let payload = raw.payload.as_slice();
        Ok(match raw.tag {
            tag::GENESIS => EventBody::Genesis(encoding::decode(payload)?),
            tag::POLICY_SET => EventBody::PolicySet(encoding::decode(payload)?),
            tag::AGENT_DEFINED => EventBody::AgentDefined(encoding::decode(payload)?),
            tag::ITEM_CREATED => EventBody::ItemCreated(encoding::decode(payload)?),
            tag::ITEM_EDITED => EventBody::ItemEdited(encoding::decode(payload)?),
            tag::ITEM_TRANSITIONED => EventBody::ItemTransitioned(encoding::decode(payload)?),
            tag::ITEM_LINKED => EventBody::ItemLinked(encoding::decode(payload)?),
            tag::ITEM_ARCHIVED => EventBody::ItemArchived(encoding::decode(payload)?),
            tag::ITEM_UNARCHIVED => EventBody::ItemUnarchived(encoding::decode(payload)?),
            tag::COMMENT => EventBody::Comment(encoding::decode(payload)?),
            tag => EventBody::Unknown {
                tag,
                payload: raw.payload,
            },
        })
    }
}

impl Serialize for EventBody {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_raw()
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for EventBody {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawBody::deserialize(deserializer)?;
        EventBody::from_raw(raw).map_err(serde::de::Error::custom)
    }
}

/// Everything the author signs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventContent {
    pub v: u16,
    /// `None` only for genesis, whose id becomes the workforce id.
    pub workforce: Option<WorkforceId>,
    pub author: DeviceKey,
    pub actor: Actor,
    /// Per author, gapless, starting at 1.
    pub seq: u64,
    /// The author's previous event. `None` exactly when `seq` is 1.
    pub prev: Option<EventId>,
    /// Heads the author had seen when writing.
    pub deps: Vec<EventId>,
    pub hlc: Hlc,
    pub body: EventBody,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub content: EventContent,
    sig: Signature,
}

impl Event {
    pub fn encode(&self) -> Result<Vec<u8>, EncodingError> {
        encoding::encode(self)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, EncodingError> {
        encoding::decode(bytes)
    }

    /// Signs arbitrary content. Normal code goes through [`EventBuilder`], which fills the
    /// envelope consistently; this exists for tools and for tests that forge invalid events.
    pub fn sign_content(content: EventContent, signer: &DeviceSigner) -> Result<Self, EventError> {
        let sig = signer.sign_event(&encoding::encode(&content)?);
        Ok(Event { content, sig })
    }

    pub fn id(&self) -> EventId {
        EventId::of(&self.encode().expect("events always encode"))
    }

    pub fn verify_signature(&self) -> Result<(), EventError> {
        let encoded = encoding::encode(&self.content)?;
        Ok(self.content.author.verify_event(&encoded, &self.sig)?)
    }

    /// Every event this one must follow: its `prev` and its `deps`.
    pub fn causal_parents(&self) -> impl Iterator<Item = EventId> + '_ {
        self.content.prev.iter().chain(&self.content.deps).copied()
    }
}

/// Fills in the envelope fields that depend on what the author has seen.
#[derive(Clone, Debug)]
pub struct EventBuilder {
    pub(crate) workforce: Option<WorkforceId>,
    pub(crate) author: DeviceKey,
    pub(crate) actor: Actor,
    pub(crate) seq: u64,
    pub(crate) prev: Option<EventId>,
    pub(crate) deps: Vec<EventId>,
    pub(crate) hlc: Hlc,
}

impl EventBuilder {
    /// The first event of a new workforce.
    pub fn genesis(author: DeviceKey, actor: Actor, now_ms: u64) -> Self {
        Self {
            workforce: None,
            author,
            actor,
            seq: 1,
            prev: None,
            deps: Vec::new(),
            hlc: Hlc::next_after(Hlc::ZERO, now_ms),
        }
    }

    pub fn sign(self, signer: &DeviceSigner, body: EventBody) -> Result<Event, EventError> {
        if signer.key() != self.author {
            return Err(EventError::SignerMismatch);
        }
        let mut deps = self.deps;
        deps.sort();
        deps.dedup();
        let content = EventContent {
            v: EVENT_VERSION,
            workforce: self.workforce,
            author: self.author,
            actor: self.actor,
            seq: self.seq,
            prev: self.prev,
            deps,
            hlc: self.hlc,
            body,
        };
        Event::sign_content(content, signer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{PrincipalSigner, Signed};
    use crate::name::Handle;

    fn signers() -> (PrincipalSigner, DeviceSigner) {
        (
            PrincipalSigner::from_bytes(&[1; 32]).unwrap(),
            DeviceSigner::from_bytes(&[2; 32]).unwrap(),
        )
    }

    pub(crate) fn genesis_event() -> Event {
        let (principal, device) = signers();
        let seed = MemberSeed {
            principal: principal.key(),
            handle: Handle::new("joshua").unwrap(),
            devices: vec![device.key()],
        };
        let genesis = Genesis {
            name: "demo".into(),
            founder: seed,
            members: vec![],
            policy: crate::policy::DEFAULT_POLICY_TOML.into(),
            forked_from: None,
        };
        let body = EventBody::Genesis(Signed::sign(&principal, None, genesis).unwrap());
        EventBuilder::genesis(device.key(), Actor::Human(principal.key()), 1_000)
            .sign(&device, body)
            .unwrap()
    }

    #[test]
    fn golden_genesis_event() {
        let event = genesis_event();
        let encoded = event.encode().unwrap();
        assert_eq!(
            (encoded.len(), event.id().to_string()),
            (GOLDEN_LEN, GOLDEN_ID.to_string())
        );
    }

    const GOLDEN_LEN: usize = 1740;
    const GOLDEN_ID: &str = "cw7dlruqu5nvotsgejyz5otdpzregtl3umgfyck5olwvd5qnzqvq";

    #[test]
    fn round_trips_and_verifies() {
        let event = genesis_event();
        let decoded = Event::decode(&event.encode().unwrap()).unwrap();
        assert_eq!(decoded, event);
        assert_eq!(decoded.id(), event.id());
        decoded.verify_signature().unwrap();
    }

    #[test]
    fn tampering_breaks_the_signature() {
        let mut event = genesis_event();
        event.content.hlc = Hlc::next_after(event.content.hlc, 5_000);
        assert!(event.verify_signature().is_err());
    }

    #[test]
    fn signer_must_be_the_author() {
        let (principal, _) = signers();
        let other = DeviceSigner::from_bytes(&[3; 32]).unwrap();
        let builder = EventBuilder::genesis(
            DeviceSigner::from_bytes(&[2; 32]).unwrap().key(),
            Actor::Human(principal.key()),
            1,
        );
        let body = EventBody::Unknown {
            tag: 999,
            payload: vec![],
        };
        assert!(matches!(
            builder.sign(&other, body),
            Err(EventError::SignerMismatch)
        ));
    }

    #[test]
    fn unknown_bodies_round_trip_byte_for_byte() {
        let (principal, device) = signers();
        let body = EventBody::Unknown {
            tag: 4242,
            payload: vec![9, 8, 7, 6],
        };
        let event = EventBuilder::genesis(device.key(), Actor::Human(principal.key()), 1)
            .sign(&device, body.clone())
            .unwrap();
        let bytes = event.encode().unwrap();
        let decoded = Event::decode(&bytes).unwrap();
        assert_eq!(decoded.content.body, body);
        assert_eq!(decoded.encode().unwrap(), bytes);
        assert_eq!(decoded.id(), event.id());
        decoded.verify_signature().unwrap();
    }

    #[test]
    fn known_tag_with_corrupt_payload_is_rejected() {
        let raw = RawBody {
            tag: tag::COMMENT,
            payload: vec![1, 2, 3],
        };
        let bytes = encoding::encode(&raw).unwrap();
        assert!(encoding::decode::<EventBody>(&bytes).is_err());
    }
}

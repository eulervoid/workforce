//! The signed event envelope and the M0 event bodies.

use std::collections::{BTreeMap, BTreeSet};

use iroh_base::Signature;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::autonomy::Autonomy;
use crate::body::StepId;
use crate::crdt::SetChange;
use crate::encoding::{self, EncodingError};
use crate::hlc::Hlc;
use crate::id::{CommentId, EventId, ItemId, SessionId, WorkforceId};
use crate::identity::{
    Actor, DeviceKey, DeviceSigner, InviteKey, KeyError, MemberSeed, PrincipalKey, Signed,
};
use crate::item::Priority;
use crate::name::{
    AgentName, HarnessName, Label, LaneName, NameError, RecoveryLevelName, Requirement,
    ReviewLevelName, StateName, TypeName,
};
use crate::session::{CommitId, EndReason, SessionRole, Verdict};

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

/// A frozen point in a parent workforce's history. `None` in genesis unless the workforce is a fork.
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
    pub autonomy: Autonomy,
    /// What the agent is told in every brief, before the item: how its owner wants it to work.
    pub instructions: Option<String>,
}

/// Removes an agent of the acting principal. Its sessions and what they wrote stay; defining it
/// again brings it back.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRemoved {
    pub name: AgentName,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InviteKind {
    Device,
}

/// Publishes the public half of an invite key. Signed by the principal the invite binds to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteCreated {
    pub kind: InviteKind,
    pub invite: InviteKey,
    /// Milliseconds since the Unix epoch. Enforced by whoever serves the join, not at admission.
    pub expires_ms: u64,
}

/// Binds the author to the principal of the invite. Always the first event of the author's feed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceBound {
    pub invite: EventId,
    /// By the invite key over the workforce, the invite event id and the author key.
    pub proof: Signature,
}

/// What a device says about itself. Last writer wins per device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceProfile {
    pub name: String,
    /// What the device has, as requirements separated by commas or spaces, such as `gpu, os:linux`.
    /// Read it with [`DeviceProfile::hardware_requirements`].
    pub hardware: String,
    pub agents: BTreeMap<AgentName, u32>,
    pub max_sessions: u32,
}

impl DeviceProfile {
    /// Parses hardware as it is written for a profile.
    pub fn parse_hardware(text: &str) -> Result<BTreeSet<Requirement>, NameError> {
        hardware_words(text).map(Requirement::new).collect()
    }

    /// The requirements the hardware of the device meets. Words that are not requirements, as
    /// older versions wrote, are left out.
    pub fn hardware_requirements(&self) -> BTreeSet<Requirement> {
        hardware_words(&self.hardware)
            .filter_map(|word| Requirement::new(word).ok())
            .collect()
    }
}

fn hardware_words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|character: char| character == ',' || character.is_whitespace())
        .filter(|word| !word.is_empty())
}

/// Written in a parent workforce by an owner who forked it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkAnnounced {
    pub to: WorkforceId,
    /// The members the fork keeps.
    pub members: Vec<PrincipalKey>,
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
    /// The review level the item needs; `None` for none.
    pub review: Option<ReviewLevelName>,
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
    ReviewLevel(Option<ReviewLevelName>),
    /// Ticks or unticks a step, named by the hash of its text. Ignored when the body has no
    /// such step.
    StepDone {
        step: StepId,
        done: bool,
    },
    /// The recovery level of the item; `None` goes back to its type's.
    RecoveryLevel(Option<RecoveryLevelName>),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemEdited {
    pub item: ItemId,
    pub changes: Vec<Change>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemMoved {
    pub item: ItemId,
    pub to: StateName,
    /// The session whose start or end this move carries out, when the daemon makes it for one.
    pub by_session: Option<SessionId>,
    /// The comment on the item that says why, written just before.
    pub because: Option<CommentId>,
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
    pub text: String,
    pub kind: CommentKind,
    /// Blockers and change requests of the item that this comment resolves.
    pub resolves: Vec<CommentId>,
    /// The step of the item's body the comment is about, named by the hash of its text. The link
    /// is not checked when read, so it dangles once the step is edited.
    pub step: Option<StepId>,
}

impl Comment {
    pub fn plain(item: ItemId, text: impl Into<String>) -> Self {
        Self {
            item,
            text: text.into(),
            kind: CommentKind::Plain,
            resolves: Vec::new(),
            step: None,
        }
    }
}

/// What a comment holds up until it is resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommentKind {
    Plain,
    /// The work cannot start or continue until someone else resolves it.
    Blocker,
    /// The work has to do this before it is submitted.
    ChangeRequest,
}

impl CommentKind {
    pub fn name(self) -> &'static str {
        match self {
            CommentKind::Plain => "comment",
            CommentKind::Blocker => "blocker",
            CommentKind::ChangeRequest => "change_request",
        }
    }
}

/// A session starts. Its id is the id of this event, and the agent is one of the acting principal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStarted {
    pub agent: AgentName,
    pub item: ItemId,
    pub role: SessionRole,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEnded {
    pub session: SessionId,
    pub reason: EndReason,
}

/// The verdict of a review session or of a principal on the work submitted for an item.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    pub item: ItemId,
    /// The submitted commit, or `None` for work submitted without commits.
    pub commit: Option<CommitId>,
    pub verdict: Verdict,
    /// The comment on the item that says why, written just before.
    pub because: Option<CommentId>,
}

/// The reviewed commit of an item went into the target branch as `commit`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Merged {
    pub item: ItemId,
    pub reviewed: CommitId,
    pub commit: CommitId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventBody {
    Genesis(Signed<Genesis>),
    PolicySet(Signed<PolicySet>),
    AgentDefined(AgentDefined),
    AgentRemoved(AgentRemoved),
    InviteCreated(Signed<InviteCreated>),
    DeviceBound(DeviceBound),
    DeviceProfile(DeviceProfile),
    ForkAnnounced(Signed<ForkAnnounced>),
    ItemCreated(ItemCreated),
    ItemEdited(ItemEdited),
    ItemMoved(ItemMoved),
    ItemLinked(ItemLinked),
    ItemArchived(ItemRef),
    ItemUnarchived(ItemRef),
    Commented(Comment),
    SessionStarted(SessionStarted),
    SessionEnded(SessionEnded),
    Reviewed(Review),
    Merged(Merged),
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
    pub const INVITE_CREATED: u16 = 4;
    pub const DEVICE_BOUND: u16 = 5;
    pub const DEVICE_PROFILE: u16 = 6;
    pub const FORK_ANNOUNCED: u16 = 7;
    pub const AGENT_REMOVED: u16 = 8;
    pub const ITEM_CREATED: u16 = 10;
    pub const ITEM_EDITED: u16 = 11;
    pub const ITEM_MOVED: u16 = 12;
    pub const ITEM_LINKED: u16 = 13;
    pub const ITEM_ARCHIVED: u16 = 14;
    pub const ITEM_UNARCHIVED: u16 = 15;
    pub const COMMENT: u16 = 16;
    pub const SESSION_STARTED: u16 = 20;
    pub const SESSION_ENDED: u16 = 21;
    pub const REVIEW: u16 = 22;
    pub const MERGED: u16 = 23;
    // 24 was `Report`, now a comment with a step. Retired: never reuse it.
}

/// The wire form of a body: an explicit tag and a length-prefixed payload, so a peer can
/// carry bodies it does not understand without losing a byte.
#[derive(Serialize, Deserialize)]
struct TaggedBody {
    tag: u16,
    payload: Vec<u8>,
}

impl EventBody {
    pub fn name(&self) -> &'static str {
        match self {
            EventBody::Genesis(_) => "Genesis",
            EventBody::PolicySet(_) => "PolicySet",
            EventBody::AgentDefined(_) => "AgentDefined",
            EventBody::AgentRemoved(_) => "AgentRemoved",
            EventBody::InviteCreated(_) => "InviteCreated",
            EventBody::DeviceBound(_) => "DeviceBound",
            EventBody::DeviceProfile(_) => "DeviceProfile",
            EventBody::ForkAnnounced(_) => "ForkAnnounced",
            EventBody::ItemCreated(_) => "ItemCreated",
            EventBody::ItemEdited(_) => "ItemEdited",
            EventBody::ItemMoved(_) => "ItemMoved",
            EventBody::ItemLinked(_) => "ItemLinked",
            EventBody::ItemArchived(_) => "ItemArchived",
            EventBody::ItemUnarchived(_) => "ItemUnarchived",
            EventBody::Commented(_) => "Commented",
            EventBody::SessionStarted(_) => "SessionStarted",
            EventBody::SessionEnded(_) => "SessionEnded",
            EventBody::Reviewed(_) => "Reviewed",
            EventBody::Merged(_) => "Merged",
            EventBody::Unknown { .. } => "Unknown",
        }
    }

    /// Serializes what the body carries, without the tag of the wire form: for reading, such as
    /// in `wf export`, not for hashing or signing.
    pub fn serialize_payload<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match self {
            EventBody::Genesis(body) => body.serialize(serializer),
            EventBody::PolicySet(body) => body.serialize(serializer),
            EventBody::AgentDefined(body) => body.serialize(serializer),
            EventBody::AgentRemoved(body) => body.serialize(serializer),
            EventBody::InviteCreated(body) => body.serialize(serializer),
            EventBody::DeviceBound(body) => body.serialize(serializer),
            EventBody::DeviceProfile(body) => body.serialize(serializer),
            EventBody::ForkAnnounced(body) => body.serialize(serializer),
            EventBody::ItemCreated(body) => body.serialize(serializer),
            EventBody::ItemEdited(body) => body.serialize(serializer),
            EventBody::ItemMoved(body) => body.serialize(serializer),
            EventBody::ItemLinked(body) => body.serialize(serializer),
            EventBody::ItemArchived(body) => body.serialize(serializer),
            EventBody::ItemUnarchived(body) => body.serialize(serializer),
            EventBody::Commented(body) => body.serialize(serializer),
            EventBody::SessionStarted(body) => body.serialize(serializer),
            EventBody::SessionEnded(body) => body.serialize(serializer),
            EventBody::Reviewed(body) => body.serialize(serializer),
            EventBody::Merged(body) => body.serialize(serializer),
            EventBody::Unknown { tag, payload } => (tag, payload).serialize(serializer),
        }
    }

    fn to_tagged(&self) -> Result<TaggedBody, EncodingError> {
        let (tag, payload) = match self {
            EventBody::Genesis(body) => (tag::GENESIS, encoding::encode(body)?),
            EventBody::PolicySet(body) => (tag::POLICY_SET, encoding::encode(body)?),
            EventBody::AgentDefined(body) => (tag::AGENT_DEFINED, encoding::encode(body)?),
            EventBody::AgentRemoved(body) => (tag::AGENT_REMOVED, encoding::encode(body)?),
            EventBody::InviteCreated(body) => (tag::INVITE_CREATED, encoding::encode(body)?),
            EventBody::DeviceBound(body) => (tag::DEVICE_BOUND, encoding::encode(body)?),
            EventBody::DeviceProfile(body) => (tag::DEVICE_PROFILE, encoding::encode(body)?),
            EventBody::ForkAnnounced(body) => (tag::FORK_ANNOUNCED, encoding::encode(body)?),
            EventBody::ItemCreated(body) => (tag::ITEM_CREATED, encoding::encode(body)?),
            EventBody::ItemEdited(body) => (tag::ITEM_EDITED, encoding::encode(body)?),
            EventBody::ItemMoved(body) => (tag::ITEM_MOVED, encoding::encode(body)?),
            EventBody::ItemLinked(body) => (tag::ITEM_LINKED, encoding::encode(body)?),
            EventBody::ItemArchived(body) => (tag::ITEM_ARCHIVED, encoding::encode(body)?),
            EventBody::ItemUnarchived(body) => (tag::ITEM_UNARCHIVED, encoding::encode(body)?),
            EventBody::Commented(body) => (tag::COMMENT, encoding::encode(body)?),
            EventBody::SessionStarted(body) => (tag::SESSION_STARTED, encoding::encode(body)?),
            EventBody::SessionEnded(body) => (tag::SESSION_ENDED, encoding::encode(body)?),
            EventBody::Reviewed(body) => (tag::REVIEW, encoding::encode(body)?),
            EventBody::Merged(body) => (tag::MERGED, encoding::encode(body)?),
            EventBody::Unknown { tag, payload } => (*tag, payload.clone()),
        };
        Ok(TaggedBody { tag, payload })
    }

    fn from_tagged(tagged: TaggedBody) -> Result<Self, EncodingError> {
        let payload = tagged.payload.as_slice();
        Ok(match tagged.tag {
            tag::GENESIS => EventBody::Genesis(encoding::decode(payload)?),
            tag::POLICY_SET => EventBody::PolicySet(encoding::decode(payload)?),
            tag::AGENT_DEFINED => EventBody::AgentDefined(encoding::decode(payload)?),
            tag::AGENT_REMOVED => EventBody::AgentRemoved(encoding::decode(payload)?),
            tag::INVITE_CREATED => EventBody::InviteCreated(encoding::decode(payload)?),
            tag::DEVICE_BOUND => EventBody::DeviceBound(encoding::decode(payload)?),
            tag::DEVICE_PROFILE => EventBody::DeviceProfile(encoding::decode(payload)?),
            tag::FORK_ANNOUNCED => EventBody::ForkAnnounced(encoding::decode(payload)?),
            tag::ITEM_CREATED => EventBody::ItemCreated(encoding::decode(payload)?),
            tag::ITEM_EDITED => EventBody::ItemEdited(encoding::decode(payload)?),
            tag::ITEM_MOVED => EventBody::ItemMoved(encoding::decode(payload)?),
            tag::ITEM_LINKED => EventBody::ItemLinked(encoding::decode(payload)?),
            tag::ITEM_ARCHIVED => EventBody::ItemArchived(encoding::decode(payload)?),
            tag::ITEM_UNARCHIVED => EventBody::ItemUnarchived(encoding::decode(payload)?),
            tag::COMMENT => EventBody::Commented(encoding::decode(payload)?),
            tag::SESSION_STARTED => EventBody::SessionStarted(encoding::decode(payload)?),
            tag::SESSION_ENDED => EventBody::SessionEnded(encoding::decode(payload)?),
            tag::REVIEW => EventBody::Reviewed(encoding::decode(payload)?),
            tag::MERGED => EventBody::Merged(encoding::decode(payload)?),
            tag => EventBody::Unknown {
                tag,
                payload: tagged.payload,
            },
        })
    }
}

impl Serialize for EventBody {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_tagged()
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for EventBody {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let tagged = TaggedBody::deserialize(deserializer)?;
        EventBody::from_tagged(tagged).map_err(serde::de::Error::custom)
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
    /// Per author, starting at 1. A feed that forks (equivocation) has several events per `seq`.
    pub seq: u64,
    /// The author's event with `seq - 1`. `None` exactly when `seq` is 1.
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

    /// Names an event the author depends on even though it is not a head, such as the invite a
    /// `DeviceBound` redeems.
    pub fn depending_on(mut self, event: EventId) -> Self {
        self.deps.push(event);
        self
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
            handle: Handle::new("ada").unwrap(),
            devices: vec![device.key()],
            previous: vec![],
        };
        let genesis = Genesis {
            name: "demo".into(),
            founder: seed,
            members: vec![],
            // Fixed text, so the golden event pins the encoding, not the default policy.
            policy: "[merge]\ntarget = \"main\"\n".into(),
            forked_from: None,
        };
        let body = EventBody::Genesis(Signed::sign(&principal, None, genesis).unwrap());
        EventBuilder::genesis(device.key(), Actor::Principal(principal.key()), 1_000)
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

    fn golden_body_bytes(body: &EventBody) -> String {
        data_encoding::HEXLOWER.encode(&encoding::encode(body).unwrap())
    }

    #[test]
    fn golden_comment_body() {
        let body = EventBody::Commented(Comment {
            item: ItemId::from_bytes([1; 32]),
            text: "why".into(),
            kind: CommentKind::ChangeRequest,
            resolves: vec![CommentId::from_bytes([3; 32])],
            step: Some(StepId::of("pick storage")),
        });
        assert_eq!(golden_body_bytes(&body), GOLDEN_COMMENT_BODY);
    }

    const GOLDEN_COMMENT_BODY: &str = "105120010101010101010101010101010101010101010101010101010101010101010103776879020120030303030303030303030303030303030303030303030303030303030303030301b67b3441c138056f";

    #[test]
    fn golden_session_bodies() {
        let item = ItemId::from_bytes([1; 32]);
        let session = SessionId::from_bytes([2; 32]);
        let commit = CommitId::Git("00112233445566778899aabbccddeeff00112233".parse().unwrap());
        let bodies = [
            EventBody::SessionStarted(SessionStarted {
                agent: AgentName::new("coder").unwrap(),
                item,
                role: SessionRole::Review {
                    commit: Some(commit),
                },
            }),
            EventBody::SessionEnded(SessionEnded {
                session,
                reason: EndReason::Submitted {
                    commit: Some(commit),
                },
            }),
            EventBody::Reviewed(Review {
                item,
                commit: Some(commit),
                verdict: Verdict::RequestChanges,
                because: Some(CommentId::from(EventId::of(b"tests"))),
            }),
            EventBody::Merged(Merged {
                item,
                reviewed: commit,
                commit,
            }),
        ];
        let encoded: Vec<String> = bodies.iter().map(golden_body_bytes).collect();
        assert_eq!(encoded, GOLDEN_SESSION_BODIES);
    }

    const GOLDEN_SESSION_BODIES: [&str; 4] = [
        "143f05636f6465722001010101010101010101010101010101010101010101010101010101010101010101001400112233445566778899aabbccddeeff00112233",
        "15392002020202020202020202020202020202020202020202020202020202020202020001001400112233445566778899aabbccddeeff00112233",
        "165b20010101010101010101010101010101010101010101010101010101010101010101001400112233445566778899aabbccddeeff0011223301012065e27c6eb2ae7ce97aceb1a372b6fb11fbc5bbffd95ee6fa17949cd148c6f730",
        "174d200101010101010101010101010101010101010101010101010101010101010101001400112233445566778899aabbccddeeff00112233001400112233445566778899aabbccddeeff00112233",
    ];

    const GOLDEN_LEN: usize = 343;
    const GOLDEN_ID: &str = "x47qrvvox2dqfkr2nzucimqflq2usnxuo3o65b5uagx7xge7ipxa";

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
            Actor::Principal(principal.key()),
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
        let event = EventBuilder::genesis(device.key(), Actor::Principal(principal.key()), 1)
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
        let tagged = TaggedBody {
            tag: tag::COMMENT,
            payload: vec![1, 2, 3],
        };
        let bytes = encoding::encode(&tagged).unwrap();
        assert!(encoding::decode::<EventBody>(&bytes).is_err());
    }
}

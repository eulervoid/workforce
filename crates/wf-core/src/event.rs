//! The signed event envelope and the M0 event bodies.

use std::borrow::Cow;
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
use crate::item::{ItemState, Priority};
use crate::name::{
    AgentName, HarnessName, Label, LaneName, NameError, RecoveryLevelName, Requirement,
    ReviewLevelName, TypeName,
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

/// A policy as written before it was bound to its event (tag 2). Still decoded and folded, but
/// no new one is authored: see [`BoundPolicySet`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicySet {
    /// Policy TOML, stored verbatim.
    pub policy: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundPolicySet {
    /// Policy TOML, stored verbatim.
    pub policy: String,
    /// The `hlc` of the event that carries the policy. The signature covers only the workforce
    /// and this statement, so without it any member's device could write an old signed policy
    /// into a new event and let it win by its newer stamp.
    pub hlc: Hlc,
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

/// A state as an event names it. Events written before the states were fixed name them the way
/// their type did, such as `in_progress` or `ratified`; the name is kept as written, so the event
/// still encodes to the bytes that were signed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WrittenState(String);

impl WrittenState {
    /// A name as an older version wrote it, for tests that write such events.
    #[cfg(any(test, feature = "testing"))]
    pub fn as_written(name: &str) -> Self {
        WrittenState(name.to_string())
    }

    /// A name no version knew reads as `draft`, so nothing starts the item.
    pub fn state(&self) -> ItemState {
        ItemState::from_written(&self.0).unwrap_or(ItemState::Draft)
    }

    /// The name, when it is not one this version reads.
    pub fn unknown(&self) -> Option<&str> {
        match ItemState::from_written(&self.0) {
            Some(_) => None,
            None => Some(&self.0),
        }
    }
}

impl From<ItemState> for WrittenState {
    fn from(state: ItemState) -> Self {
        WrittenState(state.name().to_string())
    }
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
    /// `None` only in events written before items were created in a state: those start in the
    /// initial state of their type.
    pub state: Option<WrittenState>,
}

/// The wire form of `ItemCreated` before it carried a state.
///
/// Borrows what it encodes, so that an event encoded for its id copies none of its text.
#[derive(Serialize, Deserialize)]
struct ItemCreatedWithoutState<'a> {
    item_type: Cow<'a, TypeName>,
    title: Cow<'a, str>,
    body: Cow<'a, str>,
    priority: Option<Priority>,
    lane: Option<Cow<'a, LaneName>>,
    labels: Cow<'a, [Label]>,
    requires: Cow<'a, [Requirement]>,
    review: Option<Cow<'a, ReviewLevelName>>,
}

impl ItemCreated {
    fn to_tagged(&self) -> Result<(u16, Vec<u8>), EncodingError> {
        if self.state.is_some() {
            return Ok((tag::ITEM_CREATED, encoding::encode(self)?));
        }
        let without_state = ItemCreatedWithoutState {
            item_type: Cow::Borrowed(&self.item_type),
            title: Cow::Borrowed(&self.title),
            body: Cow::Borrowed(&self.body),
            priority: self.priority,
            lane: self.lane.as_ref().map(Cow::Borrowed),
            labels: Cow::Borrowed(&self.labels),
            requires: Cow::Borrowed(&self.requires),
            review: self.review.as_ref().map(Cow::Borrowed),
        };
        Ok((
            tag::ITEM_CREATED_WITHOUT_STATE,
            encoding::encode(&without_state)?,
        ))
    }

    /// One without a state would encode to other bytes than were signed.
    fn with_state(payload: &[u8]) -> Result<Self, EncodingError> {
        let created: ItemCreated = encoding::decode(payload)?;
        match created.state {
            Some(_) => Ok(created),
            None => Err(EncodingError::malformed()),
        }
    }

    fn without_state(payload: &[u8]) -> Result<Self, EncodingError> {
        let old: ItemCreatedWithoutState = encoding::decode(payload)?;
        Ok(ItemCreated {
            item_type: old.item_type.into_owned(),
            title: old.title.into_owned(),
            body: old.body.into_owned(),
            priority: old.priority,
            lane: old.lane.map(Cow::into_owned),
            labels: old.labels.into_owned(),
            requires: old.requires.into_owned(),
            review: old.review.map(Cow::into_owned),
            state: None,
        })
    }
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
    pub to: WrittenState,
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
    /// The lines of the diff of the item's work the comment is about.
    pub lines: Option<LineAnchor>,
}

/// The wire form of `Comment` before it could be about lines of a diff, and still for one that
/// is not. Borrows what it encodes.
#[derive(Serialize, Deserialize)]
struct CommentWithoutLines<'a> {
    item: ItemId,
    text: Cow<'a, str>,
    kind: CommentKind,
    resolves: Cow<'a, [CommentId]>,
    step: Option<StepId>,
}

impl Comment {
    pub fn plain(item: ItemId, text: impl Into<String>) -> Self {
        Self {
            item,
            text: text.into(),
            kind: CommentKind::Plain,
            resolves: Vec::new(),
            step: None,
            lines: None,
        }
    }

    /// The text as it reads away from the diff: where the lines it is about are, and the lines
    /// themselves, above it.
    pub fn text_with_lines(&self) -> String {
        match &self.lines {
            None => self.text.clone(),
            Some(lines) => {
                let quote: String = lines
                    .quote
                    .lines()
                    .map(|line| format!("    {line}\n"))
                    .collect();
                format!("{}:\n{quote}\n{}", lines.heading(), self.text)
            }
        }
    }

    fn to_tagged(&self) -> Result<(u16, Vec<u8>), EncodingError> {
        if self.kind == CommentKind::CheckFailure {
            return Ok((tag::COMMENT_CHECK_FAILURE, encoding::encode(self)?));
        }
        match self.lines {
            Some(_) => Ok((tag::COMMENT_ON_LINES, encoding::encode(self)?)),
            None => Ok((
                tag::COMMENT,
                encoding::encode(&CommentWithoutLines {
                    item: self.item,
                    text: Cow::Borrowed(&self.text),
                    kind: self.kind,
                    resolves: Cow::Borrowed(&self.resolves),
                    step: self.step,
                })?,
            )),
        }
    }

    /// One without lines would encode to other bytes than were signed.
    fn on_lines(payload: &[u8]) -> Result<Self, EncodingError> {
        let comment: Comment = encoding::decode(payload)?;
        match (comment.kind, &comment.lines) {
            (CommentKind::CheckFailure, _) | (_, None) => Err(EncodingError::malformed()),
            (_, Some(_)) => Ok(comment),
        }
    }

    /// A check failure under any other tag would encode to other bytes than were signed.
    fn check_failure(payload: &[u8]) -> Result<Self, EncodingError> {
        let comment: Comment = encoding::decode(payload)?;
        match comment.kind {
            CommentKind::CheckFailure => Ok(comment),
            CommentKind::Plain | CommentKind::Blocker | CommentKind::ChangeRequest => {
                Err(EncodingError::malformed())
            }
        }
    }

    fn without_lines(payload: &[u8]) -> Result<Self, EncodingError> {
        let old: CommentWithoutLines = encoding::decode(payload)?;
        if old.kind == CommentKind::CheckFailure {
            return Err(EncodingError::malformed());
        }
        Ok(Comment {
            item: old.item,
            text: old.text.into_owned(),
            kind: old.kind,
            resolves: old.resolves.into_owned(),
            step: old.step,
            lines: None,
        })
    }
}

/// Lines of the diff of an item's work against its target, as a comment names them: they stay
/// where they were when the branch moves on, as of the commit they were read at.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineAnchor {
    /// The commit whose diff the lines are of.
    pub commit: CommitId,
    /// The file, by its path after the change.
    pub path: String,
    /// Which side of the diff `first` and `last` count on.
    pub side: DiffSide,
    /// The first and the last line, from 1, the last no smaller than the first.
    pub first: u32,
    pub last: u32,
    /// The lines as the diff showed them, each after its `+`, `-` or space: for reading the
    /// comment without the diff, and after the lines changed.
    pub quote: String,
}

/// Which side of a diff a line number counts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffSide {
    /// Before the change: for lines it removes, and only those.
    Old,
    /// After the change.
    New,
}

impl LineAnchor {
    /// `src/lib.rs:12-14`, `src/lib.rs:12`: where the lines are, as editors and compilers write
    /// it.
    pub fn location(&self) -> String {
        let lines = if self.first == self.last {
            self.first.to_string()
        } else {
            format!("{}-{}", self.first, self.last)
        };
        format!("{}:{lines}", self.path)
    }

    /// `src/lib.rs:12-14 at a8d49484`, saying which side lines removed are numbered on.
    pub fn heading(&self) -> String {
        let commit = self.commit.short();
        let side = match self.side {
            DiffSide::New => "",
            DiffSide::Old => ", removed, numbered as before the change,",
        };
        format!("{}{side} at {commit}", self.location())
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
    /// A change request the daemon's check runner wrote because a check of the item's merge
    /// result failed. Counted by [`State::failed_check_runs`](crate::state::State), and otherwise
    /// a change request.
    CheckFailure,
}

impl CommentKind {
    /// Whether the comment asks the work to change before it is submitted: a change request,
    /// whoever wrote it.
    pub fn is_change_request(self) -> bool {
        match self {
            CommentKind::ChangeRequest | CommentKind::CheckFailure => true,
            CommentKind::Plain | CommentKind::Blocker => false,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            CommentKind::Plain => "comment",
            CommentKind::Blocker => "blocker",
            CommentKind::ChangeRequest | CommentKind::CheckFailure => "change_request",
        }
    }

    /// The name for reading: words, not an identifier.
    pub fn label(self) -> String {
        self.name().replace('_', " ")
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
    /// The comment on the item that says why, written just before.
    pub because: Option<CommentId>,
}

/// The wire form of `SessionEnded` before it named the comment that says why.
#[derive(Serialize, Deserialize)]
struct SessionEndedWithoutComment {
    session: SessionId,
    reason: EndReason,
}

impl SessionEnded {
    fn to_tagged(&self) -> Result<(u16, Vec<u8>), EncodingError> {
        match self.because {
            Some(_) => Ok((tag::SESSION_ENDED, encoding::encode(self)?)),
            None => Ok((
                tag::SESSION_ENDED_WITHOUT_COMMENT,
                encoding::encode(&SessionEndedWithoutComment {
                    session: self.session,
                    reason: self.reason,
                })?,
            )),
        }
    }

    /// One without a comment would encode to other bytes than were signed.
    fn with_comment(payload: &[u8]) -> Result<Self, EncodingError> {
        let ended: SessionEnded = encoding::decode(payload)?;
        match ended.because {
            Some(_) => Ok(ended),
            None => Err(EncodingError::malformed()),
        }
    }

    fn without_comment(payload: &[u8]) -> Result<Self, EncodingError> {
        let old: SessionEndedWithoutComment = encoding::decode(payload)?;
        Ok(SessionEnded {
            session: old.session,
            reason: old.reason,
            because: None,
        })
    }
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
    BoundPolicySet(Signed<BoundPolicySet>),
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
    /// `ItemCreated` as written before it carried a state.
    pub const ITEM_CREATED_WITHOUT_STATE: u16 = 10;
    pub const ITEM_EDITED: u16 = 11;
    pub const ITEM_MOVED: u16 = 12;
    pub const ITEM_LINKED: u16 = 13;
    pub const ITEM_ARCHIVED: u16 = 14;
    pub const ITEM_UNARCHIVED: u16 = 15;
    /// `Comment` as written before it could be about lines, and still for one that is not.
    pub const COMMENT: u16 = 16;
    pub const ITEM_CREATED: u16 = 17;
    pub const SESSION_STARTED: u16 = 20;
    /// `SessionEnded` as written before it named the comment that says why, and still for an end
    /// without one.
    pub const SESSION_ENDED_WITHOUT_COMMENT: u16 = 21;
    pub const REVIEW: u16 = 22;
    pub const MERGED: u16 = 23;
    // 24 was `Report`, now a comment with a step. Retired: never reuse it.
    pub const SESSION_ENDED: u16 = 25;
    /// `Comment` about lines of a diff; one that is not is still written as `COMMENT`.
    pub const COMMENT_ON_LINES: u16 = 26;
    /// `Comment` of kind `CheckFailure`. Peers that predate it carry it as `Unknown` instead of
    /// failing to decode the event; they do not show it as a change request.
    pub const COMMENT_CHECK_FAILURE: u16 = 27;
    /// `PolicySet` as written once it carried the hlc of its event; tag 2 is the unbound one.
    pub const BOUND_POLICY_SET: u16 = 28;
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
            EventBody::BoundPolicySet(_) => "BoundPolicySet",
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
            EventBody::BoundPolicySet(body) => body.serialize(serializer),
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
            EventBody::BoundPolicySet(body) => (tag::BOUND_POLICY_SET, encoding::encode(body)?),
            EventBody::AgentDefined(body) => (tag::AGENT_DEFINED, encoding::encode(body)?),
            EventBody::AgentRemoved(body) => (tag::AGENT_REMOVED, encoding::encode(body)?),
            EventBody::InviteCreated(body) => (tag::INVITE_CREATED, encoding::encode(body)?),
            EventBody::DeviceBound(body) => (tag::DEVICE_BOUND, encoding::encode(body)?),
            EventBody::DeviceProfile(body) => (tag::DEVICE_PROFILE, encoding::encode(body)?),
            EventBody::ForkAnnounced(body) => (tag::FORK_ANNOUNCED, encoding::encode(body)?),
            EventBody::ItemCreated(body) => body.to_tagged()?,
            EventBody::ItemEdited(body) => (tag::ITEM_EDITED, encoding::encode(body)?),
            EventBody::ItemMoved(body) => (tag::ITEM_MOVED, encoding::encode(body)?),
            EventBody::ItemLinked(body) => (tag::ITEM_LINKED, encoding::encode(body)?),
            EventBody::ItemArchived(body) => (tag::ITEM_ARCHIVED, encoding::encode(body)?),
            EventBody::ItemUnarchived(body) => (tag::ITEM_UNARCHIVED, encoding::encode(body)?),
            EventBody::Commented(body) => body.to_tagged()?,
            EventBody::SessionStarted(body) => (tag::SESSION_STARTED, encoding::encode(body)?),
            EventBody::SessionEnded(body) => body.to_tagged()?,
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
            tag::BOUND_POLICY_SET => EventBody::BoundPolicySet(encoding::decode(payload)?),
            tag::AGENT_DEFINED => EventBody::AgentDefined(encoding::decode(payload)?),
            tag::AGENT_REMOVED => EventBody::AgentRemoved(encoding::decode(payload)?),
            tag::INVITE_CREATED => EventBody::InviteCreated(encoding::decode(payload)?),
            tag::DEVICE_BOUND => EventBody::DeviceBound(encoding::decode(payload)?),
            tag::DEVICE_PROFILE => EventBody::DeviceProfile(encoding::decode(payload)?),
            tag::FORK_ANNOUNCED => EventBody::ForkAnnounced(encoding::decode(payload)?),
            tag::ITEM_CREATED_WITHOUT_STATE => {
                EventBody::ItemCreated(ItemCreated::without_state(payload)?)
            }
            tag::ITEM_CREATED => EventBody::ItemCreated(ItemCreated::with_state(payload)?),
            tag::ITEM_EDITED => EventBody::ItemEdited(encoding::decode(payload)?),
            tag::ITEM_MOVED => EventBody::ItemMoved(encoding::decode(payload)?),
            tag::ITEM_LINKED => EventBody::ItemLinked(encoding::decode(payload)?),
            tag::ITEM_ARCHIVED => EventBody::ItemArchived(encoding::decode(payload)?),
            tag::ITEM_UNARCHIVED => EventBody::ItemUnarchived(encoding::decode(payload)?),
            tag::COMMENT => EventBody::Commented(Comment::without_lines(payload)?),
            tag::COMMENT_ON_LINES => EventBody::Commented(Comment::on_lines(payload)?),
            tag::COMMENT_CHECK_FAILURE => EventBody::Commented(Comment::check_failure(payload)?),
            tag::SESSION_STARTED => EventBody::SessionStarted(encoding::decode(payload)?),
            tag::SESSION_ENDED_WITHOUT_COMMENT => {
                EventBody::SessionEnded(SessionEnded::without_comment(payload)?)
            }
            tag::SESSION_ENDED => EventBody::SessionEnded(SessionEnded::with_comment(payload)?),
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
    /// The `hlc` the event will have, for statements that must name it.
    pub fn hlc(&self) -> Hlc {
        self.hlc
    }

    /// Gives the event the `hlc` a statement in its body already names. Admission refuses it when
    /// it does not come after the parents of the event.
    pub fn with_hlc(mut self, hlc: Hlc) -> Self {
        self.hlc = hlc;
        self
    }

    /// The first event of a new workforce.
    pub fn genesis(author: DeviceKey, actor: Actor, now_ms: u64) -> Self {
        Self {
            workforce: None,
            author,
            actor,
            seq: 1,
            prev: None,
            deps: Vec::new(),
            hlc: Hlc::next_after(Hlc::ZERO, now_ms.min(u64::MAX - 1))
                .expect("a clock below the maximum has a successor of ZERO"),
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
mod golden_bodies;

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
            lines: None,
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
                because: None,
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

    fn created_task(state: Option<WrittenState>) -> EventBody {
        EventBody::ItemCreated(ItemCreated {
            item_type: TypeName::new("task").unwrap(),
            title: "t".into(),
            body: String::new(),
            priority: Some(Priority::P2),
            lane: None,
            labels: vec![],
            requires: vec![],
            review: None,
            state,
        })
    }

    /// Bodies written before the states were fixed keep their bytes, so their ids and signatures
    /// hold.
    #[test]
    fn golden_bodies_from_before_the_fixed_states() {
        let created = created_task(None);
        let moved = EventBody::ItemMoved(ItemMoved {
            item: ItemId::from_bytes([1; 32]),
            to: WrittenState::as_written("in_progress"),
            by_session: None,
            because: None,
        });
        for (body, golden) in [created, moved].iter().zip(GOLDEN_OLD_ITEM_BODIES) {
            assert_eq!(golden_body_bytes(body), golden);
            let decoded: EventBody =
                encoding::decode(&data_encoding::HEXLOWER.decode(golden.as_bytes()).unwrap())
                    .unwrap();
            assert_eq!(&decoded, body);
        }
    }

    const GOLDEN_OLD_ITEM_BODIES: [&str; 2] = [
        "0a0e047461736b017400010200000000",
        "0c2f2001010101010101010101010101010101010101010101010101010101010101010b696e5f70726f67726573730000",
    ];

    #[test]
    fn an_item_created_in_a_state_round_trips_under_its_own_tag() {
        let created = created_task(Some(ItemState::Draft.into()));
        let bytes = encoding::encode(&created).unwrap();
        assert_eq!(bytes[0], tag::ITEM_CREATED as u8);
        assert_eq!(encoding::decode::<EventBody>(&bytes).unwrap(), created);
        let EventBody::ItemCreated(without) = created_task(None) else {
            unreachable!()
        };
        let forged = TaggedBody {
            tag: tag::ITEM_CREATED,
            payload: encoding::encode(&without).unwrap(),
        };
        assert!(encoding::decode::<EventBody>(&encoding::encode(&forged).unwrap()).is_err());
    }

    fn comment_on_lines() -> Comment {
        Comment {
            kind: CommentKind::ChangeRequest,
            lines: Some(LineAnchor {
                commit: CommitId::Git("00112233445566778899aabbccddeeff00112233".parse().unwrap()),
                path: "src/lib.rs".into(),
                side: DiffSide::New,
                first: 12,
                last: 13,
                quote: "+fn parse_lazily() {}\n fn main() {}".into(),
            }),
            ..Comment::plain(ItemId::from_bytes([1; 32]), "Keep the old name.")
        }
    }

    #[test]
    fn a_comment_on_lines_round_trips_under_its_own_tag() {
        let comment = comment_on_lines();
        let bytes = encoding::encode(&EventBody::Commented(comment.clone())).unwrap();
        assert_eq!(bytes[0], tag::COMMENT_ON_LINES as u8);
        assert_eq!(
            encoding::decode::<EventBody>(&bytes).unwrap(),
            EventBody::Commented(comment.clone())
        );
        let forged = TaggedBody {
            tag: tag::COMMENT_ON_LINES,
            payload: encoding::encode(&Comment {
                lines: None,
                ..comment
            })
            .unwrap(),
        };
        assert!(encoding::decode::<EventBody>(&encoding::encode(&forged).unwrap()).is_err());
    }

    #[test]
    fn a_check_failure_round_trips_under_its_own_tag_that_older_peers_carry_unknown() {
        let failure = Comment {
            kind: CommentKind::CheckFailure,
            ..Comment::plain(ItemId::from_bytes([1; 32]), "cargo test failed")
        };
        let body = EventBody::Commented(failure.clone());
        let bytes = encoding::encode(&body).unwrap();
        assert_eq!(bytes[0], tag::COMMENT_CHECK_FAILURE as u8);
        assert_eq!(encoding::decode::<EventBody>(&bytes).unwrap(), body);
        // What a peer that predates the tag does with it: carry it and write it back unchanged.
        let tagged: TaggedBody = encoding::decode(&bytes).unwrap();
        let carried = EventBody::Unknown {
            tag: tagged.tag,
            payload: tagged.payload,
        };
        assert_eq!(encoding::encode(&carried).unwrap(), bytes);
        // The kind belongs to the tag: under the others it would re-encode to other bytes.
        for other in [tag::COMMENT, tag::COMMENT_ON_LINES] {
            let forged = TaggedBody {
                tag: other,
                payload: encoding::encode(&failure).unwrap(),
            };
            assert!(encoding::decode::<EventBody>(&encoding::encode(&forged).unwrap()).is_err());
        }
        let forged = TaggedBody {
            tag: tag::COMMENT_CHECK_FAILURE,
            payload: encoding::encode(&Comment::plain(failure.item, "x")).unwrap(),
        };
        assert!(encoding::decode::<EventBody>(&encoding::encode(&forged).unwrap()).is_err());
    }

    #[test]
    fn a_comment_on_lines_reads_with_where_they_are_and_the_lines_above_it() {
        assert_eq!(
            comment_on_lines().text_with_lines(),
            "src/lib.rs:12-13 at 00112233:\n    +fn parse_lazily() {}\n     fn main() {}\n\nKeep the old name."
        );
    }

    #[test]
    fn a_session_end_with_a_comment_round_trips_under_its_own_tag() {
        let ended = SessionEnded {
            session: SessionId::from_bytes([2; 32]),
            reason: EndReason::Released,
            because: Some(CommentId::from_bytes([3; 32])),
        };
        let bytes = encoding::encode(&EventBody::SessionEnded(ended.clone())).unwrap();
        assert_eq!(bytes[0], tag::SESSION_ENDED as u8);
        assert_eq!(
            encoding::decode::<EventBody>(&bytes).unwrap(),
            EventBody::SessionEnded(ended.clone())
        );
        let forged = TaggedBody {
            tag: tag::SESSION_ENDED,
            payload: encoding::encode(&SessionEnded {
                because: None,
                ..ended
            })
            .unwrap(),
        };
        assert!(encoding::decode::<EventBody>(&encoding::encode(&forged).unwrap()).is_err());
    }

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
        event.content.hlc = Hlc::next_after(event.content.hlc, 5_000).unwrap();
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

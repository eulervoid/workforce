//! Admission rules.
//!
//! [`State::receive`] enforces what can be decided from an event's causal past alone: signatures,
//! feed continuity, authorship and authority. Rules that depend on the current policy or on the
//! state of an item (`check_policy`) are applied by the author before signing. Peers do not
//! re-check them, because a concurrent policy change would make acceptance depend on arrival
//! order; the fold keeps such events, and an item can end up in a state its type no longer uses.

use std::collections::{BTreeSet, VecDeque};

use serde::Serialize;

use crate::body::{Body, BodyError, StepId};
use crate::event::{
    AgentDefined, Change, CommentKind, EVENT_VERSION, Event, EventBody, Genesis, ItemCreated,
    ItemEdited, ItemLinked, ItemMoved, Link,
};
use crate::hlc::HLC_TOLERANCE_MS;
use crate::id::{CommentId, EventId, ItemId, SessionId, WorkforceId};
use crate::identity::{Actor, AgentId, DeviceKey, PrincipalKey, Role, Signed};
use crate::item::ItemState;
use crate::name::{LaneName, RecoveryLevelName, ReviewLevelName, TypeName};
use crate::policy::Field;
use crate::session::{SessionRole, Verdict};
use crate::state::{
    Disposition, ForkState, MAX_PENDING, MAX_PENDING_BINDINGS, Origin, PendingOn, Received, State,
    Trust,
};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GenesisError {
    #[error("the workforce needs a name")]
    EmptyName,
    #[error("the signing device is not a device of the founder")]
    AuthorNotFounderDevice,
    #[error("the actor is not the founder")]
    ActorNotFounder,
    #[error("the genesis statement is not signed by the founder")]
    NotSignedByFounder,
    #[error("principal {0} is seeded twice")]
    DuplicatePrincipal(PrincipalKey),
    #[error("handle `{0}` is used twice")]
    DuplicateHandle(String),
    #[error("device {0} is bound to two principals")]
    DuplicateDevice(DeviceKey),
    #[error("a fork names the heads of the workforce it was forked from")]
    EmptyForkPoint,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Invalid {
    #[error("unsupported event version {0}")]
    UnsupportedVersion(u16),
    #[error("event signature is invalid")]
    BadSignature,
    #[error("genesis must not name a workforce")]
    GenesisNamesWorkforce,
    #[error("event does not name a workforce")]
    MissingWorkforce,
    #[error("event belongs to another workforce")]
    WrongWorkforce(WorkforceId),
    #[error("this workforce already has a genesis")]
    DuplicateGenesis,
    #[error("an event of {0} that is not part of the history this fork was made from")]
    NotInForkHistory(WorkforceId),
    #[error("invalid genesis")]
    Genesis(#[from] GenesisError),
    #[error("seq {got} from {author} is not valid here, expected {expected}")]
    UnexpectedSeq {
        author: DeviceKey,
        expected: u64,
        got: u64,
    },
    #[error("`prev` is not the event of {author} with the previous seq")]
    PrevMismatch { author: DeviceKey },
    #[error("hlc is not greater than the hlc of the events it follows")]
    HlcNotAfterParents,
    #[error("hlc has a wall time or counter at its maximum")]
    HlcAtMaximum,
    #[error("hlc is more than {tolerance_ms} ms ahead of this device's clock; it is refused for now and accepted when offered again once the clock has caught up", tolerance_ms = HLC_TOLERANCE_MS)]
    HlcInFuture,
    #[error("device {0} is not bound to a member")]
    UnknownDevice(DeviceKey),
    #[error("the actor is not the principal of the signing device")]
    ActorMismatch,
    #[error("the acting agent is not defined")]
    UnknownAgent,
    #[error("{0} is not a member")]
    NotAMember(PrincipalKey),
    #[error("a device is bound by the first event of its own feed")]
    BindingNotFirst,
    #[error("the invite is not among the parents of the binding")]
    InviteNotInParents,
    #[error("{0} is not an invite")]
    UnknownInvite(EventId),
    #[error("the proof does not come from the invite key for this device")]
    BadInviteProof,
    #[error("{0} is not an owner")]
    NotOwner(PrincipalKey),
    #[error("the policy was signed for another event: its hlc is not the hlc of the event")]
    PolicyNotForThisEvent,
    #[error("principal signature is invalid")]
    BadPrincipalSignature,
    #[error("unknown item {0}")]
    UnknownItem(ItemId),
    #[error("unknown session {0}")]
    UnknownSession(SessionId),
    #[error("session {0} is not a session of this agent on this device")]
    SessionMismatch(SessionId),
    #[error("an item cannot depend on itself")]
    SelfDependency,
    #[error("{0} is not a comment of the item in the causal past of the event")]
    ResolvesOutsideCausalPast(CommentId),
    #[error("too many events are waiting for missing parents")]
    PendingFull,
}

/// The names that would have been accepted, for an error that names one that is not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OneOf(Vec<String>);

impl OneOf {
    pub fn new<Name: std::fmt::Display>(names: impl IntoIterator<Item = Name>) -> Self {
        Self(names.into_iter().map(|name| name.to_string()).collect())
    }
}

impl std::fmt::Display for OneOf {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0.is_empty() {
            return formatter.write_str("there are none");
        }
        let names: Vec<String> = self.0.iter().map(|name| format!("`{name}`")).collect();
        write!(formatter, "valid: {}", names.join(", "))
    }
}

/// A rule the author must satisfy before signing, checked against the author's current view.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PolicyViolation {
    #[error("unknown item {0}")]
    UnknownItem(ItemId),
    #[error("there is no item type `{name}`; {valid}")]
    UnknownType { name: TypeName, valid: OneOf },
    #[error("a `{type_name}` is never `{state}`; {valid}")]
    StateNotUsed {
        type_name: TypeName,
        state: ItemState,
        valid: OneOf,
    },
    #[error("a `{type_name}` is created `{initial}`{or_draft}, not `{state}`")]
    CreatedInState {
        type_name: TypeName,
        state: ItemState,
        initial: ItemState,
        or_draft: &'static str,
    },
    #[error("type `{type_name}` does not have the `{field:?}` field")]
    FieldNotAllowed { type_name: TypeName, field: Field },
    #[error("a `{type_name}` needs a priority")]
    PriorityRequired { type_name: TypeName },
    #[error("a `{type_name}` cannot depend on a `{target}`")]
    DependencyNotAllowed {
        type_name: TypeName,
        target: TypeName,
    },
    #[error("{0} cannot be linked to itself")]
    LinkToSelf(ItemId),
    #[error("{from} cannot depend on {to}: that would make a cycle")]
    DependencyCycle { from: ItemId, to: ItemId },
    #[error("only done or cancelled items can be archived, not a `{state}` one")]
    ArchiveRequiresTerminal { state: ItemState },
    #[error("{0} is already archived")]
    AlreadyArchived(ItemId),
    #[error("{0} is not archived")]
    NotArchived(ItemId),
    #[error("only a decision can supersede another decision")]
    SupersedesRequiresDecisions,
    #[error("supersedes cannot be removed")]
    CannotRemoveSupersedes,
    #[error("there is no lane `{name}`; {valid}")]
    UnknownLane { name: LaneName, valid: OneOf },
    #[error(transparent)]
    Body(#[from] BodyError),
    #[error("the item has no step {0}")]
    UnknownStep(StepId),
    #[error("unknown session {0}")]
    UnknownSession(SessionId),
    #[error("session {0} is not live on this device")]
    SessionNotLive(SessionId),
    #[error("an agent cannot write {0}")]
    AgentCannotWrite(&'static str),
    #[error("a session cannot end another session")]
    SessionEndsAnother,
    #[error("a session cannot move its own item to `done`; it submits the work instead")]
    SessionFinishesItsItem,
    #[error(
        "only a review session of the commit, by an agent that did not do the work, can review as an agent"
    )]
    NotTheReviewer,
    #[error("only an agent can escalate a review")]
    PrincipalCannotEscalate,
    #[error("{0} was not submitted at that commit, so a verdict on it counts for nothing")]
    StaleReview(ItemId),
    #[error("there is no review level `{name}`; {valid}")]
    UnknownReviewLevel { name: ReviewLevelName, valid: OneOf },
    #[error("a `{type_name}` is never `review`, so it cannot wait for a review")]
    ReviewWithoutReviewState { type_name: TypeName },
    #[error("an agent that worked on {0} cannot lower its review level")]
    AgentLowersReview(ItemId),
    #[error("there is no recovery level `{name}`; {valid}")]
    UnknownRecoveryLevel {
        name: RecoveryLevelName,
        valid: OneOf,
    },
    #[error(
        "an agent cannot change the recovery level of {0}; the principal decides how much is retried"
    )]
    AgentChangesRecovery(ItemId),
    #[error("a review session comments on the item, not on its steps")]
    StepInReviewComment,
    #[error(
        "{0} has open change requests; resolve each with `wf comment <item> \"...\" --resolve <comment>` first"
    )]
    OpenChangeRequests(ItemId),
    #[error(
        "requesting changes on {0} needs at least one open change request: write each with `wf comment <item> \"...\" --request-change`"
    )]
    NoChangeRequests(ItemId),
    #[error("{0} is not a comment of this item")]
    UnknownComment(CommentId),
    #[error("an item needs a title")]
    EmptyTitle,
    #[error("a comment needs text unless it resolves something")]
    EmptyComment,
    #[error("lines {first} to {last} are no lines: they count from 1, the last after the first")]
    NoLines { first: u32, last: u32 },
    #[error("{0} is not a blocker or change request of this item")]
    NotResolvable(CommentId),
    #[error("{0} is resolved already")]
    AlreadyResolved(CommentId),
    #[error("a session cannot resolve a blocker it wrote; someone else has to")]
    SessionResolvesItsOwnBlocker,
}

/// Whether the item has an open comment of the kind.
fn has_open(item: &crate::item::Item, kind: CommentKind) -> bool {
    item.open_comments().iter().any(|(_, comment)| {
        comment.kind == kind || (kind.is_change_request() && comment.kind.is_change_request())
    })
}

enum Admission {
    Apply(Origin),
    Wait(PendingOn),
    Duplicate,
}

impl State {
    /// Validates and applies an event, or parks it until its parents arrive.
    ///
    /// Events may arrive in any order and any number of times. Errors are about the submitted
    /// event only; previously pending events that become invalid are reported in `rejected`.
    ///
    /// This checks no clock against the device's own, only that no `hlc` is at its maximum; replay
    /// from the store and tests use it. Events from peers go through [`State::receive_at`].
    pub fn receive(&mut self, event: Event, trust: Trust) -> Result<Received, Invalid> {
        self.receive_from(event, trust, u64::MAX)
    }

    /// Like [`State::receive`], and refuses events whose `hlc` is more than [`HLC_TOLERANCE_MS`]
    /// ahead of `now_ms`. Such an event is neither stored nor pending: nothing in the state wakes
    /// it when time passes, so it is offered again by sync once the clock has caught up.
    pub fn receive_at(
        &mut self,
        event: Event,
        trust: Trust,
        now_ms: u64,
    ) -> Result<Received, Invalid> {
        self.receive_from(event, trust, now_ms.saturating_add(HLC_TOLERANCE_MS))
    }

    fn receive_from(
        &mut self,
        event: Event,
        trust: Trust,
        latest_wall_ms: u64,
    ) -> Result<Received, Invalid> {
        let mut received = Received {
            disposition: Disposition::Duplicate,
            applied: Vec::new(),
            rejected: Vec::new(),
        };
        let submitted = event.id();
        if self.pending.events.contains_key(&submitted) {
            return Ok(received);
        }
        let mut queue = VecDeque::from([(event, trust)]);
        while let Some((event, trust)) = queue.pop_front() {
            let id = event.id();
            let is_submitted = id == submitted;
            match self.admit(&event, trust, latest_wall_ms) {
                Ok(Admission::Apply(origin)) => {
                    let is_genesis = origin == Origin::Own
                        && matches!(event.content.body, EventBody::Genesis(_));
                    self.apply(&event, id, origin);
                    if is_submitted {
                        received.disposition = Disposition::Applied;
                    }
                    received.applied.push(event);
                    // Pending events were verified when they first arrived.
                    queue.extend(
                        self.pending
                            .release(PendingOn::Event(id))
                            .into_iter()
                            .map(|released| (released, Trust::Stored)),
                    );
                    if is_genesis {
                        queue.extend(
                            self.pending
                                .release(PendingOn::Genesis)
                                .into_iter()
                                .map(|released| (released, Trust::Stored)),
                        );
                    }
                    if origin == Origin::History && !self.history_incomplete() {
                        queue.extend(
                            self.pending
                                .release(PendingOn::History)
                                .into_iter()
                                .map(|released| (released, Trust::Stored)),
                        );
                    }
                }
                Ok(Admission::Wait(waiting)) => {
                    if self.pending.events.len() >= MAX_PENDING {
                        if is_submitted {
                            return Err(Invalid::PendingFull);
                        }
                        received.rejected.push((id, Invalid::PendingFull));
                        continue;
                    }
                    let is_binding = matches!(event.content.body, EventBody::DeviceBound(_));
                    if is_binding && self.pending_bindings() >= MAX_PENDING_BINDINGS {
                        if is_submitted {
                            return Err(Invalid::PendingFull);
                        }
                        received.rejected.push((id, Invalid::PendingFull));
                        continue;
                    }
                    self.pending.insert(waiting, id, event);
                    if is_submitted {
                        received.disposition = Disposition::Pending;
                    }
                }
                Ok(Admission::Duplicate) => {}
                Err(error) if is_submitted => return Err(error),
                Err(error) => received.rejected.push((id, error)),
            }
        }
        Ok(received)
    }

    fn pending_bindings(&self) -> usize {
        self.pending
            .events
            .values()
            .filter(|pending| matches!(pending.content.body, EventBody::DeviceBound(_)))
            .count()
    }

    fn admit(
        &self,
        event: &Event,
        trust: Trust,
        latest_wall_ms: u64,
    ) -> Result<Admission, Invalid> {
        let content = &event.content;
        if self.known.contains_key(&event.id()) {
            return Ok(Admission::Duplicate);
        }
        if content.v != EVENT_VERSION {
            return Err(Invalid::UnsupportedVersion(content.v));
        }
        if trust == Trust::Untrusted && event.verify_signature().is_err() {
            return Err(Invalid::BadSignature);
        }
        // Before anything can park the event, so the descendants of a refused event are refused
        // too. Released events were checked when they arrived and are not judged by a later clock.
        if trust == Trust::Untrusted && event.content.hlc.wall_ms() > latest_wall_ms {
            return Err(Invalid::HlcInFuture);
        }

        let Some(workforce) = &self.workforce else {
            return match (&content.body, content.workforce) {
                (EventBody::Genesis(_), _) => {
                    self.admit_genesis(event, trust)?;
                    Ok(Admission::Apply(Origin::Own))
                }
                (_, None) => Err(Invalid::MissingWorkforce),
                (_, Some(id)) => match self.expected {
                    Some(expected) if expected != id => Err(Invalid::WrongWorkforce(id)),
                    Some(_) | None => Ok(Admission::Wait(PendingOn::Genesis)),
                },
            };
        };

        // The ids of the history of a fork are named by the genesis and the events already taken
        // in, and an event is its own proof that it is the one named: there is nothing else to
        // check, and the rules of the parent were applied when it was written.
        if workforce
            .fork
            .as_ref()
            .and_then(ForkState::wanted)
            .is_some_and(|wanted| wanted.contains(&event.id()))
        {
            Self::check_feed_shape(event)?;
            if !content.hlc.is_admissible() {
                return Err(Invalid::HlcAtMaximum);
            }
            return Ok(Admission::Apply(Origin::History));
        }
        if matches!(content.body, EventBody::Genesis(_)) {
            return Err(Invalid::DuplicateGenesis);
        }
        match content.workforce {
            None => return Err(Invalid::MissingWorkforce),
            Some(id) if id != workforce.id => {
                return Err(match &workforce.fork {
                    Some(fork) if fork.parent() == id => Invalid::NotInForkHistory(id),
                    _ => Invalid::WrongWorkforce(id),
                });
            }
            Some(_) => {}
        }

        // What the devices of a fork write waits until the history it continues has been folded:
        // their feeds and the items they touch are in it.
        if self.history_incomplete() {
            self.check_may_wait_for_history(event)?;
            return Ok(Admission::Wait(PendingOn::History));
        }

        if let Some(missing) = event
            .causal_parents()
            .find(|parent| !self.known.contains_key(parent))
        {
            return Ok(Admission::Wait(PendingOn::Event(missing)));
        }

        self.check_feed(event)?;
        self.check_hlc(event)?;
        match &content.body {
            EventBody::DeviceBound(_) => self.check_binding(event)?,
            EventBody::Genesis(_)
            | EventBody::PolicySet(_)
            | EventBody::BoundPolicySet(_)
            | EventBody::AgentDefined(_)
            | EventBody::AgentRemoved(_)
            | EventBody::InviteCreated(_)
            | EventBody::DeviceProfile(_)
            | EventBody::ForkAnnounced(_)
            | EventBody::ItemCreated(_)
            | EventBody::ItemEdited(_)
            | EventBody::ItemMoved(_)
            | EventBody::ItemLinked(_)
            | EventBody::ItemArchived(_)
            | EventBody::ItemUnarchived(_)
            | EventBody::Commented(_)
            | EventBody::SessionStarted(_)
            | EventBody::SessionEnded(_)
            | EventBody::Reviewed(_)
            | EventBody::Merged(_)
            | EventBody::Unknown { .. } => self.check_author(event)?,
        }
        self.check_body(event, trust)?;
        Ok(Admission::Apply(Origin::Own))
    }

    /// What can be checked about the feed position of an event without any other event: `seq`
    /// counts from 1 and `prev` is empty exactly at seq 1. The history of a fork gets no more.
    fn check_feed_shape(event: &Event) -> Result<(), Invalid> {
        let content = &event.content;
        if content.seq == 0 {
            return Err(Invalid::UnexpectedSeq {
                author: content.author,
                expected: 1,
                got: 0,
            });
        }
        if content.prev.is_none() != (content.seq == 1) {
            return Err(Invalid::PrevMismatch {
                author: content.author,
            });
        }
        Ok(())
    }

    /// What is known about a device without the history of a fork, so that unbound keys cannot
    /// fill the pending events: a bound device writes as its principal, and a new device starts
    /// with its binding.
    fn check_may_wait_for_history(&self, event: &Event) -> Result<(), Invalid> {
        let content = &event.content;
        if matches!(content.body, EventBody::DeviceBound(_)) {
            return if content.seq == 1 {
                Ok(())
            } else {
                Err(Invalid::BindingNotFirst)
            };
        }
        let principal = self
            .principal_of(&content.author)
            .ok_or(Invalid::UnknownDevice(content.author))?;
        if content.actor.principal() != principal {
            return Err(Invalid::ActorMismatch);
        }
        Ok(())
    }

    /// `prev` is empty exactly at seq 1, and otherwise an event of the same author with the
    /// previous seq. Nothing else about the feed is checked: equivocated feeds are admitted.
    fn check_feed(&self, event: &Event) -> Result<(), Invalid> {
        Self::check_feed_shape(event)?;
        let content = &event.content;
        let author = content.author;
        let prev_is_valid = match content.prev {
            None => true,
            Some(prev) => self
                .known
                .get(&prev)
                .is_some_and(|known| known.author == author && known.seq + 1 == content.seq),
        };
        if prev_is_valid {
            Ok(())
        } else {
            Err(Invalid::PrevMismatch { author })
        }
    }

    fn check_hlc(&self, event: &Event) -> Result<(), Invalid> {
        let hlc = event.content.hlc;
        if !hlc.is_admissible() {
            return Err(Invalid::HlcAtMaximum);
        }
        let follows_all = event
            .causal_parents()
            .all(|parent| self.known[&parent].hlc < event.content.hlc);
        if follows_all {
            Ok(())
        } else {
            Err(Invalid::HlcNotAfterParents)
        }
    }

    fn check_author(&self, event: &Event) -> Result<(), Invalid> {
        let content = &event.content;
        let principal = self
            .principal_of(&content.author)
            .ok_or(Invalid::UnknownDevice(content.author))?;
        if content.actor.principal() != principal {
            return Err(Invalid::ActorMismatch);
        }
        if let Actor::Agent { agent, session } = &content.actor {
            let workforce = self.workforce.as_ref().expect("checked by the caller");
            if !workforce.agents.contains_key(agent) {
                return Err(Invalid::UnknownAgent);
            }
            match workforce.sessions.get(session) {
                Some(folded) if folded.agent() == agent && folded.device() == content.author => {}
                Some(_) => return Err(Invalid::SessionMismatch(*session)),
                None => return Err(Invalid::UnknownSession(*session)),
            }
        }
        Ok(())
    }

    /// A `DeviceBound` is the author's first event, follows the invite it redeems and proves with
    /// the invite key that this very device is meant. The author is not bound yet, so the usual
    /// author check does not apply.
    fn check_binding(&self, event: &Event) -> Result<(), Invalid> {
        let content = &event.content;
        let EventBody::DeviceBound(bound) = &content.body else {
            unreachable!("the caller matched on a binding")
        };
        let workforce = self.workforce.as_ref().expect("checked by the caller");
        if content.seq != 1 {
            return Err(Invalid::BindingNotFirst);
        }
        if !content.deps.contains(&bound.invite) {
            return Err(Invalid::InviteNotInParents);
        }
        let invite = workforce
            .invites
            .get(&bound.invite)
            .ok_or(Invalid::UnknownInvite(bound.invite))?;
        invite
            .key
            .verify_proof(workforce.id, bound.invite, content.author, &bound.proof)
            .map_err(|_| Invalid::BadInviteProof)?;
        if content.actor != Actor::Principal(invite.principal) {
            return Err(Invalid::ActorMismatch);
        }
        Ok(())
    }

    fn check_body(&self, event: &Event, trust: Trust) -> Result<(), Invalid> {
        let workforce = self.workforce.as_ref().expect("checked by the caller");
        let known_item = |item: &ItemId| {
            if workforce.items.contains_key(item) {
                Ok(())
            } else {
                Err(Invalid::UnknownItem(*item))
            }
        };
        match &event.content.body {
            EventBody::Genesis(_) => Err(Invalid::DuplicateGenesis),
            EventBody::PolicySet(signed) => {
                self.check_signed_by_actor(signed, event, trust, Role::Owner)?;
                // Not bound to its event, so it could be an old policy written into a new
                // event; the fold lets any bound policy beat it.
                Ok(())
            }
            EventBody::BoundPolicySet(signed) => {
                self.check_signed_by_actor(signed, event, trust, Role::Owner)?;
                if signed.statement.hlc != event.content.hlc {
                    return Err(Invalid::PolicyNotForThisEvent);
                }
                // A policy this version cannot read is still admitted: a newer version may have
                // written it. The workforce keeps running on the last policy it could read.
                Ok(())
            }
            EventBody::InviteCreated(signed) => {
                self.check_signed_by_actor(signed, event, trust, Role::Member)
            }
            EventBody::ForkAnnounced(signed) => {
                self.check_signed_by_actor(signed, event, trust, Role::Owner)
            }
            EventBody::AgentDefined(AgentDefined { .. })
            | EventBody::AgentRemoved(_)
            | EventBody::DeviceBound(_)
            | EventBody::DeviceProfile(_)
            | EventBody::Unknown { .. } => Ok(()),
            EventBody::ItemCreated(ItemCreated { .. }) => Ok(()),
            EventBody::SessionStarted(started) => {
                let agent = AgentId {
                    principal: workforce.canonical(&event.content.actor.principal()),
                    name: started.agent.clone(),
                };
                if !workforce.agents.contains_key(&agent) {
                    return Err(Invalid::UnknownAgent);
                }
                known_item(&started.item)
            }
            EventBody::SessionEnded(ended) => {
                if workforce.sessions.contains_key(&ended.session) {
                    Ok(())
                } else {
                    Err(Invalid::UnknownSession(ended.session))
                }
            }
            EventBody::Reviewed(crate::event::Review { item, .. })
            | EventBody::Merged(crate::event::Merged { item, .. }) => known_item(item),
            EventBody::ItemEdited(ItemEdited { item, .. })
            | EventBody::ItemMoved(ItemMoved { item, .. })
            | EventBody::ItemArchived(crate::event::ItemRef { item })
            | EventBody::ItemUnarchived(crate::event::ItemRef { item }) => known_item(item),
            EventBody::Commented(comment) => {
                known_item(&comment.item)?;
                // Whether the comment is already folded here must not decide whether it resolves:
                // only what the author had seen does.
                comment.resolves.iter().try_for_each(|resolved| {
                    if self.in_causal_past(event, EventId::from(*resolved)) {
                        Ok(())
                    } else {
                        Err(Invalid::ResolvesOutsideCausalPast(*resolved))
                    }
                })
            }
            EventBody::ItemLinked(ItemLinked { item, link }) => {
                known_item(item)?;
                let target = match link {
                    Link::DependsOn(crate::crdt::SetChange::Add(target)) => target,
                    Link::DependsOn(crate::crdt::SetChange::Remove { value, .. }) => value,
                    Link::Supersedes(target) => target,
                };
                known_item(target)?;
                if target == item {
                    return Err(Invalid::SelfDependency);
                }
                Ok(())
            }
        }
    }

    /// A statement signed by a principal who holds at least `role` and is the actor of the event
    /// that carries it, for this workforce.
    fn check_signed_by_actor<T: Serialize>(
        &self,
        signed: &Signed<T>,
        event: &Event,
        trust: Trust,
        role: Role,
    ) -> Result<(), Invalid> {
        let workforce = self.workforce.as_ref().expect("checked by the caller");
        let member = workforce
            .members
            .get(&signed.signer)
            .ok_or(Invalid::NotAMember(signed.signer))?;
        // `Owner` sorts before `Member`.
        if member.role > role {
            return Err(Invalid::NotOwner(signed.signer));
        }
        if signed.signer != event.content.actor.principal() {
            return Err(Invalid::ActorMismatch);
        }
        if trust == Trust::Untrusted && signed.verify(Some(workforce.id)).is_err() {
            return Err(Invalid::BadPrincipalSignature);
        }
        Ok(())
    }

    /// Whether `target` is one of the causal parents of the event or one of their ancestors.
    fn in_causal_past(&self, event: &Event, target: EventId) -> bool {
        let Some(floor) = self.known.get(&target).map(|known| known.hlc) else {
            return false;
        };
        let mut seen = BTreeSet::new();
        let mut pending: Vec<EventId> = event.causal_parents().collect();
        while let Some(id) = pending.pop() {
            if id == target {
                return true;
            }
            // An ancestor has a smaller hlc than the events after it.
            if let Some(known) = self.known.get(&id)
                && known.hlc > floor
                && seen.insert(id)
            {
                pending.extend(known.parents.iter().copied());
            }
        }
        false
    }

    fn admit_genesis(&self, event: &Event, trust: Trust) -> Result<(), Invalid> {
        let content = &event.content;
        if content.workforce.is_some() {
            return Err(Invalid::GenesisNamesWorkforce);
        }
        let id = WorkforceId::from(event.id());
        if self.expected.is_some_and(|expected| expected != id) {
            return Err(Invalid::WrongWorkforce(id));
        }
        let EventBody::Genesis(signed) = &content.body else {
            unreachable!("the caller matched on a genesis body")
        };
        let genesis: &Genesis = &signed.statement;
        // The genesis of a fork continues the feed its device had in the parent workforce.
        let continues_a_feed = genesis.forked_from.is_some() && content.seq > 1;
        if content.seq == 0
            || content.prev.is_some() != (content.seq > 1)
            || (content.seq > 1 && !continues_a_feed)
            || !content.deps.is_empty()
        {
            return Err(Invalid::UnexpectedSeq {
                author: content.author,
                expected: 1,
                got: content.seq,
            });
        }
        if genesis.name.trim().is_empty() {
            return Err(GenesisError::EmptyName.into());
        }
        if genesis
            .forked_from
            .as_ref()
            .is_some_and(|fork| fork.heads.is_empty())
        {
            return Err(GenesisError::EmptyForkPoint.into());
        }
        if signed.signer != genesis.founder.principal {
            return Err(GenesisError::NotSignedByFounder.into());
        }
        if trust == Trust::Untrusted && signed.verify(None).is_err() {
            return Err(Invalid::BadPrincipalSignature);
        }
        if !genesis.founder.devices.contains(&content.author) {
            return Err(GenesisError::AuthorNotFounderDevice.into());
        }
        if content.actor != Actor::Principal(genesis.founder.principal) {
            return Err(GenesisError::ActorNotFounder.into());
        }
        let mut principals = std::collections::BTreeSet::new();
        let mut handles = std::collections::BTreeSet::new();
        let mut devices = std::collections::BTreeSet::new();
        for seed in std::iter::once(&genesis.founder).chain(&genesis.members) {
            if !principals.insert(seed.principal) {
                return Err(GenesisError::DuplicatePrincipal(seed.principal).into());
            }
            if !handles.insert(seed.handle.clone()) {
                return Err(GenesisError::DuplicateHandle(seed.handle.to_string()).into());
            }
            for device in &seed.devices {
                if !devices.insert(*device) {
                    return Err(GenesisError::DuplicateDevice(*device).into());
                }
            }
        }
        Ok(())
    }

    /// Checks what the acting agent may do, if the actor is one. Run by the author before signing,
    /// together with [`State::check_policy`]. These are guards against mistakes and shortcuts,
    /// not against members, who are trusted and whose keys are on disk. The daemon decides the
    /// actor from the connection, so for an agent in its sandbox they are the last word.
    pub fn check_actor(
        &self,
        author: DeviceKey,
        actor: &Actor,
        body: &EventBody,
    ) -> Result<(), PolicyViolation> {
        let Some(workforce) = &self.workforce else {
            return Ok(());
        };
        let Actor::Agent { session, .. } = actor else {
            return match body {
                EventBody::Reviewed(review) if review.verdict == Verdict::Escalate => {
                    Err(PolicyViolation::PrincipalCannotEscalate)
                }
                EventBody::Genesis(_)
                | EventBody::PolicySet(_)
                | EventBody::BoundPolicySet(_)
                | EventBody::AgentDefined(_)
                | EventBody::AgentRemoved(_)
                | EventBody::InviteCreated(_)
                | EventBody::DeviceBound(_)
                | EventBody::DeviceProfile(_)
                | EventBody::ForkAnnounced(_)
                | EventBody::ItemCreated(_)
                | EventBody::ItemEdited(_)
                | EventBody::ItemMoved(_)
                | EventBody::ItemLinked(_)
                | EventBody::ItemArchived(_)
                | EventBody::ItemUnarchived(_)
                | EventBody::Commented(_)
                | EventBody::SessionStarted(_)
                | EventBody::SessionEnded(_)
                | EventBody::Reviewed(_)
                | EventBody::Merged(_)
                | EventBody::Unknown { .. } => Ok(()),
            };
        };
        let folded = workforce
            .sessions
            .get(session)
            .ok_or(PolicyViolation::UnknownSession(*session))?;
        if !folded.is_live() || folded.device() != author {
            return Err(PolicyViolation::SessionNotLive(*session));
        }
        match body {
            EventBody::Genesis(_)
            | EventBody::PolicySet(_)
            | EventBody::BoundPolicySet(_)
            | EventBody::AgentDefined(_)
            | EventBody::AgentRemoved(_)
            | EventBody::InviteCreated(_)
            | EventBody::DeviceBound(_)
            | EventBody::ForkAnnounced(_)
            | EventBody::SessionStarted(_)
            | EventBody::Merged(_) => Err(PolicyViolation::AgentCannotWrite(body.name())),
            EventBody::SessionEnded(ended) if ended.session != *session => {
                Err(PolicyViolation::SessionEndsAnother)
            }
            EventBody::Reviewed(review) => {
                let reviewing = folded.item() == review.item
                    && matches!(folded.role(), SessionRole::Review { commit } if commit == review.commit);
                let did_the_work = workforce.sessions.values().any(|other| {
                    other.agent() == folded.agent()
                        && other.item() == review.item
                        && other.role() == SessionRole::Work
                });
                if reviewing && !did_the_work {
                    Ok(())
                } else {
                    Err(PolicyViolation::NotTheReviewer)
                }
            }
            EventBody::ItemEdited(edited)
                if edited
                    .changes
                    .iter()
                    .any(|change| matches!(change, Change::RecoveryLevel(_))) =>
            {
                Err(PolicyViolation::AgentChangesRecovery(edited.item))
            }
            EventBody::ItemEdited(edited) => {
                let lowers = edited
                    .changes
                    .iter()
                    .any(|change| matches!(change, Change::ReviewLevel(_)))
                    && self.lowers_review(actor, edited);
                if lowers {
                    Err(PolicyViolation::AgentLowersReview(edited.item))
                } else {
                    Ok(())
                }
            }
            EventBody::ItemMoved(moved) => {
                if moved.to.state() == ItemState::Done && moved.item == folded.item() {
                    Err(PolicyViolation::SessionFinishesItsItem)
                } else {
                    Ok(())
                }
            }
            EventBody::Commented(comment) => {
                let own_blocker = workforce.items.get(&comment.item).is_some_and(|item| {
                    comment.resolves.iter().any(|resolved| {
                        item.comments().get(resolved).is_some_and(|target| {
                            target.kind == CommentKind::Blocker
                                && target.session() == Some(*session)
                        })
                    })
                });
                let on_a_step_in_review =
                    comment.step.is_some() && matches!(folded.role(), SessionRole::Review { .. });
                if own_blocker {
                    Err(PolicyViolation::SessionResolvesItsOwnBlocker)
                } else if on_a_step_in_review {
                    Err(PolicyViolation::StepInReviewComment)
                } else {
                    Ok(())
                }
            }
            EventBody::DeviceProfile(_)
            | EventBody::ItemCreated(_)
            | EventBody::ItemLinked(_)
            | EventBody::ItemArchived(_)
            | EventBody::ItemUnarchived(_)
            | EventBody::SessionEnded(_)
            | EventBody::Unknown { .. } => Ok(()),
        }
    }

    /// Whether an edit by an agent lowers the review level of an item the agent has worked on.
    /// Supervisors, agents in the `supervision` lane, may lower it.
    fn lowers_review(&self, actor: &Actor, edited: &ItemEdited) -> bool {
        let (Some(workforce), Actor::Agent { agent, .. }) = (&self.workforce, actor) else {
            return false;
        };
        let supervision = LaneName::new("supervision").expect("a valid lane name");
        let supervises = workforce
            .agents
            .get(agent)
            .and_then(|definition| definition.value().as_ref())
            .is_some_and(|definition| definition.lanes.contains(&supervision));
        let worked_on = workforce
            .sessions
            .values()
            .any(|session| session.agent() == agent && session.item() == edited.item);
        let Some(item) = workforce.items.get(&edited.item) else {
            return false;
        };
        if supervises || !worked_on {
            return false;
        }
        let policy = &workforce.running_policy().policy;
        // `None` ranks below every level, so removing the review lowers it.
        let rank = |level: Option<&ReviewLevelName>| {
            level.and_then(|name| policy.review_rank(name).map(|rank| rank + 1))
        };
        let before = rank(item.effective_review_level(policy));
        let type_default = policy
            .type_config(item.item_type())
            .and_then(|config| config.review.as_ref());
        edited
            .changes
            .iter()
            .filter_map(|change| {
                let Change::ReviewLevel(review) = change else {
                    return None;
                };
                Some(review.as_ref().or(type_default))
            })
            .any(|after| rank(after).unwrap_or(0) < before.unwrap_or(0))
    }

    /// Checks the rules that depend on the current policy and item states. Run by the author
    /// before signing; see the module documentation for why peers do not repeat it.
    pub fn check_policy(&self, body: &EventBody) -> Result<(), PolicyViolation> {
        let Some(workforce) = &self.workforce else {
            return Ok(());
        };
        let policy = &workforce.running_policy().policy;
        let known_lane = |lane: &LaneName| {
            if policy.lanes.contains_key(lane) {
                Ok(())
            } else {
                Err(PolicyViolation::UnknownLane {
                    name: lane.clone(),
                    valid: OneOf::new(policy.lanes.keys()),
                })
            }
        };
        let item_of = |id: &ItemId| {
            workforce
                .items
                .get(id)
                .ok_or(PolicyViolation::UnknownItem(*id))
        };
        let type_of = |type_name: &TypeName| {
            policy
                .type_config(type_name)
                .ok_or_else(|| PolicyViolation::UnknownType {
                    name: type_name.clone(),
                    valid: OneOf::new(policy.types.keys()),
                })
        };
        let known_review = |type_name: &TypeName, review: &Option<ReviewLevelName>| {
            let Some(level) = review else {
                return Ok(());
            };
            if policy.review_level(level).is_none() {
                return Err(PolicyViolation::UnknownReviewLevel {
                    name: level.clone(),
                    valid: OneOf::new(policy.review.iter().map(|level| &level.name)),
                });
            }
            let reviewable = type_of(type_name)?.uses(ItemState::Review);
            if reviewable {
                Ok(())
            } else {
                Err(PolicyViolation::ReviewWithoutReviewState {
                    type_name: type_name.clone(),
                })
            }
        };
        let because_on = |item: &crate::item::Item, because: Option<CommentId>| match because {
            Some(comment) if !item.comments().contains_key(&comment) => {
                Err(PolicyViolation::UnknownComment(comment))
            }
            Some(_) | None => Ok(()),
        };
        let require_field = |type_name: &TypeName, field: Field| {
            if type_of(type_name)?.allows_field(field) {
                Ok(())
            } else {
                Err(PolicyViolation::FieldNotAllowed {
                    type_name: type_name.clone(),
                    field,
                })
            }
        };

        match body {
            EventBody::Genesis(_)
            | EventBody::PolicySet(_)
            | EventBody::BoundPolicySet(_)
            | EventBody::Unknown { .. } => Ok(()),
            EventBody::Commented(comment) => {
                if comment.text.trim().is_empty() && comment.resolves.is_empty() {
                    return Err(PolicyViolation::EmptyComment);
                }
                if let Some(lines) = &comment.lines
                    && (lines.first == 0 || lines.first > lines.last)
                {
                    return Err(PolicyViolation::NoLines {
                        first: lines.first,
                        last: lines.last,
                    });
                }
                let item = item_of(&comment.item)?;
                for resolved in &comment.resolves {
                    let target = item
                        .comments()
                        .get(resolved)
                        .filter(|target| target.kind != CommentKind::Plain)
                        .ok_or(PolicyViolation::NotResolvable(*resolved))?;
                    if target.resolved().is_some() {
                        return Err(PolicyViolation::AlreadyResolved(*resolved));
                    }
                }
                match comment.step {
                    Some(step) if item.body().step(step).is_none() => {
                        Err(PolicyViolation::UnknownStep(step))
                    }
                    Some(_) | None => Ok(()),
                }
            }
            EventBody::InviteCreated(_)
            | EventBody::DeviceBound(_)
            | EventBody::DeviceProfile(_)
            | EventBody::ForkAnnounced(_)
            | EventBody::SessionStarted(_)
            | EventBody::Merged(_) => Ok(()),
            EventBody::SessionEnded(ended) => {
                let submitted = matches!(ended.reason, crate::session::EndReason::Submitted { .. });
                let item = workforce
                    .sessions
                    .get(&ended.session)
                    .map(|session| session.item());
                match item.and_then(|item| workforce.items.get(&item)) {
                    Some(item) if submitted && has_open(item, CommentKind::ChangeRequest) => {
                        Err(PolicyViolation::OpenChangeRequests(item.id()))
                    }
                    Some(_) | None => Ok(()),
                }
            }
            EventBody::Reviewed(review) => {
                let item = item_of(&review.item)?;
                because_on(item, review.because)?;
                match self.submission(review.item) {
                    Some((commit, _)) if commit == review.commit => {}
                    Some(_) | None => return Err(PolicyViolation::StaleReview(review.item)),
                }
                let changes_open = has_open(item, CommentKind::ChangeRequest);
                match review.verdict {
                    Verdict::Approve if changes_open => {
                        Err(PolicyViolation::OpenChangeRequests(review.item))
                    }
                    Verdict::RequestChanges if !changes_open => {
                        Err(PolicyViolation::NoChangeRequests(review.item))
                    }
                    Verdict::Approve | Verdict::RequestChanges | Verdict::Escalate => Ok(()),
                }
            }
            EventBody::AgentDefined(defined) => defined.lanes.iter().try_for_each(known_lane),
            EventBody::AgentRemoved(_) => Ok(()),
            EventBody::ItemCreated(created) => {
                let type_name = &created.item_type;
                let config = type_of(type_name)?;
                if created.title.trim().is_empty() {
                    return Err(PolicyViolation::EmptyTitle);
                }
                if let Some(state) = created.state.as_ref().map(|written| written.state()) {
                    let draft = config.uses(ItemState::Draft);
                    if state != config.initial && !(draft && state == ItemState::Draft) {
                        return Err(PolicyViolation::CreatedInState {
                            type_name: type_name.clone(),
                            state,
                            initial: config.initial,
                            or_draft: if draft { " or as a `draft`" } else { "" },
                        });
                    }
                }
                match created.priority {
                    Some(_) => require_field(type_name, Field::Priority)?,
                    None if type_of(type_name)?.allows_field(Field::Priority) => {
                        return Err(PolicyViolation::PriorityRequired {
                            type_name: type_name.clone(),
                        });
                    }
                    None => {}
                }
                if !created.labels.is_empty() {
                    require_field(type_name, Field::Labels)?;
                }
                if !created.requires.is_empty() {
                    require_field(type_name, Field::Requires)?;
                }
                Body::parse(&created.body)?;
                known_review(type_name, &created.review)?;
                created.lane.iter().try_for_each(known_lane)
            }
            EventBody::ItemEdited(edited) => {
                let item = item_of(&edited.item)?;
                let type_name = item.item_type();
                for change in &edited.changes {
                    match change {
                        Change::Title(title) => {
                            if title.trim().is_empty() {
                                return Err(PolicyViolation::EmptyTitle);
                            }
                        }
                        Change::Body(text) => {
                            Body::parse(text)?;
                        }
                        Change::StepDone { step, .. } => {
                            if item.body().step(*step).is_none() {
                                return Err(PolicyViolation::UnknownStep(*step));
                            }
                        }
                        Change::Priority(None) => {
                            if type_of(type_name)?.allows_field(Field::Priority) {
                                return Err(PolicyViolation::PriorityRequired {
                                    type_name: type_name.clone(),
                                });
                            }
                        }
                        Change::Priority(Some(_)) => require_field(type_name, Field::Priority)?,
                        Change::Lane(lane) => lane.iter().try_for_each(known_lane)?,
                        Change::Labels(_) => require_field(type_name, Field::Labels)?,
                        Change::Requires(_) => require_field(type_name, Field::Requires)?,
                        Change::ReviewLevel(review) => known_review(type_name, review)?,
                        Change::RecoveryLevel(Some(level))
                            if policy.recovery_level(level).is_none() =>
                        {
                            return Err(PolicyViolation::UnknownRecoveryLevel {
                                name: level.clone(),
                                valid: OneOf::new(policy.recovery.iter().map(|level| &level.name)),
                            });
                        }
                        Change::RecoveryLevel(_) => {}
                    }
                }
                Ok(())
            }
            EventBody::ItemMoved(moved) => {
                let item = item_of(&moved.item)?;
                because_on(item, moved.because)?;
                let config = type_of(item.item_type())?;
                let state = moved.to.state();
                if config.uses(state) {
                    Ok(())
                } else {
                    Err(PolicyViolation::StateNotUsed {
                        type_name: item.item_type().clone(),
                        state,
                        valid: OneOf::new(&config.states),
                    })
                }
            }
            EventBody::ItemLinked(linked) => {
                let item = item_of(&linked.item)?;
                let target = match &linked.link {
                    Link::DependsOn(crate::crdt::SetChange::Add(target))
                    | Link::DependsOn(crate::crdt::SetChange::Remove { value: target, .. })
                    | Link::Supersedes(target) => target,
                };
                if *target == linked.item {
                    return Err(PolicyViolation::LinkToSelf(linked.item));
                }
                match &linked.link {
                    Link::DependsOn(crate::crdt::SetChange::Remove { .. }) => Ok(()),
                    Link::DependsOn(crate::crdt::SetChange::Add(target)) => {
                        let target_type = item_of(target)?.item_type();
                        if !type_of(item.item_type())?.allows_dependency_on(target_type) {
                            return Err(PolicyViolation::DependencyNotAllowed {
                                type_name: item.item_type().clone(),
                                target: target_type.clone(),
                            });
                        }
                        if self.dependency_graph().reaches(*target, linked.item) {
                            return Err(PolicyViolation::DependencyCycle {
                                from: linked.item,
                                to: *target,
                            });
                        }
                        Ok(())
                    }
                    Link::Supersedes(target) => {
                        let both_decisions = [item, item_of(target)?]
                            .iter()
                            .all(|candidate| candidate.item_type().as_str() == "decision");
                        if both_decisions {
                            Ok(())
                        } else {
                            Err(PolicyViolation::SupersedesRequiresDecisions)
                        }
                    }
                }
            }
            EventBody::ItemArchived(reference) => {
                let item = item_of(&reference.item)?;
                if item.archived_at().is_some() {
                    return Err(PolicyViolation::AlreadyArchived(reference.item));
                }
                let state = item.state(policy);
                if state.is_terminal() {
                    Ok(())
                } else {
                    Err(PolicyViolation::ArchiveRequiresTerminal { state })
                }
            }
            EventBody::ItemUnarchived(reference) => {
                if item_of(&reference.item)?.archived_at().is_some() {
                    Ok(())
                } else {
                    Err(PolicyViolation::NotArchived(reference.item))
                }
            }
        }
    }
}

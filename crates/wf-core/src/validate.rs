//! Admission rules.
//!
//! [`State::receive`] enforces what can be decided from an event's causal past alone: signatures,
//! feed continuity, authorship and authority. Rules that depend on the current policy or on the
//! state of an item (`check_policy`) are applied by the author before signing. Peers do not
//! re-check them, because a concurrent policy change would make acceptance depend on arrival
//! order; the fold keeps such events and flags the result instead ("unknown state").

use std::collections::VecDeque;

use crate::body::{Body, BodyError, StepId};
use crate::event::{
    AgentDefined, Change, EVENT_VERSION, Event, EventBody, Genesis, ItemCreated, ItemEdited,
    ItemLinked, ItemTransitioned, Link,
};
use crate::id::{EventId, ItemId, SessionId, WorkforceId};
use crate::identity::{Actor, AgentId, DeviceKey, PrincipalKey, Role};
use crate::name::{LaneName, ReviewName, StateName, TypeName};
use crate::policy::{Field, Policy, PolicyError, StateCategory};
use crate::session::SessionRole;
use crate::state::{Disposition, ForkState, MAX_PENDING, Origin, Received, State, Trust, Waiting};

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
    #[error("principal signature is invalid")]
    BadPrincipalSignature,
    #[error(transparent)]
    Policy(#[from] PolicyError),
    #[error("unknown item {0}")]
    UnknownItem(ItemId),
    #[error("unknown session {0}")]
    UnknownSession(SessionId),
    #[error("session {0} is not a session of this agent on this device")]
    SessionMismatch(SessionId),
    #[error("an item cannot depend on itself")]
    SelfDependency,
    #[error("too many events are waiting for missing parents")]
    PendingFull,
}

/// A rule the author must satisfy before signing, checked against the author's current view.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PolicyViolation {
    #[error("unknown item {0}")]
    UnknownItem(ItemId),
    #[error("there is no item type `{0}`")]
    UnknownType(TypeName),
    #[error("type `{type_name}` has no state `{state}`")]
    UnknownState {
        type_name: TypeName,
        state: StateName,
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
    #[error("only done or cancelled items can be archived")]
    ArchiveRequiresTerminal {
        state_category: Option<StateCategory>,
    },
    #[error("{0} is already archived")]
    AlreadyArchived(ItemId),
    #[error("{0} is not archived")]
    NotArchived(ItemId),
    #[error("only a decision can supersede another decision")]
    SupersedesRequiresDecisions,
    #[error("supersedes cannot be removed")]
    CannotRemoveSupersedes,
    #[error("there is no lane `{0}`")]
    UnknownLane(LaneName),
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
    #[error("a session cannot move its own item to a done state; it submits the work instead")]
    SessionFinishesItsItem,
    #[error("there is no review level `{0}`")]
    UnknownReviewLevel(ReviewName),
    #[error("a `{type_name}` has no blocked state to wait in during review")]
    ReviewWithoutBlockedState { type_name: TypeName },
    #[error("an agent that worked on {0} cannot lower its review level")]
    AgentLowersReview(ItemId),
    #[error("a report is written by its own session")]
    ReportNotFromItsSession,
    #[error("a review session reports on the item, not on its steps")]
    StepInReviewReport,
}

enum Admission {
    Apply(Origin),
    Wait(Waiting),
    Duplicate,
}

impl State {
    /// Validates and applies an event, or parks it until its parents arrive.
    ///
    /// Events may arrive in any order and any number of times. Errors are about the submitted
    /// event only; previously pending events that become invalid are reported in `rejected`.
    pub fn receive(&mut self, event: Event, trust: Trust) -> Result<Received, Invalid> {
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
            match self.admit(&event, trust) {
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
                            .release(Waiting::Event(id))
                            .into_iter()
                            .map(|released| (released, Trust::Stored)),
                    );
                    if is_genesis {
                        queue.extend(
                            self.pending
                                .release(Waiting::Genesis)
                                .into_iter()
                                .map(|released| (released, Trust::Stored)),
                        );
                    }
                    if origin == Origin::History && !self.history_incomplete() {
                        queue.extend(
                            self.pending
                                .release(Waiting::History)
                                .into_iter()
                                .map(|released| (released, Trust::Stored)),
                        );
                    }
                }
                Ok(Admission::Wait(waiting)) => {
                    // Events that wait for the history of a fork are signed by bound devices and
                    // end when the history is complete, so they are not capped.
                    if waiting != Waiting::History && self.pending.events.len() >= MAX_PENDING {
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

    fn admit(&self, event: &Event, trust: Trust) -> Result<Admission, Invalid> {
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

        let Some(workforce) = &self.workforce else {
            return match (&content.body, content.workforce) {
                (EventBody::Genesis(_), _) => {
                    self.admit_genesis(event, trust)?;
                    Ok(Admission::Apply(Origin::Own))
                }
                (_, None) => Err(Invalid::MissingWorkforce),
                (_, Some(id)) => match self.expected {
                    Some(expected) if expected != id => Err(Invalid::WrongWorkforce(id)),
                    Some(_) | None => Ok(Admission::Wait(Waiting::Genesis)),
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
            return Ok(Admission::Wait(Waiting::History));
        }

        if let Some(missing) = event
            .causal_parents()
            .find(|parent| !self.known.contains_key(parent))
        {
            return Ok(Admission::Wait(Waiting::Event(missing)));
        }

        self.check_feed(event)?;
        self.check_hlc(event)?;
        match &content.body {
            EventBody::DeviceBound(_) => self.check_binding(event)?,
            _ => self.check_author(event)?,
        }
        self.check_body(event, trust)?;
        Ok(Admission::Apply(Origin::Own))
    }

    /// `prev` is empty exactly at seq 1, and otherwise an event of the same author with the
    /// previous seq. Nothing else about the feed is checked: equivocated feeds are admitted.
    fn check_feed(&self, event: &Event) -> Result<(), Invalid> {
        let content = &event.content;
        let author = content.author;
        if content.seq == 0 {
            return Err(Invalid::UnexpectedSeq {
                author,
                expected: 1,
                got: 0,
            });
        }
        let prev_is_valid = match content.prev {
            None => content.seq == 1,
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
        if content.actor != Actor::Human(invite.principal) {
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
                let owner = workforce
                    .members
                    .get(&signed.signer)
                    .is_some_and(|member| member.role == Role::Owner);
                if !owner {
                    return Err(Invalid::NotOwner(signed.signer));
                }
                if trust == Trust::Untrusted && signed.verify(Some(workforce.id)).is_err() {
                    return Err(Invalid::BadPrincipalSignature);
                }
                Policy::parse(&signed.statement.policy)?;
                Ok(())
            }
            EventBody::InviteCreated(signed) => {
                if !workforce.members.contains_key(&signed.signer) {
                    return Err(Invalid::NotAMember(signed.signer));
                }
                if signed.signer != event.content.actor.principal() {
                    return Err(Invalid::ActorMismatch);
                }
                if trust == Trust::Untrusted && signed.verify(Some(workforce.id)).is_err() {
                    return Err(Invalid::BadPrincipalSignature);
                }
                Ok(())
            }
            EventBody::ForkAnnounced(signed) => {
                let owner = workforce
                    .members
                    .get(&signed.signer)
                    .is_some_and(|member| member.role == Role::Owner);
                if !owner {
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
            EventBody::AgentDefined(AgentDefined { .. })
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
            EventBody::Report(report) => {
                if workforce.sessions.contains_key(&report.session) {
                    Ok(())
                } else {
                    Err(Invalid::UnknownSession(report.session))
                }
            }
            EventBody::Review(crate::event::Review { item, .. })
            | EventBody::Merged(crate::event::Merged { item, .. }) => known_item(item),
            EventBody::ItemEdited(ItemEdited { item, .. })
            | EventBody::ItemTransitioned(ItemTransitioned { item, .. })
            | EventBody::Comment(crate::event::Comment { item, .. })
            | EventBody::ItemArchived(crate::event::ItemRef { item })
            | EventBody::ItemUnarchived(crate::event::ItemRef { item }) => known_item(item),
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
        if content.actor != Actor::Human(genesis.founder.principal) {
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
        Policy::parse(&genesis.policy)?;
        Ok(())
    }

    /// Checks what the acting agent may do, if the actor is one. Run by the author before signing,
    /// together with [`State::check_policy`]. These are guards against mistakes and shortcuts,
    /// not against attacks: members are trusted and their keys are on disk.
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
                EventBody::Report(_) => Err(PolicyViolation::ReportNotFromItsSession),
                _ => Ok(()),
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
            | EventBody::AgentDefined(_)
            | EventBody::InviteCreated(_)
            | EventBody::DeviceBound(_)
            | EventBody::ForkAnnounced(_)
            | EventBody::SessionStarted(_)
            | EventBody::Merged(_) => Err(PolicyViolation::AgentCannotWrite(body.name())),
            EventBody::SessionEnded(ended) if ended.session != *session => {
                Err(PolicyViolation::SessionEndsAnother)
            }
            EventBody::Report(report) if report.session != *session => {
                Err(PolicyViolation::ReportNotFromItsSession)
            }
            EventBody::ItemEdited(edited) => {
                let lowers = edited
                    .changes
                    .iter()
                    .any(|change| matches!(change, Change::Review(_)))
                    && self.lowers_review(actor, edited);
                if lowers {
                    Err(PolicyViolation::AgentLowersReview(edited.item))
                } else {
                    Ok(())
                }
            }
            EventBody::ItemTransitioned(transition) if transition.item == folded.item() => {
                let policy = &workforce.policy.value().policy;
                let finishes = workforce
                    .items
                    .get(&transition.item)
                    .and_then(|item| policy.type_config(item.item_type()))
                    .and_then(|config| config.state_category_of(&transition.to))
                    == Some(StateCategory::Done);
                if finishes {
                    Err(PolicyViolation::SessionFinishesItsItem)
                } else {
                    Ok(())
                }
            }
            EventBody::DeviceProfile(_)
            | EventBody::ItemCreated(_)
            | EventBody::ItemTransitioned(_)
            | EventBody::ItemLinked(_)
            | EventBody::ItemArchived(_)
            | EventBody::ItemUnarchived(_)
            | EventBody::Comment(_)
            | EventBody::SessionEnded(_)
            | EventBody::Review(_)
            | EventBody::Report(_)
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
            .is_some_and(|definition| definition.value().lanes.contains(&supervision));
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
        let policy = &workforce.policy.value().policy;
        // `None` ranks below every level, so removing the review lowers it.
        let rank = |level: Option<&ReviewName>| {
            level.and_then(|name| policy.review_rank(name).map(|rank| rank + 1))
        };
        let before = rank(item.effective_review(policy));
        let type_default = policy
            .type_config(item.item_type())
            .and_then(|config| config.review.as_ref());
        edited
            .changes
            .iter()
            .filter_map(|change| {
                let Change::Review(review) = change else {
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
        let policy = &workforce.policy.value().policy;
        let known_lane = |lane: &LaneName| {
            if policy.lanes.contains_key(lane) {
                Ok(())
            } else {
                Err(PolicyViolation::UnknownLane(lane.clone()))
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
                .ok_or_else(|| PolicyViolation::UnknownType(type_name.clone()))
        };
        let known_review = |type_name: &TypeName, review: &Option<ReviewName>| {
            let Some(level) = review else {
                return Ok(());
            };
            if policy.review_level(level).is_none() {
                return Err(PolicyViolation::UnknownReviewLevel(level.clone()));
            }
            let reviewable = type_of(type_name)?
                .first_state(StateCategory::Blocked)
                .is_some();
            if reviewable {
                Ok(())
            } else {
                Err(PolicyViolation::ReviewWithoutBlockedState {
                    type_name: type_name.clone(),
                })
            }
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
            | EventBody::Comment(_)
            | EventBody::Unknown { .. } => Ok(()),
            EventBody::InviteCreated(_)
            | EventBody::DeviceBound(_)
            | EventBody::DeviceProfile(_)
            | EventBody::ForkAnnounced(_)
            | EventBody::SessionStarted(_)
            | EventBody::SessionEnded(_)
            | EventBody::Review(_)
            | EventBody::Merged(_) => Ok(()),
            EventBody::Report(report) => {
                let session = workforce
                    .sessions
                    .get(&report.session)
                    .ok_or(PolicyViolation::UnknownSession(report.session))?;
                let Some(step) = report.step else {
                    return Ok(());
                };
                if matches!(session.role(), SessionRole::Review { .. }) {
                    return Err(PolicyViolation::StepInReviewReport);
                }
                let has_step = workforce
                    .items
                    .get(&session.item())
                    .is_some_and(|item| item.body().step(step).is_some());
                if has_step {
                    Ok(())
                } else {
                    Err(PolicyViolation::UnknownStep(step))
                }
            }
            EventBody::AgentDefined(defined) => defined.lanes.iter().try_for_each(known_lane),
            EventBody::ItemCreated(created) => {
                let type_name = &created.item_type;
                type_of(type_name)?;
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
                        Change::Title(_) => {}
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
                        Change::Review(review) => known_review(type_name, review)?,
                    }
                }
                Ok(())
            }
            EventBody::ItemTransitioned(transition) => {
                let item = item_of(&transition.item)?;
                let config = type_of(item.item_type())?;
                match config.state_category_of(&transition.to) {
                    Some(_) => Ok(()),
                    None => Err(PolicyViolation::UnknownState {
                        type_name: item.item_type().clone(),
                        state: transition.to.clone(),
                    }),
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
                let state_category = item.standing(policy).state_category();
                match state_category {
                    Some(state_category) if state_category.is_terminal() => Ok(()),
                    _ => Err(PolicyViolation::ArchiveRequiresTerminal { state_category }),
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

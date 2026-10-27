//! Signing a log of event version 1 again in the current version: `wf import --resign`.
//!
//! It reads the `content` of each line of a `wf export` by version 1, as that version showed it,
//! not the signed bytes, so no decoder of version 1 is kept. The whole log becomes one feed of this
//! device under a new genesis: every id changes, and every id an event names, or a text refers to,
//! is changed with it. Before the result is handed out, the export and the new log fold to the same
//! items under the map of ids. Delete this module, with `wf import --resign`, once no log of
//! version 1 is left (SPEC §6, §17.7).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;

use serde::Deserialize;
use serde_json::Value;

use crate::body::{Body, StepId};
use crate::crdt::SetChange;
use crate::event::{
    Change, EVENT_VERSION, Event, EventBody, EventBuilder, EventContent, EventError, Genesis,
    ItemCreated, ItemLinked, ItemMoved, ItemRef, Link,
};
use crate::hlc::Hlc;
use crate::id::{
    CommentId, EventId, HashId, ItemId, PrefixIndex, SHORT_COMMENT_ID_LEN, SHORT_ID_LEN, SessionId,
    WorkforceId,
};
use crate::identity::{
    Actor, AgentId, DeviceKey, DeviceSigner, MemberSeed, PrincipalKey, PrincipalSigner, Signed,
};
use crate::item::{ItemState, Priority};
use crate::name::{Label, LaneName, Requirement, ReviewLevelName, TypeName};
use crate::policy::{Policy, PolicyError};
use crate::refs::{self, Token};
use crate::state::{Disposition, Origin, State, Trust};
use crate::validate::Invalid;

/// The event version whose exports this module reads.
pub const OLD_VERSION: u16 = 1;

#[derive(Debug, thiserror::Error)]
pub enum ResignError {
    #[error("event {id}: {message}")]
    Unreadable { id: EventId, message: String },
    #[error("event {id} is of version {version}; this reads exports of version {OLD_VERSION}")]
    Version { id: EventId, version: u16 },
    #[error("event {id} is a `{kind}`, which version {OLD_VERSION} did not write")]
    UnknownKind { id: EventId, kind: String },
    #[error("the export holds no genesis, so not a whole workforce")]
    NoGenesis,
    #[error("the export holds the geneses of {0} workforces that are not forks of one another")]
    SeveralWorkforces(usize),
    #[error("the principal key of this device for workforce {0} is not the key of its founder")]
    NotFounder(WorkforceId),
    #[error(
        "event {id} is by another principal than the founder, {principal}: only the log of a \
         workforce of one principal can be signed again"
    )]
    OtherPrincipal {
        id: EventId,
        principal: PrincipalKey,
    },
    #[error(
        "sessions are live: {}; end them with the version that wrote the export, and export again",
        .0.join(", ")
    )]
    LiveSessions(Vec<String>),
    #[error(
        "the policy does not read in the current form; rewrite it with `wf policy edit` of the \
         version that wrote the export, and export again"
    )]
    Policy(#[source] PolicyError),
    #[error("event {id}, a `{kind}`, is refused once signed again")]
    Refused {
        id: EventId,
        kind: &'static str,
        #[source]
        invalid: Invalid,
    },
    #[error("event {id}, a `{kind}`, waits for an event the new log lacks")]
    Waits { id: EventId, kind: &'static str },
    #[error("the new log does not fold to what the export does: {0}")]
    Mismatch(String),
    #[error(transparent)]
    Sign(#[from] EventError),
    #[error(transparent)]
    Key(#[from] crate::identity::KeyError),
}

/// One line of `wf export` by version 1. Its `event`, the signed bytes, is not read.
#[derive(Deserialize)]
pub struct ExportedEvent {
    id: EventId,
    kind: String,
    content: Value,
}

/// The log signed again.
pub struct Resigned {
    pub old_workforce: WorkforceId,
    pub workforce: WorkforceId,
    /// The genesis, then every event carried over, in the order they were signed.
    pub events: Vec<Event>,
    /// The new id of every event carried over, by its old one, with the kind of its body.
    pub renamed: BTreeMap<EventId, (EventId, &'static str)>,
    /// How many events of each kind were not carried over.
    pub dropped: BTreeMap<String, usize>,
}

impl Resigned {
    pub fn item(&self, old: ItemId) -> Option<ItemId> {
        renamed(&self.renamed, old)
    }

    pub fn session(&self, old: SessionId) -> Option<SessionId> {
        renamed(&self.renamed, old)
    }

    pub fn comment(&self, old: CommentId) -> Option<CommentId> {
        renamed(&self.renamed, old)
    }

    /// Every item, by its old id and its new one.
    pub fn items(&self) -> impl Iterator<Item = (ItemId, ItemId)> + '_ {
        self.of_kind("ItemCreated")
    }

    /// Every session, by its old id and its new one.
    pub fn sessions(&self) -> impl Iterator<Item = (SessionId, SessionId)> + '_ {
        self.of_kind("SessionStarted")
    }

    fn of_kind<T: From<EventId>>(&self, kind: &'static str) -> impl Iterator<Item = (T, T)> + '_ {
        self.renamed
            .iter()
            .filter(move |(_, (_, renamed_kind))| *renamed_kind == kind)
            .map(|(old, (new, _))| (T::from(*old), T::from(*new)))
    }
}

fn renamed<T: HashId + From<EventId>>(
    map: &BTreeMap<EventId, (EventId, &'static str)>,
    old: T,
) -> Option<T> {
    map.get(&event_id(old)).map(|(new, _)| T::from(*new))
}

fn event_id(id: impl HashId) -> EventId {
    EventId::from_bytes(*id.as_bytes())
}

/// The envelope as version 1 showed it. `seq` and `prev` are made again.
#[derive(Deserialize)]
struct Content {
    v: u16,
    workforce: Option<WorkforceId>,
    author: DeviceKey,
    actor: Actor,
    deps: Vec<EventId>,
    hlc: Hlc,
    body: Value,
}

#[derive(Deserialize)]
struct Statement<T> {
    statement: T,
}

#[derive(Deserialize)]
struct PolicyStatement {
    policy: String,
}

/// `ItemCreated` as version 1 showed it: without a state, or with one by an old name.
#[derive(Deserialize)]
struct OldItemCreated {
    item_type: TypeName,
    title: String,
    body: String,
    priority: Option<Priority>,
    lane: Option<LaneName>,
    labels: Vec<Label>,
    requires: Vec<Requirement>,
    review: Option<ReviewLevelName>,
    state: Option<String>,
}

/// `ItemMoved` as version 1 showed it, with the state as written.
#[derive(Deserialize)]
struct OldItemMoved {
    item: ItemId,
    to: String,
    by_session: Option<crate::id::SessionId>,
    because: Option<CommentId>,
}

/// A state as version 1 wrote it: the fixed states, and the names the built-in types had before
/// them. A name no version knew read as `draft`, as version 1 read it.
fn state_as_written(name: &str) -> ItemState {
    match name {
        "backlog" => ItemState::Draft,
        "in_progress" => ItemState::Active,
        "in_review" | "proposed" => ItemState::Review,
        "ratified" | "superseded" => ItemState::Done,
        "dropped" | "rejected" => ItemState::Cancelled,
        current => current.parse().unwrap_or(ItemState::Draft),
    }
}

enum OldBody {
    Genesis(Genesis),
    Policy(String),
    /// An invite, a bind or a fork announcement: nothing of it is carried over.
    Dropped,
    /// Given its state once the policy is known.
    Created(OldItemCreated),
    Carried(EventBody),
}

struct OldEvent {
    id: EventId,
    kind: String,
    workforce: Option<WorkforceId>,
    author: DeviceKey,
    actor: Actor,
    deps: Vec<EventId>,
    hlc: Hlc,
    body: OldBody,
}

impl OldEvent {
    fn read(exported: ExportedEvent) -> Result<Self, ResignError> {
        let id = exported.id;
        let unreadable = |error: serde_json::Error| ResignError::Unreadable {
            id,
            message: error.to_string(),
        };
        let content: Content = serde_json::from_value(exported.content).map_err(unreadable)?;
        if content.v != OLD_VERSION {
            return Err(ResignError::Version {
                id,
                version: content.v,
            });
        }
        let value = content.body;
        let carried = |body: Result<EventBody, serde_json::Error>| {
            body.map(OldBody::Carried).map_err(unreadable)
        };
        use serde_json::from_value;
        let body = match exported.kind.as_str() {
            "Genesis" => OldBody::Genesis(
                from_value::<Statement<Genesis>>(value)
                    .map_err(unreadable)?
                    .statement,
            ),
            "PolicySet" | "BoundPolicySet" => OldBody::Policy(
                from_value::<Statement<PolicyStatement>>(value)
                    .map_err(unreadable)?
                    .statement
                    .policy,
            ),
            "InviteCreated" | "DeviceBound" | "ForkAnnounced" => OldBody::Dropped,
            "ItemCreated" => OldBody::Created(from_value(value).map_err(unreadable)?),
            "ItemMoved" => {
                let moved: OldItemMoved = from_value(value).map_err(unreadable)?;
                OldBody::Carried(EventBody::ItemMoved(ItemMoved {
                    item: moved.item,
                    to: state_as_written(&moved.to),
                    by_session: moved.by_session,
                    because: moved.because,
                }))
            }
            "AgentDefined" => carried(from_value(value).map(EventBody::AgentDefined))?,
            "AgentRemoved" => carried(from_value(value).map(EventBody::AgentRemoved))?,
            "DeviceProfile" => carried(from_value(value).map(EventBody::DeviceProfile))?,
            "ItemEdited" => carried(from_value(value).map(EventBody::ItemEdited))?,
            "ItemLinked" => carried(from_value(value).map(EventBody::ItemLinked))?,
            "ItemArchived" => carried(from_value(value).map(EventBody::ItemArchived))?,
            "ItemUnarchived" => carried(from_value(value).map(EventBody::ItemUnarchived))?,
            "Commented" => carried(from_value(value).map(EventBody::Commented))?,
            "SessionStarted" => carried(from_value(value).map(EventBody::SessionStarted))?,
            "SessionEnded" => carried(from_value(value).map(EventBody::SessionEnded))?,
            "Reviewed" => carried(from_value(value).map(EventBody::Reviewed))?,
            "Merged" => carried(from_value(value).map(EventBody::Merged))?,
            "TargetMoved" => carried(from_value(value).map(EventBody::TargetMoved))?,
            "MergeClaimed" => carried(from_value(value).map(EventBody::MergeClaimed))?,
            _ => {
                return Err(ResignError::UnknownKind {
                    id,
                    kind: exported.kind,
                });
            }
        };
        Ok(OldEvent {
            id,
            kind: exported.kind,
            workforce: content.workforce,
            author: content.author,
            actor: content.actor,
            deps: content.deps,
            hlc: content.hlc,
            body,
        })
    }

    fn stamp(&self) -> (Hlc, EventId) {
        (self.hlc, self.id)
    }
}

/// The log of a `wf export` by version 1, read.
pub struct Export {
    events: Vec<OldEvent>,
}

impl Export {
    pub fn read(exported: Vec<ExportedEvent>) -> Result<Self, ResignError> {
        let events = exported
            .into_iter()
            .map(OldEvent::read)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Export { events })
    }

    /// The workforce the export is of; of a fork, the fork rather than its parents.
    pub fn workforce(&self) -> Result<WorkforceId, ResignError> {
        own_genesis(&self.events).map(|(workforce, _)| workforce)
    }

    /// Signs the log again as one feed of `device`, under a new genesis signed by `principal`,
    /// the founder's key. Fails when the log cannot be carried over whole, or when the new log
    /// would fold to other items than the export.
    pub fn resign(
        &self,
        device: &DeviceSigner,
        principal: &PrincipalSigner,
    ) -> Result<Resigned, ResignError> {
        resign(&self.events, device, principal)
    }
}

fn resign(
    events: &[OldEvent],
    device: &DeviceSigner,
    principal: &PrincipalSigner,
) -> Result<Resigned, ResignError> {
    let (old_workforce, own) = own_genesis(events)?;
    if own.founder.principal != principal.key() {
        return Err(ResignError::NotFounder(old_workforce));
    }
    let founder = own.founder.principal;
    let aliases = aliases(events);
    let policy_source = latest_policy(events, old_workforce, own);
    let policy = Policy::parse(&policy_source).map_err(ResignError::Policy)?;
    refuse_live_sessions(events)?;

    let genesis = Genesis {
        name: own.name.clone(),
        founder: MemberSeed {
            principal: founder,
            handle: own.founder.handle.clone(),
            devices: vec![device.key()],
            previous: Vec::new(),
        },
        members: Vec::new(),
        policy: policy_source,
        forked_from: None,
    };
    let first_hlc = events
        .iter()
        .map(|event| event.hlc)
        .min()
        .expect("an export with a genesis has events");
    let genesis = EventBuilder::genesis(device.key(), Actor::Principal(founder), 0)
        .with_hlc(first_hlc)
        .sign(
            device,
            EventBody::Genesis(Signed::sign(principal, None, genesis)?),
        )?;
    let workforce = WorkforceId::from(genesis.id());

    let mut carried = Vec::new();
    let mut dropped: BTreeMap<String, usize> = BTreeMap::new();
    for event in events {
        let keep = match &event.body {
            OldBody::Created(_) => true,
            OldBody::Carried(EventBody::DeviceProfile(_)) => event.author == device.key(),
            OldBody::Carried(_) => true,
            OldBody::Genesis(_) | OldBody::Policy(_) | OldBody::Dropped => false,
        };
        if keep {
            carried.push(event);
        } else {
            *dropped.entry(event.kind.clone()).or_default() += 1;
        }
    }
    carried.sort_by_key(|event| event.stamp());

    let mut text = Rewriter::new(events);
    let old_geneses: BTreeSet<EventId> = events
        .iter()
        .filter(|event| matches!(event.body, OldBody::Genesis(_)))
        .map(|event| event.id)
        .collect();
    let mut state = State::new();
    admit(&mut state, genesis.clone(), genesis.id(), "Genesis")?;
    let mut old_side = Vec::new();
    let mut signed = vec![genesis.clone()];
    let (mut previous, mut previous_old, mut hlc) = (genesis.id(), genesis.id(), first_hlc);
    for (index, event) in carried.into_iter().enumerate() {
        let principal = canonical(&aliases, event.actor.principal());
        if principal != founder {
            return Err(ResignError::OtherPrincipal {
                id: event.id,
                principal,
            });
        }
        let body = match &event.body {
            OldBody::Created(created) => EventBody::ItemCreated(with_state(created, &policy)),
            OldBody::Carried(body) => body.clone(),
            OldBody::Genesis(_) | OldBody::Policy(_) | OldBody::Dropped => {
                unreachable!("only carried events are signed again")
            }
        };
        let body = text.rewrite(event.id, body);
        hlc = if event.hlc > hlc {
            event.hlc
        } else {
            Hlc::next_after(hlc, 0).expect("an exported clock below the maximum")
        };
        let seq = index as u64 + 2;
        let rename = |id: EventId| match text.renamed.get(&id) {
            Some((new, _)) => *new,
            None if old_geneses.contains(&id) => genesis.id(),
            None => id,
        };
        let new_body = rename_body(&body, &rename);
        let content = EventContent {
            v: EVENT_VERSION,
            workforce: Some(workforce),
            author: device.key(),
            actor: rename_actor(&event.actor, founder, &rename),
            seq,
            prev: Some(previous),
            deps: renamed_deps(&event.deps, &text.renamed, &old_geneses, genesis.id()),
            hlc,
            body: new_body,
        };
        let new = Event::sign_content(content, device)?;
        let kind = new.content.body.name();
        let id = new.id();
        admit(&mut state, new.clone(), event.id, kind)?;
        text.renamed.insert(event.id, (id, kind));
        let old_content = EventContent {
            v: EVENT_VERSION,
            workforce: Some(workforce),
            author: device.key(),
            actor: rename_actor(&event.actor, founder, &|id| id),
            seq,
            prev: Some(previous_old),
            deps: Vec::new(),
            hlc,
            body,
        };
        old_side.push((event.id, Event::sign_content(old_content, device)?));
        signed.push(new);
        previous = id;
        previous_old = event.id;
    }

    let mut old_state = State::new();
    old_state.apply(&genesis, genesis.id(), Origin::Own);
    for (id, event) in &old_side {
        old_state.apply(event, *id, Origin::Own);
    }
    compare(&old_state, &state, &text.renamed)?;

    Ok(Resigned {
        old_workforce,
        workforce,
        events: signed,
        renamed: text.renamed,
        dropped,
    })
}

fn admit(
    state: &mut State,
    event: Event,
    old: EventId,
    kind: &'static str,
) -> Result<(), ResignError> {
    let received =
        state
            .receive(event, Trust::Untrusted)
            .map_err(|invalid| ResignError::Refused {
                id: old,
                kind,
                invalid,
            })?;
    match received.disposition {
        Disposition::Applied => Ok(()),
        Disposition::Pending | Disposition::Duplicate => Err(ResignError::Waits { id: old, kind }),
    }
}

/// The genesis of the workforce the export is of: of a fork, the export holds the geneses of its
/// parents too, which it names as forked from.
fn own_genesis(events: &[OldEvent]) -> Result<(WorkforceId, &Genesis), ResignError> {
    let geneses: Vec<(EventId, &Genesis)> = events
        .iter()
        .filter_map(|event| match &event.body {
            OldBody::Genesis(genesis) => Some((event.id, genesis)),
            OldBody::Policy(_) | OldBody::Dropped | OldBody::Created(_) | OldBody::Carried(_) => {
                None
            }
        })
        .collect();
    let parents: BTreeSet<WorkforceId> = geneses
        .iter()
        .filter_map(|(_, genesis)| genesis.forked_from.as_ref())
        .map(|point| point.workforce)
        .collect();
    let own: Vec<_> = geneses
        .into_iter()
        .filter(|(id, _)| !parents.contains(&WorkforceId::from(*id)))
        .collect();
    match own.as_slice() {
        [] => Err(ResignError::NoGenesis),
        [(id, genesis)] => Ok((WorkforceId::from(*id), genesis)),
        several => Err(ResignError::SeveralWorkforces(several.len())),
    }
}

/// Keys that members of a fork used in its parents, by the key that replaced them.
fn aliases(events: &[OldEvent]) -> BTreeMap<PrincipalKey, PrincipalKey> {
    let mut aliases = BTreeMap::new();
    for event in events {
        if let OldBody::Genesis(genesis) = &event.body {
            for seed in std::iter::once(&genesis.founder).chain(&genesis.members) {
                for previous in &seed.previous {
                    aliases.insert(*previous, seed.principal);
                }
            }
        }
    }
    aliases
}

fn canonical(aliases: &BTreeMap<PrincipalKey, PrincipalKey>, key: PrincipalKey) -> PrincipalKey {
    let mut key = key;
    // A fork of a fork may have replaced a key twice.
    while let Some(next) = aliases.get(&key).filter(|next| **next != key) {
        key = *next;
    }
    key
}

/// The policy the workforce of the export ran on last: of its genesis and its own policy events,
/// the latest. Those of the parents of a fork never applied to it.
fn latest_policy(events: &[OldEvent], workforce: WorkforceId, own: &Genesis) -> String {
    events
        .iter()
        .filter_map(|event| match &event.body {
            OldBody::Policy(policy) if event.workforce == Some(workforce) => {
                Some((event.stamp(), policy))
            }
            OldBody::Policy(_)
            | OldBody::Genesis(_)
            | OldBody::Dropped
            | OldBody::Created(_)
            | OldBody::Carried(_) => None,
        })
        .max_by_key(|(stamp, _)| *stamp)
        .map_or_else(|| own.policy.clone(), |(_, policy)| policy.clone())
}

/// A session that has not ended would be live in the new log with nothing that runs it.
fn refuse_live_sessions(events: &[OldEvent]) -> Result<(), ResignError> {
    let mut started = BTreeSet::new();
    let mut ended = BTreeSet::new();
    for event in events {
        match &event.body {
            OldBody::Carried(EventBody::SessionStarted(_)) => {
                started.insert(SessionId::from(event.id));
            }
            OldBody::Carried(EventBody::SessionEnded(end)) => {
                ended.insert(end.session);
            }
            OldBody::Carried(_)
            | OldBody::Genesis(_)
            | OldBody::Policy(_)
            | OldBody::Dropped
            | OldBody::Created(_) => {}
        }
    }
    let live: Vec<String> = started.difference(&ended).map(HashId::short).collect();
    match live.is_empty() {
        true => Ok(()),
        false => Err(ResignError::LiveSessions(live)),
    }
}

/// An item created without a state starts in the initial state of its type, as version 1 read it.
fn with_state(created: &OldItemCreated, policy: &Policy) -> ItemCreated {
    let state = match &created.state {
        Some(name) => state_as_written(name),
        None => policy
            .type_config(&created.item_type)
            .map_or(ItemState::Draft, |config| config.initial),
    };
    ItemCreated {
        item_type: created.item_type.clone(),
        title: created.title.clone(),
        body: created.body.clone(),
        priority: created.priority,
        lane: created.lane.clone(),
        labels: created.labels.clone(),
        requires: created.requires.clone(),
        review: created.review.clone(),
        state,
    }
}

fn renamed_deps(
    deps: &[EventId],
    renamed: &BTreeMap<EventId, (EventId, &'static str)>,
    old_geneses: &BTreeSet<EventId>,
    genesis: EventId,
) -> Vec<EventId> {
    let mut new: Vec<EventId> = deps
        .iter()
        .filter_map(|dep| match renamed.get(dep) {
            Some((new, _)) => Some(*new),
            None => old_geneses.contains(dep).then_some(genesis),
        })
        .collect();
    new.sort();
    new.dedup();
    new
}

fn rename_actor(
    actor: &Actor,
    founder: PrincipalKey,
    rename: &impl Fn(EventId) -> EventId,
) -> Actor {
    match actor {
        Actor::Principal(_) => Actor::Principal(founder),
        Actor::Daemon(_) => Actor::Daemon(founder),
        Actor::Agent { agent, session } => Actor::Agent {
            agent: AgentId {
                principal: founder,
                name: agent.name.clone(),
            },
            session: rename_id(*session, rename),
        },
    }
}

fn rename_id<T: HashId + From<EventId>>(id: T, rename: &impl Fn(EventId) -> EventId) -> T {
    T::from(rename(event_id(id)))
}

fn rename_ids<T: HashId + From<EventId>>(
    ids: &[T],
    rename: &impl Fn(EventId) -> EventId,
) -> Vec<T> {
    ids.iter().map(|id| rename_id(*id, rename)).collect()
}

fn rename_set_change<T: Clone>(
    change: &SetChange<T>,
    value: impl Fn(&T) -> T,
    rename: &impl Fn(EventId) -> EventId,
) -> SetChange<T> {
    match change {
        SetChange::Add(added) => SetChange::Add(value(added)),
        SetChange::Remove {
            value: removed,
            observed,
        } => SetChange::Remove {
            value: value(removed),
            observed: observed.iter().map(|id| rename(*id)).collect(),
        },
    }
}

/// `body` with every id it names renamed.
fn rename_body(body: &EventBody, rename: &impl Fn(EventId) -> EventId) -> EventBody {
    let item = |id: ItemId| rename_id(id, rename);
    match body {
        EventBody::Genesis(_)
        | EventBody::PolicySet(_)
        | EventBody::AgentDefined(_)
        | EventBody::AgentRemoved(_)
        | EventBody::InviteCreated(_)
        | EventBody::DeviceBound(_)
        | EventBody::DeviceProfile(_)
        | EventBody::ForkAnnounced(_)
        | EventBody::ItemCreated(_)
        | EventBody::TargetMoved(_)
        | EventBody::Unknown { .. } => body.clone(),
        EventBody::ItemEdited(edited) => {
            let mut edited = edited.clone();
            edited.item = item(edited.item);
            for change in &mut edited.changes {
                match change {
                    Change::Labels(labels) => {
                        *labels = rename_set_change(labels, Clone::clone, rename)
                    }
                    Change::Requires(requires) => {
                        *requires = rename_set_change(requires, Clone::clone, rename);
                    }
                    Change::Title(_)
                    | Change::Body(_)
                    | Change::Priority(_)
                    | Change::Lane(_)
                    | Change::ReviewLevel(_)
                    | Change::StepDone { .. }
                    | Change::RecoveryLevel(_) => {}
                }
            }
            EventBody::ItemEdited(edited)
        }
        EventBody::ItemMoved(moved) => EventBody::ItemMoved(ItemMoved {
            item: item(moved.item),
            to: moved.to,
            by_session: moved.by_session.map(|session| rename_id(session, rename)),
            because: moved.because.map(|comment| rename_id(comment, rename)),
        }),
        EventBody::ItemLinked(linked) => EventBody::ItemLinked(ItemLinked {
            item: item(linked.item),
            link: match &linked.link {
                Link::DependsOn(change) => {
                    Link::DependsOn(rename_set_change(change, |target| item(*target), rename))
                }
                Link::Supersedes(target) => Link::Supersedes(item(*target)),
            },
        }),
        EventBody::ItemArchived(reference) => EventBody::ItemArchived(ItemRef {
            item: item(reference.item),
        }),
        EventBody::ItemUnarchived(reference) => EventBody::ItemUnarchived(ItemRef {
            item: item(reference.item),
        }),
        EventBody::Commented(comment) => {
            let mut comment = comment.clone();
            comment.item = item(comment.item);
            comment.resolves = rename_ids(&comment.resolves, rename);
            EventBody::Commented(comment)
        }
        EventBody::SessionStarted(started) => {
            let mut started = started.clone();
            started.item = item(started.item);
            EventBody::SessionStarted(started)
        }
        EventBody::SessionEnded(ended) => {
            let mut ended = ended.clone();
            ended.session = rename_id(ended.session, rename);
            ended.because = ended.because.map(|comment| rename_id(comment, rename));
            EventBody::SessionEnded(ended)
        }
        EventBody::Reviewed(reviewed) => {
            let mut reviewed = reviewed.clone();
            reviewed.item = item(reviewed.item);
            reviewed.because = reviewed.because.map(|comment| rename_id(comment, rename));
            EventBody::Reviewed(reviewed)
        }
        EventBody::Merged(merged) => {
            let mut merged = merged.clone();
            merged.item = item(merged.item);
            EventBody::Merged(merged)
        }
        EventBody::MergeClaimed(claimed) => {
            let mut claimed = claimed.clone();
            claimed.item = item(claimed.item);
            EventBody::MergeClaimed(claimed)
        }
    }
}

/// Rewrites the references in texts to the new ids, and the steps a changed text renames.
struct Rewriter {
    items: PrefixIndex<ItemId>,
    comments: BTreeMap<ItemId, Vec<CommentId>>,
    /// The steps of each item whose text a rewritten reference changed, by their old id.
    steps: BTreeMap<ItemId, BTreeMap<StepId, StepId>>,
    renamed: BTreeMap<EventId, (EventId, &'static str)>,
}

impl Rewriter {
    fn new(events: &[OldEvent]) -> Self {
        let mut items = Vec::new();
        let mut comments: BTreeMap<ItemId, Vec<CommentId>> = BTreeMap::new();
        for event in events {
            match &event.body {
                OldBody::Created(_) => items.push(ItemId::from(event.id)),
                OldBody::Carried(EventBody::Commented(comment)) => comments
                    .entry(comment.item)
                    .or_default()
                    .push(CommentId::from(event.id)),
                OldBody::Carried(_)
                | OldBody::Genesis(_)
                | OldBody::Policy(_)
                | OldBody::Dropped => {}
            }
        }
        Rewriter {
            items: PrefixIndex::new(items),
            comments,
            steps: BTreeMap::new(),
            renamed: BTreeMap::new(),
        }
    }

    /// The texts of the body of event `id` with their references rewritten, and the steps it
    /// names renamed with them.
    fn rewrite(&mut self, id: EventId, body: EventBody) -> EventBody {
        match body {
            EventBody::ItemCreated(mut created) => {
                let item = ItemId::from(id);
                created.title = self.text(&created.title);
                let text = self.text(&created.body);
                self.note_steps(item, &created.body, &text);
                created.body = text;
                EventBody::ItemCreated(created)
            }
            EventBody::ItemEdited(mut edited) => {
                for change in &mut edited.changes {
                    match change {
                        Change::Title(title) => *title = self.text(title),
                        Change::Body(body) => {
                            let text = self.text(body);
                            self.note_steps(edited.item, body, &text);
                            *body = text;
                        }
                        Change::StepDone { step, .. } => *step = self.step(edited.item, *step),
                        Change::Priority(_)
                        | Change::Lane(_)
                        | Change::Labels(_)
                        | Change::Requires(_)
                        | Change::ReviewLevel(_)
                        | Change::RecoveryLevel(_) => {}
                    }
                }
                EventBody::ItemEdited(edited)
            }
            EventBody::Commented(mut comment) => {
                comment.text = self.text(&comment.text);
                comment.step = comment.step.map(|step| self.step(comment.item, step));
                EventBody::Commented(comment)
            }
            body @ (EventBody::Genesis(_)
            | EventBody::PolicySet(_)
            | EventBody::AgentDefined(_)
            | EventBody::AgentRemoved(_)
            | EventBody::InviteCreated(_)
            | EventBody::DeviceBound(_)
            | EventBody::DeviceProfile(_)
            | EventBody::ForkAnnounced(_)
            | EventBody::ItemMoved(_)
            | EventBody::ItemLinked(_)
            | EventBody::ItemArchived(_)
            | EventBody::ItemUnarchived(_)
            | EventBody::SessionStarted(_)
            | EventBody::SessionEnded(_)
            | EventBody::Reviewed(_)
            | EventBody::Merged(_)
            | EventBody::TargetMoved(_)
            | EventBody::MergeClaimed(_)
            | EventBody::Unknown { .. }) => body,
        }
    }

    /// `text` with each reference outside code that resolves to an item, or a comment on one,
    /// that has a new id written with the new id, as long a prefix as before and no shorter
    /// than usual.
    fn text(&self, text: &str) -> String {
        let found = refs::parse(text);
        refs::rewrite_item_refs(text, |token| {
            if !found.contains(token) {
                return None;
            }
            match token {
                Token::ItemRef(prefix) => {
                    let (_, new) = self.item(prefix)?;
                    Some(prefix_of(new, prefix.len().max(SHORT_ID_LEN)))
                }
                Token::CommentRef { item, comment } => {
                    let (old_item, new_item) = self.item(item)?;
                    let old = crate::id::resolve_prefix(
                        comment,
                        self.comments.get(&old_item)?.iter().copied(),
                    )
                    .ok()?;
                    let new = renamed(&self.renamed, old)?;
                    Some(format!(
                        "{}/{}",
                        prefix_of(new_item, item.len().max(SHORT_ID_LEN)),
                        prefix_of(new, comment.len().max(SHORT_COMMENT_ID_LEN))
                    ))
                }
                Token::Mention(_) => None,
            }
        })
    }

    fn item(&self, prefix: &str) -> Option<(ItemId, ItemId)> {
        let old = self.items.resolve(prefix).ok()?;
        Some((old, renamed(&self.renamed, old)?))
    }

    fn note_steps(&mut self, item: ItemId, old: &str, new: &str) {
        if old == new {
            return;
        }
        let (Ok(old), Ok(new)) = (Body::parse(old), Body::parse(new)) else {
            return;
        };
        let steps = self.steps.entry(item).or_default();
        for (old, new) in old.steps().zip(new.steps()) {
            if old.id != new.id {
                steps.insert(old.id, new.id);
            }
        }
    }

    fn step(&self, item: ItemId, step: StepId) -> StepId {
        self.steps
            .get(&item)
            .and_then(|steps| steps.get(&step))
            .copied()
            .unwrap_or(step)
    }
}

fn prefix_of(id: impl HashId, length: usize) -> String {
    let text = id.to_base32();
    text[..length.min(text.len())].to_string()
}

/// What the comparison looks at: per item, each field as text, ids renamed.
#[derive(PartialEq, Eq)]
struct Folded {
    items: BTreeMap<ItemId, Vec<(&'static str, String)>>,
    sessions: BTreeMap<SessionId, String>,
    target_chain: Vec<EventId>,
}

fn compare(
    old: &State,
    new: &State,
    renamed: &BTreeMap<EventId, (EventId, &'static str)>,
) -> Result<(), ResignError> {
    let rename = |id: EventId| renamed.get(&id).map_or(id, |(new, _)| *new);
    let before = folded(old, &rename);
    let after = folded(new, &|id| id);
    for (item, fields) in &before.items {
        let Some(new_fields) = after.items.get(item) else {
            return Err(ResignError::Mismatch(format!("item {item} is missing")));
        };
        for ((name, was), (_, is)) in fields.iter().zip(new_fields) {
            if was != is {
                return Err(ResignError::Mismatch(format!(
                    "the {name} of item {item} is {is}, not {was}"
                )));
            }
        }
    }
    if before.items.len() != after.items.len() {
        return Err(ResignError::Mismatch(format!(
            "{} items, not {}",
            after.items.len(),
            before.items.len()
        )));
    }
    if before.sessions != after.sessions {
        let differs = before
            .sessions
            .iter()
            .find(|(session, was)| after.sessions.get(*session) != Some(*was))
            .map_or_else(
                || "another session".to_string(),
                |(session, _)| format!("session {session}"),
            );
        return Err(ResignError::Mismatch(format!("{differs} differs")));
    }
    if before.target_chain != after.target_chain {
        return Err(ResignError::Mismatch("the target's chain differs".into()));
    }
    Ok(())
}

fn folded(state: &State, rename: &impl Fn(EventId) -> EventId) -> Folded {
    let Some(workforce) = &state.workforce else {
        return Folded {
            items: BTreeMap::new(),
            sessions: BTreeMap::new(),
            target_chain: Vec::new(),
        };
    };
    let id = |id: EventId| rename(id);
    let item_id = |item: ItemId| rename_id(item, rename);
    let comment_id = |comment: CommentId| rename_id(comment, rename);
    fn text(value: impl Debug) -> String {
        format!("{value:?}")
    }
    let items = workforce
        .items
        .values()
        .map(|item| {
            let comments: BTreeMap<CommentId, _> = item
                .comments
                .iter()
                .map(|(comment, folded)| {
                    let resolved = folded.resolved().map(|resolved| comment_id(resolved.by));
                    (comment_id(*comment), (folded.kind, folded.step, resolved))
                })
                .collect();
            let verdicts: BTreeMap<EventId, _> = item
                .verdicts
                .iter()
                .map(|(event, review)| {
                    let verdict = (review.commit, review.verdict, review.round_limit_reached);
                    (id(*event), verdict)
                })
                .collect();
            let merges: BTreeMap<EventId, _> = item
                .merges
                .iter()
                .map(|(event, merge)| (id(*event), (merge.reviewed, merge.commit, merge.lost)))
                .collect();
            let claims: BTreeMap<EventId, _> = item
                .merge_claims
                .iter()
                .map(|(event, claim)| (id(*event), claim.commit))
                .collect();
            let depends_on: BTreeSet<ItemId> = item
                .depends_on
                .values()
                .map(|target| item_id(*target))
                .collect();
            let marks: BTreeMap<StepId, bool> = item
                .step_marks
                .iter()
                .map(|(step, mark)| (*step, *mark.value()))
                .collect();
            let fields = vec![
                ("type", text(&item.item_type)),
                ("title", text(item.title.value())),
                ("body", text(item.body.value().render())),
                ("state", text(item.state.value())),
                ("priority", text(item.priority.value())),
                ("lane", text(item.lane.value())),
                ("review level", text(item.review.value())),
                ("recovery level", text(item.recovery.value())),
                ("labels", text(item.labels.values().collect::<Vec<_>>())),
                (
                    "requirements",
                    text(item.requires.values().collect::<Vec<_>>()),
                ),
                ("dependencies", text(depends_on)),
                (
                    "superseded item",
                    text(item.supersedes.value().map(item_id)),
                ),
                ("archived", text(item.archived.value().is_some())),
                ("ticked steps", text(marks)),
                ("comments", text(comments)),
                ("verdicts", text(verdicts)),
                ("merges", text(merges)),
                ("merge claims", text(claims)),
            ];
            (item_id(item.id), fields)
        })
        .collect();
    let sessions = workforce
        .sessions
        .values()
        .map(|session| {
            let ended = session.ended().map(|ended| ended.reason);
            let summary = (
                item_id(session.item()),
                &session.agent().name,
                session.role(),
                ended,
                session.head(),
            );
            (rename_id(session.id(), rename), text(summary))
        })
        .collect();
    let target_chain = workforce
        .target_chain
        .iter()
        .map(|event| id(*event))
        .collect();
    Folded {
        items,
        sessions,
        target_chain,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::DeviceProfile;
    use crate::testing::{Fixture, comment, create, create_with_body, define_agent, start_session};

    /// The line `wf export` of version 1 wrote for `event`, as far as an import reads it.
    fn exported(event: &Event) -> ExportedEvent {
        struct Payload<'a>(&'a EventBody);
        impl serde::Serialize for Payload<'_> {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                self.0.serialize_payload(serializer)
            }
        }
        let mut content = serde_json::to_value(&event.content).unwrap();
        content["v"] = OLD_VERSION.into();
        content["body"] = serde_json::to_value(Payload(&event.content.body)).unwrap();
        ExportedEvent {
            id: event.id(),
            kind: event.content.body.name().to_string(),
            content,
        }
    }

    fn export(fixture: &Fixture, events: &[Event]) -> Export {
        let lines = std::iter::once(&fixture.genesis)
            .chain(events)
            .map(exported)
            .collect();
        Export::read(lines).unwrap()
    }

    #[test]
    fn the_events_of_every_device_are_signed_by_this_one_and_the_items_kept() {
        let fixture = Fixture::new(2);
        let mut state = fixture.state();
        let first = fixture.apply(&mut state, 1, 2_000, create("task", "First"));
        let reference = format!("after #{}", &ItemId::from(first.id()).to_string()[..6]);
        let second = fixture.apply(
            &mut state,
            0,
            3_000,
            create_with_body("task", "Second", &reference),
        );
        let said = fixture.apply(&mut state, 1, 4_000, comment(first.id().into(), "hi"));
        let profile = fixture.apply(
            &mut state,
            1,
            5_000,
            EventBody::DeviceProfile(DeviceProfile {
                name: "desktop".into(),
                hardware: "test".into(),
                agents: Default::default(),
                max_sessions: 1,
            }),
        );
        let export = export(&fixture, &[first.clone(), second, said.clone(), profile]);
        assert_eq!(export.workforce().unwrap(), fixture.workforce());

        let resigned = export
            .resign(&fixture.devices[0], &fixture.founder)
            .unwrap();
        assert_ne!(resigned.workforce, fixture.workforce());
        assert!(
            resigned
                .events
                .iter()
                .all(|event| event.content.author == fixture.devices[0].key())
        );
        assert_eq!(resigned.dropped.get("DeviceProfile"), Some(&1));
        let mut state = State::new();
        for event in &resigned.events {
            state.receive(event.clone(), Trust::Untrusted).unwrap();
        }
        let new_first = resigned.item(first.id().into()).unwrap();
        assert_eq!(state.item(new_first).unwrap().title(), "First");
        let new_comment = resigned.comment(said.id().into()).unwrap();
        assert!(state.items().any(|item| match item {
            crate::item::ItemEntry::Live(item) => item.comments().contains_key(&new_comment),
            crate::item::ItemEntry::Archived(_) => false,
        }));
        let written = state
            .items()
            .find(|item| item.title() == "Second")
            .map(|item| match item {
                crate::item::ItemEntry::Live(item) => item.body().render(),
                crate::item::ItemEntry::Archived(_) => String::new(),
            })
            .unwrap();
        assert_eq!(written, format!("after #{}", &new_first.to_string()[..6]));
    }

    #[test]
    fn a_session_that_has_not_ended_is_refused() {
        let fixture = Fixture::new(1);
        let mut state = fixture.state();
        let agent = fixture.apply(&mut state, 0, 2_000, define_agent("coder"));
        let item = fixture.apply(&mut state, 0, 3_000, create("task", "First"));
        let started = fixture.apply(
            &mut state,
            0,
            4_000,
            start_session("coder", item.id().into()),
        );
        let error = export(&fixture, &[agent, item, started])
            .resign(&fixture.devices[0], &fixture.founder)
            .err()
            .unwrap();
        assert!(matches!(error, ResignError::LiveSessions(_)), "{error}");
    }

    #[test]
    fn the_events_of_another_principal_are_refused() {
        let fixture = Fixture::new(1);
        let state = fixture.state();
        let created = state
            .builder(
                fixture.member_device.key(),
                Actor::Principal(fixture.member.key()),
                2_000,
            )
            .unwrap()
            .sign(&fixture.member_device, create("task", "Theirs"))
            .unwrap();
        let error = export(&fixture, &[created])
            .resign(&fixture.devices[0], &fixture.founder)
            .err()
            .unwrap();
        assert!(
            matches!(error, ResignError::OtherPrincipal { .. }),
            "{error}"
        );
    }

    #[test]
    fn only_the_founder_signs_again() {
        let fixture = Fixture::new(1);
        let error = export(&fixture, &[])
            .resign(&fixture.devices[0], &fixture.member)
            .err()
            .unwrap();
        assert!(matches!(error, ResignError::NotFounder(_)), "{error}");
    }

    #[test]
    fn states_are_read_by_their_old_names() {
        assert_eq!(state_as_written("in_progress"), ItemState::Active);
        assert_eq!(state_as_written("in_review"), ItemState::Review);
        assert_eq!(state_as_written("ratified"), ItemState::Done);
        assert_eq!(state_as_written("rejected"), ItemState::Cancelled);
        assert_eq!(state_as_written("open"), ItemState::Open);
    }
}

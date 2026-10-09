//! The folded state of a workforce: a deterministic function of the set of accepted events.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::crdt::Lww;
use crate::event::{Event, EventBuilder};
use crate::hlc::Hlc;
use crate::id::{EventId, ItemId, WorkforceId};
use crate::identity::{Actor, AgentId, DeviceKey, PrincipalKey, Role};
use crate::item::{Item, ItemView};
use crate::name::{AgentName, Handle, HarnessName, LaneName, Requirement};
use crate::policy::Policy;
use crate::validate::Invalid;

/// Events that wait for missing parents are capped so a malformed `deps` cannot grow memory forever.
pub const MAX_PENDING: usize = 10_000;

/// Whether an event's signatures still need checking. The store only holds verified events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trust {
    Untrusted,
    Stored,
}

/// The policy as stored, with its parsed form.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyDoc {
    pub source: String,
    pub policy: Policy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub handle: Handle,
    pub role: Role,
    pub devices: BTreeSet<DeviceKey>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDefinition {
    pub harness: HarnessName,
    pub model: Option<String>,
    pub capabilities: Vec<Requirement>,
    pub lanes: Vec<LaneName>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedHead {
    pub seq: u64,
    pub id: EventId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Workforce {
    pub(crate) id: WorkforceId,
    pub(crate) name: String,
    pub(crate) founder: PrincipalKey,
    pub(crate) members: BTreeMap<PrincipalKey, Member>,
    pub(crate) devices: BTreeMap<DeviceKey, PrincipalKey>,
    pub(crate) policy: Lww<PolicyDoc>,
    pub(crate) agents: BTreeMap<AgentId, Lww<AgentDefinition>>,
    pub(crate) items: BTreeMap<ItemId, Item>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Waiting {
    Genesis,
    Event(EventId),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Pending {
    pub(crate) events: BTreeMap<EventId, Event>,
    pub(crate) waiting: BTreeMap<Waiting, BTreeSet<EventId>>,
}

impl Pending {
    pub(crate) fn insert(&mut self, waiting: Waiting, id: EventId, event: Event) {
        self.events.insert(id, event);
        self.waiting.entry(waiting).or_default().insert(id);
    }

    pub(crate) fn release(&mut self, waiting: Waiting) -> Vec<Event> {
        let Some(ids) = self.waiting.remove(&waiting) else {
            return Vec::new();
        };
        ids.iter().filter_map(|id| self.events.remove(id)).collect()
    }
}

/// What happened to the submitted event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    Applied,
    /// Waiting for missing parents.
    Pending,
    /// Already known.
    Duplicate,
}

/// The outcome of [`State::receive`]. The store persists exactly these changes.
#[derive(Debug)]
pub struct Received {
    pub disposition: Disposition,
    /// Newly applied events in application order: the submitted one and any it released.
    pub applied: Vec<Event>,
    /// Previously pending events that turned out to be invalid and were dropped.
    pub rejected: Vec<(EventId, Invalid)>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub(crate) workforce: Option<Workforce>,
    pub(crate) feeds: BTreeMap<DeviceKey, FeedHead>,
    pub(crate) known: BTreeMap<EventId, Hlc>,
    pub(crate) heads: BTreeSet<EventId>,
    #[serde(skip)]
    pub(crate) pending: Pending,
}

impl State {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn workforce_id(&self) -> Option<WorkforceId> {
        self.workforce.as_ref().map(|workforce| workforce.id)
    }

    pub fn name(&self) -> Option<&str> {
        self.workforce
            .as_ref()
            .map(|workforce| workforce.name.as_str())
    }

    pub fn founder(&self) -> Option<PrincipalKey> {
        self.workforce.as_ref().map(|workforce| workforce.founder)
    }

    pub fn policy(&self) -> Option<&PolicyDoc> {
        self.workforce
            .as_ref()
            .map(|workforce| workforce.policy.value())
    }

    pub fn members(&self) -> impl Iterator<Item = (&PrincipalKey, &Member)> {
        self.workforce
            .iter()
            .flat_map(|workforce| workforce.members.iter())
    }

    pub fn member(&self, principal: &PrincipalKey) -> Option<&Member> {
        self.workforce.as_ref()?.members.get(principal)
    }

    /// The principal a device is bound to.
    pub fn principal_of(&self, device: &DeviceKey) -> Option<PrincipalKey> {
        self.workforce.as_ref()?.devices.get(device).copied()
    }

    pub fn agents(&self) -> impl Iterator<Item = (&AgentId, &AgentDefinition)> {
        self.workforce
            .iter()
            .flat_map(|workforce| workforce.agents.iter())
            .map(|(id, definition)| (id, definition.value()))
    }

    pub fn agent(&self, name: &AgentName, principal: PrincipalKey) -> Option<&AgentDefinition> {
        let id = AgentId {
            principal,
            name: name.clone(),
        };
        self.workforce.as_ref()?.agents.get(&id).map(Lww::value)
    }

    pub fn item(&self, id: ItemId) -> Option<ItemView<'_>> {
        self.workforce.as_ref()?.items.get(&id).map(ItemView::of)
    }

    /// All items, archived ones as stubs, ordered by id.
    pub fn items(&self) -> impl Iterator<Item = ItemView<'_>> {
        self.workforce
            .iter()
            .flat_map(|workforce| workforce.items.values())
            .map(ItemView::of)
    }

    pub fn live_items(&self) -> impl Iterator<Item = &Item> {
        self.items().filter_map(|view| match view {
            ItemView::Live(item) => Some(item),
            ItemView::Archived(_) => None,
        })
    }

    pub(crate) fn raw_items(&self) -> impl Iterator<Item = &Item> {
        self.workforce
            .iter()
            .flat_map(|workforce| workforce.items.values())
    }

    pub fn feed_heads(&self) -> &BTreeMap<DeviceKey, FeedHead> {
        &self.feeds
    }

    /// Events nothing else depends on yet.
    pub fn heads(&self) -> &BTreeSet<EventId> {
        &self.heads
    }

    pub fn event_count(&self) -> usize {
        self.known.len()
    }

    pub fn contains(&self, id: &EventId) -> bool {
        self.known.contains_key(id)
    }

    pub fn pending_events(&self) -> impl Iterator<Item = &Event> {
        self.pending.events.values()
    }

    /// Starts an event authored by `author` on top of everything this state has seen.
    /// `None` before genesis, when there is no workforce to write into.
    pub fn builder(&self, author: DeviceKey, actor: Actor, now_ms: u64) -> Option<EventBuilder> {
        let workforce = self.workforce.as_ref()?;
        let head = self.feeds.get(&author);
        let floor = self
            .heads
            .iter()
            .filter_map(|head| self.known.get(head))
            .max()
            .copied()
            .unwrap_or(Hlc::ZERO);
        Some(EventBuilder {
            workforce: Some(workforce.id),
            author,
            actor,
            seq: head.map_or(1, |head| head.seq + 1),
            prev: head.map(|head| head.id),
            deps: self.heads.iter().copied().collect(),
            hlc: Hlc::next_after(floor, now_ms),
        })
    }
}

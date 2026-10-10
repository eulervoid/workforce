//! The folded state of a workforce: a deterministic function of the set of accepted events.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::crdt::{Lww, Stamp};
use crate::event::{DeviceProfile, Event, EventBuilder, InviteKind};
use crate::hlc::Hlc;
use crate::id::{EventId, ItemId, WorkforceId};
use crate::identity::{Actor, AgentId, DeviceKey, InviteKey, PrincipalKey, Role};
use crate::item::{Item, ItemView};
use crate::name::{AgentName, Handle, HarnessName, LaneName, Requirement};
use crate::policy::Policy;
use crate::sync::{FeedVersion, Versions};
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FeedHead {
    pub seq: u64,
    pub id: EventId,
}

/// Whether an applied event was written in this workforce or is history of the one it was forked
/// from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Origin {
    Own,
    History,
}

/// What the applied events say about where a new event sits in its feed.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Neighbours {
    /// An applied event names the new one as its `prev`.
    pub(crate) has_child: bool,
    /// Another applied event has the same `prev`.
    pub(crate) has_sibling: bool,
}

/// What one device has written. A device that wrote two events with the same `seq` (a machine
/// restored from a snapshot, say) has several tips; both branches are kept.
///
/// Events normally arrive after their parents, but the history of a fork is applied from the
/// newest event down, so nothing here depends on the order events were applied in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Feed {
    tips: BTreeSet<FeedHead>,
    roots: u32,
    equivocated: Option<u64>,
}

impl Feed {
    pub(crate) fn new() -> Self {
        Self {
            tips: BTreeSet::new(),
            roots: 0,
            equivocated: None,
        }
    }

    /// Records an event of this feed. If another applied event already has the same `prev` (or
    /// this is a second root), two events share a `seq` and the feed has equivocated.
    pub(crate) fn record(&mut self, head: FeedHead, prev: Option<EventId>, around: Neighbours) {
        let equivocated = match prev {
            Some(prev) => {
                self.tips.remove(&FeedHead {
                    seq: head.seq - 1,
                    id: prev,
                });
                around.has_sibling
            }
            None => {
                self.roots += 1;
                self.roots > 1
            }
        };
        if equivocated {
            self.equivocated = Some(self.equivocated.map_or(head.seq, |seq| seq.min(head.seq)));
        }
        if !around.has_child {
            self.tips.insert(head);
        }
    }

    /// The highest tip: the only one unless the feed has equivocated.
    pub fn head(&self) -> FeedHead {
        *self.tips.last().expect("a feed has at least one tip")
    }

    pub fn max_seq(&self) -> u64 {
        self.head().seq
    }

    pub fn tips(&self) -> impl Iterator<Item = &FeedHead> {
        self.tips.iter()
    }

    /// The lowest `seq` at which the feed has two events, if any.
    pub fn equivocated(&self) -> Option<u64> {
        self.equivocated
    }
}

/// What is remembered about every applied event: enough to check the events that follow it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Known {
    pub(crate) hlc: Hlc,
    pub(crate) author: DeviceKey,
    pub(crate) seq: u64,
}

/// A device bound to a principal: by genesis, or by redeeming an invite.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Binding {
    pub(crate) principal: PrincipalKey,
    /// The earliest binding wins, so a device bound twice (a cloned join) has one principal on
    /// every peer.
    pub(crate) stamp: Stamp,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invite {
    pub kind: InviteKind,
    /// The principal that signed it, and whose devices it binds.
    pub principal: PrincipalKey,
    pub key: InviteKey,
    pub expires_ms: u64,
    /// Every device whose `DeviceBound` used this invite.
    pub redeemed_by: BTreeSet<DeviceKey>,
}

/// A fork of this workforce that an owner announced here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Announcement {
    pub to: WorkforceId,
    /// The members the fork keeps.
    pub members: Vec<PrincipalKey>,
    pub by: PrincipalKey,
}

/// How a forked workforce takes in the history it was forked from: events of the parent are
/// read-only history, admitted only when named by an event already taken in, starting with the
/// heads the fork was made at.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ForkState {
    pub(crate) parent: WorkforceId,
    pub(crate) heads: Vec<EventId>,
    /// Named by an applied event (or the fork point) but not applied yet.
    pub(crate) wanted: BTreeSet<EventId>,
}

/// What `wf device list` shows about a bound device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo<'a> {
    pub device: DeviceKey,
    pub principal: PrincipalKey,
    pub profile: Option<&'a DeviceProfile>,
    /// Bound with an invite that was redeemed by more than one device.
    pub invite_redeemed_twice: bool,
    /// The lowest `seq` at which the device's feed forked.
    pub equivocated: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Workforce {
    pub(crate) id: WorkforceId,
    pub(crate) name: String,
    pub(crate) founder: PrincipalKey,
    pub(crate) members: BTreeMap<PrincipalKey, Member>,
    pub(crate) devices: BTreeMap<DeviceKey, Binding>,
    pub(crate) invites: BTreeMap<EventId, Invite>,
    pub(crate) fork: Option<ForkState>,
    /// Principal keys of the parent workforce, by the key that replaced them here.
    pub(crate) aliases: BTreeMap<PrincipalKey, PrincipalKey>,
    pub(crate) announcements: BTreeMap<EventId, Announcement>,
    pub(crate) profiles: BTreeMap<DeviceKey, Lww<DeviceProfile>>,
    pub(crate) policy: Lww<PolicyDoc>,
    pub(crate) agents: BTreeMap<AgentId, Lww<AgentDefinition>>,
    pub(crate) items: BTreeMap<ItemId, Item>,
}

impl Workforce {
    pub(crate) fn canonical(&self, principal: &PrincipalKey) -> PrincipalKey {
        self.aliases.get(principal).copied().unwrap_or(*principal)
    }
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
    pub(crate) feeds: BTreeMap<DeviceKey, Feed>,
    pub(crate) known: BTreeMap<EventId, Known>,
    /// Events that some applied event names as its `prev`.
    pub(crate) extended: BTreeSet<EventId>,
    pub(crate) heads: BTreeSet<EventId>,
    #[serde(skip)]
    pub(crate) pending: Pending,
    /// The workforce this state is for, until its genesis has been applied: any other genesis
    /// is refused, and so are events of another workforce.
    #[serde(skip)]
    pub(crate) expected: Option<WorkforceId>,
}

impl State {
    /// A state that takes whichever workforce's genesis arrives first.
    pub fn new() -> Self {
        Self::default()
    }

    /// A state that only takes the genesis of workforce `id`.
    pub fn expecting(id: WorkforceId) -> Self {
        Self {
            expected: Some(id),
            ..Self::default()
        }
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

    /// Looks a member up by a current key or by a key the member used in the parent workforce.
    pub fn member(&self, principal: &PrincipalKey) -> Option<&Member> {
        let workforce = self.workforce.as_ref()?;
        workforce.members.get(&workforce.canonical(principal))
    }

    /// The key a principal has in this workforce, whichever key an event names.
    pub fn canonical_principal(&self, principal: &PrincipalKey) -> PrincipalKey {
        self.workforce
            .as_ref()
            .map_or(*principal, |workforce| workforce.canonical(principal))
    }

    /// The workforce this one was forked from and the heads it was forked at.
    pub fn fork_point(&self) -> Option<(WorkforceId, &[EventId])> {
        let fork = self.workforce.as_ref()?.fork.as_ref()?;
        Some((fork.parent, &fork.heads))
    }

    /// Forks of this workforce announced by its owners.
    pub fn announcements(&self) -> impl Iterator<Item = &Announcement> {
        self.workforce
            .iter()
            .flat_map(|workforce| workforce.announcements.values())
    }

    /// The principal a device is bound to.
    pub fn principal_of(&self, device: &DeviceKey) -> Option<PrincipalKey> {
        self.workforce
            .as_ref()?
            .devices
            .get(device)
            .map(|binding| binding.principal)
    }

    pub fn invite(&self, id: &EventId) -> Option<&Invite> {
        self.workforce.as_ref()?.invites.get(id)
    }

    /// Every bound device with what it says about itself and what is wrong with it.
    pub fn devices(&self) -> impl Iterator<Item = DeviceInfo<'_>> {
        self.workforce.iter().flat_map(|workforce| {
            workforce
                .devices
                .iter()
                .map(|(device, binding)| DeviceInfo {
                    device: *device,
                    principal: binding.principal,
                    profile: workforce.profiles.get(device).map(Lww::value),
                    invite_redeemed_twice: workforce.invites.values().any(|invite| {
                        invite.redeemed_by.len() > 1 && invite.redeemed_by.contains(device)
                    }),
                    equivocated: self.feeds.get(device).and_then(Feed::equivocated),
                })
        })
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

    pub fn feeds(&self) -> &BTreeMap<DeviceKey, Feed> {
        &self.feeds
    }

    /// What is known of each author's feed: what a peer needs to know to send the rest.
    pub fn versions(&self) -> Versions {
        self.feeds
            .iter()
            .map(|(author, feed)| {
                (
                    *author,
                    FeedVersion {
                        max_seq: feed.max_seq(),
                        tips: feed.tips().copied().collect(),
                    },
                )
            })
            .collect()
    }

    /// Devices whose feed has two events with the same `seq`, and the lowest such `seq`.
    pub fn equivocations(&self) -> impl Iterator<Item = (DeviceKey, u64)> + '_ {
        self.feeds
            .iter()
            .filter_map(|(author, feed)| Some((*author, feed.equivocated()?)))
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

    /// Starts the genesis of a fork of this workforce: it continues the feed `author` has here
    /// and comes after everything this state has seen. `None` before genesis.
    pub fn fork_genesis_builder(
        &self,
        author: DeviceKey,
        actor: Actor,
        now_ms: u64,
    ) -> Option<EventBuilder> {
        let mut builder = self.builder(author, actor, now_ms)?;
        builder.workforce = None;
        builder.deps = Vec::new();
        Some(builder)
    }

    /// Starts an event authored by `author` on top of everything this state has seen.
    /// `None` before genesis, when there is no workforce to write into.
    pub fn builder(&self, author: DeviceKey, actor: Actor, now_ms: u64) -> Option<EventBuilder> {
        let workforce = self.workforce.as_ref()?;
        let head = self.feeds.get(&author).map(Feed::head);
        let floor = self
            .heads
            .iter()
            .copied()
            .chain(head.map(|head| head.id))
            .filter_map(|id| self.known.get(&id))
            .map(|known| known.hlc)
            .max()
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

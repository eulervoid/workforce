//! Placement: which device takes a claimable thing (an item in a role, as [`crate::claims`] says)
//! when several could. Every device ranks the devices that could take it by the same rules, from
//! the log, the profiles and the presence of devices, so they agree; the dispatcher of the device
//! of rank *r* waits *r* times [`RANK_DELAY`] before it starts. Claims keep a wrong ranking
//! correct: it costs a delay or a race, never two holders. Pure functions of the state.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crate::claims::Presence;
use crate::id::{HashId, ItemId, SessionId};
use crate::identity::{Actor, AgentId, DeviceKey};
use crate::session::SessionRole;
use crate::state::State;
use crate::supervision::WaitsOn;

/// How much longer each rank waits than the one before it: well above the few seconds a claim
/// takes to reach the other devices.
pub const RANK_DELAY: Duration = Duration::from_secs(15);

/// The devices that could take each claimable thing, best first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Placement {
    ranked: BTreeMap<(ItemId, SessionRole), Vec<DeviceKey>>,
}

impl Placement {
    /// The rank of `device` for `item` in `role`: 0 for the device that starts at once. A device
    /// that is not among those that could take it comes after all of them.
    pub fn rank(&self, item: ItemId, role: &SessionRole, device: DeviceKey) -> usize {
        let ranked = self.ranked(item, role);
        ranked
            .iter()
            .position(|candidate| *candidate == device)
            .unwrap_or(ranked.len())
    }

    /// The devices that could take `item` in `role`, best first.
    pub fn ranked(&self, item: ItemId, role: &SessionRole) -> &[DeviceKey] {
        self.ranked
            .get(&(item, role.clone()))
            .map_or(&[], Vec::as_slice)
    }

    /// How long the device of `rank` waits before it starts.
    pub fn delay(rank: usize) -> Duration {
        RANK_DELAY * u32::try_from(rank).unwrap_or(u32::MAX)
    }
}

impl State {
    /// What `agent` would start on `device`, in the order the dispatcher takes it: reviews, then
    /// blockers to supervise, then work to diagnose, then work in dispatch order. Nothing when the
    /// device or the agent has no free slot. What only `expired` sessions hold is free to take
    /// over.
    pub fn startable(
        &self,
        device: DeviceKey,
        agent: &AgentId,
        expired: &BTreeSet<SessionId>,
    ) -> Vec<(ItemId, SessionRole)> {
        if !self.has_free_slot(device, agent) {
            return Vec::new();
        }
        let reviews = self
            .reviewable(device, agent, expired)
            .into_iter()
            .map(|(item, commit, step)| (item, SessionRole::Review { commit, step }));
        let supervisions = self
            .supervisable(agent, expired)
            .into_iter()
            .map(|item| (item, SessionRole::Supervise));
        let diagnoses = self
            .diagnosable(device, agent, expired)
            .into_iter()
            .map(|item| (item, SessionRole::Diagnose));
        let work = self
            .dispatchable(device, agent, expired)
            .into_iter()
            .map(|item| (item.id(), SessionRole::Work));
        reviews
            .chain(supervisions)
            .chain(diagnoses)
            .chain(work)
            .collect()
    }

    /// The placement of everything claimable, as the daemon on `here` sees it at `now_ms`: for
    /// each thing, the devices with a hosted auto agent that has a free slot and may take it
    /// ([`State::startable`]), leaving out those whose lease ran out. `here` always counts as
    /// present. They rank by [`State::home`] first, then by a hash of the thing and the device
    /// (rendezvous hashing), which spreads the rest evenly and that every device computes alike.
    pub fn placement(&self, here: DeviceKey, presence: &Presence, now_ms: u64) -> Placement {
        let lease = self.lease();
        let expired = self.expired(here, presence, now_ms);
        let mut ranked: BTreeMap<(ItemId, SessionRole), Vec<DeviceKey>> = BTreeMap::new();
        for info in self.devices() {
            let device = info.device;
            if device != here && presence.gone(device, lease, now_ms) {
                continue;
            }
            for (agent, _) in self.auto_agents(device) {
                for thing in self.startable(device, &agent, &expired) {
                    let devices = ranked.entry(thing).or_default();
                    if !devices.contains(&device) {
                        devices.push(device);
                    }
                }
            }
        }
        for ((item, role), devices) in &mut ranked {
            let home = self.home(*item, role);
            devices.sort_by_cached_key(|device| {
                (
                    Some(*device) != home,
                    Reverse(rendezvous(*item, role, *device)),
                )
            });
        }
        Placement { ranked }
    }

    /// The device `item` in `role` goes to first, if any: for work, where the work is (the device
    /// of the item's latest work session, with its repository, branch and build caches); for a
    /// review, where it was written (the device of the work session that submitted it, where the
    /// commit is and the checks just ran); for supervision, where it waits (the device of the
    /// session that wrote the oldest blocker a supervisor answers). A diagnosis has none.
    pub fn home(&self, item: ItemId, role: &SessionRole) -> Option<DeviceKey> {
        match role {
            SessionRole::Work => self
                .sessions_of(item)
                .filter(|session| session.role() == SessionRole::Work)
                .max_by_key(|session| session.started())
                .map(|session| session.device()),
            SessionRole::Review { .. } => self
                .submitting_session(item)
                .map(|(session, _)| session.device()),
            SessionRole::Supervise => {
                let crate::item::ItemEntry::Live(live) = self.item(item)? else {
                    return None;
                };
                self.open_blockers_of(live)
                    .into_iter()
                    .filter(|blocker| !matches!(blocker.waits_on, WaitsOn::Principal(_)))
                    .filter_map(|blocker| live.comments().get(&blocker.comment))
                    .min_by_key(|comment| comment.at)
                    .and_then(|comment| match comment.by {
                        Actor::Agent { session, .. } => self.session(session),
                        Actor::Principal(_) | Actor::Daemon(_) => None,
                    })
                    .map(|session| session.device())
            }
            SessionRole::Diagnose => None,
        }
    }
}

/// The score of `device` for `item` in `role`: the highest takes it. The same on every device, and
/// for each thing an independent draw, so things spread evenly over devices.
pub(crate) fn rendezvous(item: ItemId, role: &SessionRole, device: DeviceKey) -> u64 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"wf placement\0");
    hasher.update(item.as_bytes());
    hasher.update(&postcard::to_allocvec(role).expect("a role encodes"));
    hasher.update(device.as_bytes());
    let hash = hasher.finalize();
    let mut first = [0; 8];
    first.copy_from_slice(&hash.as_bytes()[..8]);
    u64::from_le_bytes(first)
}

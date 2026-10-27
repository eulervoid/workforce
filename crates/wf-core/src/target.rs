//! The target branch as a chain in the log. Every move of it names the tip it moved from: a
//! `Merged` with `onto`, or a `TargetMoved` a daemon wrote for a push of the principal. Two moves
//! onto the same tip of the chain are a fork, and the earliest by `(hlc, EventId)` wins on every
//! device; the target is the end of the chain of winners. The chain starts at the earliest move,
//! whose `onto` no earlier move made: the tip the target had before the log named one. At each
//! tip it goes on with the earliest later move onto it, so a move made from a commit before the
//! chain came back to it is no fork, and moves that go round in a circle are followed round it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::claims::Presence;
use crate::crdt::Stamp;
use crate::id::{EventId, ItemId};
use crate::identity::{DeviceKey, PrincipalKey};
use crate::item::ItemState;
use crate::session::CommitId;
use crate::state::State;

/// One move of the target branch, from `onto` to `commit`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetMove {
    pub commit: CommitId,
    pub onto: CommitId,
    pub cause: MoveCause,
    /// The device whose `<wf>/git` moved, which wrote the event.
    pub device: DeviceKey,
    pub(crate) stamp: Stamp,
}

impl TargetMove {
    pub fn event(&self) -> EventId {
        self.stamp.id
    }
}

/// What moved the target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MoveCause {
    /// The merge of the item's reviewed work.
    Merge { item: ItemId },
    /// A push of the principal to `wf`.
    Push,
}

/// The ids of the moves on the chain, in order, from the moves of the log.
pub(crate) fn chain(moves: &BTreeMap<EventId, TargetMove>) -> Vec<EventId> {
    let mut by_stamp: Vec<&TargetMove> = moves.values().collect();
    by_stamp.sort_by_key(|step| step.stamp);
    let Some((start, later)) = by_stamp.split_first() else {
        return Vec::new();
    };
    let mut chain = vec![start.stamp.id];
    let mut tip = start.commit;
    for step in later {
        if step.onto == tip {
            chain.push(step.stamp.id);
            tip = step.commit;
        }
    }
    chain
}

impl State {
    /// Where the target branch is, as the log says: the end of its chain. `None` until a move of
    /// it names the tip it moved from.
    pub fn target(&self) -> Option<CommitId> {
        self.target_tip().map(|step| step.commit)
    }

    /// The move at the end of the target's chain, whose `commit` is the target.
    pub fn target_tip(&self) -> Option<&TargetMove> {
        let workforce = self.workforce.as_ref()?;
        let last = workforce.target_chain.last()?;
        workforce.target_moves.get(last)
    }

    /// Every move of the target the log names, on the chain or not, by event id.
    pub fn target_moves(&self) -> impl Iterator<Item = &TargetMove> {
        self.workforce
            .iter()
            .flat_map(|workforce| workforce.target_moves.values())
    }

    /// Whether the move written by `event` lost a fork: it is not on the chain.
    pub fn move_lost(&self, event: EventId) -> bool {
        self.workforce.as_ref().is_some_and(|workforce| {
            workforce.target_moves.contains_key(&event) && !workforce.target_chain.contains(&event)
        })
    }

    /// The moves that lost a fork, of the target as `device` moved it.
    pub fn lost_moves(&self, device: DeviceKey) -> impl Iterator<Item = &TargetMove> {
        self.target_moves()
            .filter(move |step| step.device == device && self.move_lost(step.event()))
    }

    /// The items done whose merge lost a fork, that the daemon on `device` moves back to `review`,
    /// where they wait for their merge again: those whose merge it made, and those of a device of
    /// its principal that is gone, as `presence` says at `now_ms`, which may never return to. Only
    /// the latest merge of an item counts, so an item merged again since stays done.
    pub fn lost_merges(&self, device: DeviceKey, presence: &Presence, now_ms: u64) -> Vec<ItemId> {
        let (principal, lease) = (self.principal_of(&device), self.lease());
        let sends_back = |maker: DeviceKey| {
            maker == device
                || (principal.is_some()
                    && self.principal_of(&maker) == principal
                    && presence.gone(maker, lease, now_ms))
        };
        self.live_items()
            .filter(|live| live.state() == ItemState::Done)
            .filter(|live| {
                live.all_merges()
                    .max_by_key(|merge| merge.stamp)
                    .filter(|latest| latest.lost)
                    .and_then(|latest| self.target_move(latest.stamp.id))
                    .is_some_and(|step| sends_back(step.device))
            })
            .map(|live| live.id())
            .collect()
    }

    fn target_move(&self, event: EventId) -> Option<&TargetMove> {
        self.workforce.as_ref()?.target_moves.get(&event)
    }

    /// The pushes of `principal` to the target that lost a fork, the latest of each of its
    /// devices: their commits are not in the target, and wait for the principal to rebase them
    /// onto it and push again. One is gone once a later push from that device is on the chain.
    pub fn lost_pushes(&self, principal: PrincipalKey) -> Vec<TargetMove> {
        let mut latest: BTreeMap<DeviceKey, &TargetMove> = BTreeMap::new();
        for step in self.target_moves() {
            if step.cause != MoveCause::Push || self.principal_of(&step.device) != Some(principal) {
                continue;
            }
            latest
                .entry(step.device)
                .and_modify(|later| {
                    if step.stamp > later.stamp {
                        *later = step;
                    }
                })
                .or_insert(step);
        }
        latest
            .into_values()
            .filter(|step| self.move_lost(step.event()))
            .copied()
            .collect()
    }

    /// Whether `device` meets the policy's `[merge] requires` with its hardware, so that it may
    /// claim merges and make them.
    pub fn may_merge(&self, device: DeviceKey) -> bool {
        let Some(policy) = self.policy().map(|document| &document.policy) else {
            return false;
        };
        if policy.merge.requires.is_empty() {
            return true;
        }
        let Some(profile) = self.device_profile(device) else {
            return false;
        };
        let hardware = profile.hardware_requirements();
        policy
            .merge
            .requires
            .iter()
            .all(|requirement| hardware.contains(requirement))
    }
}

//! The target branch as a chain in the log. Every move of it names the tip it moved from: a
//! `Merged` with `onto`, or a `TargetMoved` a daemon wrote for a push of the principal. Two moves
//! with the same `onto` are a fork, and the earliest by `(hlc, EventId)` wins on every device; the
//! target is the end of the chain of winners. The chain starts at the earliest move whose `onto`
//! no move made, the tip the target had before the log named one.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::crdt::Stamp;
use crate::id::{EventId, ItemId};
use crate::identity::DeviceKey;
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
    let made: BTreeSet<CommitId> = moves.values().map(|step| step.commit).collect();
    let mut by_onto: BTreeMap<CommitId, &TargetMove> = BTreeMap::new();
    for step in moves.values() {
        by_onto
            .entry(step.onto)
            .and_modify(|earliest| {
                if step.stamp < earliest.stamp {
                    *earliest = step;
                }
            })
            .or_insert(step);
    }
    let Some(start) = moves
        .values()
        .filter(|step| !made.contains(&step.onto))
        .min_by_key(|step| step.stamp)
    else {
        return Vec::new();
    };
    let mut chain = vec![start.stamp.id];
    // A commit the chain reached already ends it, so moves that go round in a circle (no device
    // writes them, as a push only fast-forwards) cannot make it endless.
    let mut reached = BTreeSet::from([start.onto, start.commit]);
    let mut tip = start.commit;
    while let Some(next) = by_onto.get(&tip) {
        if !reached.insert(next.commit) {
            break;
        }
        chain.push(next.stamp.id);
        tip = next.commit;
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

    /// The items whose merge `device` made lost a fork while they are done: its daemon moves them
    /// back to `review`, where they wait for their merge again. Only the latest merge of an item
    /// counts, so an item merged again since stays done.
    pub fn lost_merges(&self, device: DeviceKey) -> Vec<ItemId> {
        let Some(policy) = self.policy().map(|document| &document.policy) else {
            return Vec::new();
        };
        let lost: BTreeSet<ItemId> = self
            .lost_moves(device)
            .filter_map(|step| match step.cause {
                MoveCause::Merge { item } => Some(item),
                MoveCause::Push => None,
            })
            .collect();
        lost.into_iter()
            .filter(|item| {
                self.live_items().any(|live| {
                    live.id() == *item
                        && live.state(policy) == ItemState::Done
                        && live
                            .all_merges()
                            .max_by_key(|merge| merge.stamp)
                            .is_some_and(|merge| merge.lost)
                })
            })
            .collect()
    }
}

//! Merge primitives. Every operation is commutative, associative and idempotent, so the
//! folded state does not depend on the order concurrent events arrive in.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::hlc::Hlc;
use crate::id::EventId;

/// Total order over events: HLC first, event id as the final tie break.
///
/// Authors always stamp events with an HLC greater than everything they causally follow, so
/// this order extends causal order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Stamp {
    pub hlc: Hlc,
    pub id: EventId,
}

/// Last writer wins, by [`Stamp`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lww<T> {
    value: T,
    stamp: Stamp,
}

impl<T> Lww<T> {
    pub fn new(value: T, stamp: Stamp) -> Self {
        Self { value, stamp }
    }

    pub fn value(&self) -> &T {
        &self.value
    }

    pub fn stamp(&self) -> Stamp {
        self.stamp
    }

    /// Returns whether the write won.
    pub fn set(&mut self, value: T, stamp: Stamp) -> bool {
        if stamp > self.stamp {
            self.value = value;
            self.stamp = stamp;
            true
        } else {
            false
        }
    }
}

/// An edit to an add-wins set, as carried in events.
///
/// A removal names the add events its author had observed, so a concurrent add survives it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SetChange<T> {
    Add(T),
    Remove { value: T, observed: Vec<EventId> },
}

/// Observed-remove set: an element is present while at least one add of it is unremoved.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddWinsSet<T: Ord> {
    elements: BTreeMap<T, BTreeSet<Stamp>>,
}

impl<T: Ord> Default for AddWinsSet<T> {
    fn default() -> Self {
        Self {
            elements: BTreeMap::new(),
        }
    }
}

impl<T: Ord + Clone> AddWinsSet<T> {
    pub fn apply(&mut self, change: &SetChange<T>, stamp: Stamp) {
        match change {
            SetChange::Add(value) => self.add(value.clone(), stamp),
            SetChange::Remove { value, observed } => self.remove(value, observed),
        }
    }

    pub fn add(&mut self, value: T, stamp: Stamp) {
        self.elements.entry(value).or_default().insert(stamp);
    }

    pub fn remove(&mut self, value: &T, observed: &[EventId]) {
        if let Some(tags) = self.elements.get_mut(value) {
            tags.retain(|tag| !observed.contains(&tag.id));
            if tags.is_empty() {
                self.elements.remove(value);
            }
        }
    }

    pub fn contains(&self, value: &T) -> bool {
        self.elements.contains_key(value)
    }

    pub fn values(&self) -> impl Iterator<Item = &T> {
        self.elements.keys()
    }

    pub fn is_empty(&self) -> bool {
        self.elements.is_empty()
    }

    /// The add events currently keeping `value` present. A removal must list these.
    pub fn observed(&self, value: &T) -> Vec<EventId> {
        self.elements
            .get(value)
            .map(|tags| tags.iter().map(|tag| tag.id).collect())
            .unwrap_or_default()
    }

    /// The newest add keeping `value` present.
    pub fn newest_stamp(&self, value: &T) -> Option<Stamp> {
        self.elements
            .get(value)
            .and_then(|tags| tags.last().copied())
    }

    /// Builds the change that removes `value` as currently observed.
    pub fn removal(&self, value: &T) -> SetChange<T> {
        SetChange::Remove {
            value: value.clone(),
            observed: self.observed(value),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(wall: u8) -> Stamp {
        Stamp {
            hlc: Hlc::next_after(Hlc::ZERO, u64::from(wall)).unwrap(),
            id: EventId::of(&[wall]),
        }
    }

    #[test]
    fn lww_keeps_the_greatest_stamp_in_any_order() {
        let mut forward = Lww::new("a", stamp(1));
        forward.set("b", stamp(2));
        forward.set("c", stamp(3));
        let mut backward = Lww::new("c", stamp(3));
        backward.set("b", stamp(2));
        backward.set("a", stamp(1));
        assert_eq!(forward, backward);
        assert_eq!(forward.value(), &"c");
    }

    #[test]
    fn add_wins_over_concurrent_remove() {
        let first_add = stamp(1);
        let concurrent_add = stamp(2);

        let mut author_a = AddWinsSet::default();
        author_a.add("x", first_add);
        let removal = author_a.removal(&"x");

        let mut one_order = AddWinsSet::default();
        one_order.add("x", first_add);
        one_order.add("x", concurrent_add);
        one_order.apply(&removal, stamp(3));

        let mut other_order = AddWinsSet::default();
        other_order.add("x", first_add);
        other_order.apply(&removal, stamp(3));
        other_order.add("x", concurrent_add);

        assert_eq!(one_order, other_order);
        assert!(one_order.contains(&"x"));
    }

    #[test]
    fn removal_of_everything_observed_removes() {
        let mut set = AddWinsSet::default();
        set.add("x", stamp(1));
        set.add("x", stamp(2));
        let removal = set.removal(&"x");
        set.apply(&removal, stamp(3));
        assert!(!set.contains(&"x"));
        assert!(set.is_empty());
    }

    #[test]
    fn re_adding_after_removal_restores() {
        let mut set = AddWinsSet::default();
        set.add("x", stamp(1));
        let removal = set.removal(&"x");
        set.apply(&removal, stamp(2));
        set.add("x", stamp(3));
        assert!(set.contains(&"x"));
    }
}

//! The dependency graph, resolved at query time.
//!
//! `depends_on` must be acyclic, but concurrent links can form a cycle. Rejecting one would depend
//! on arrival order, so every edge is kept and the cycle is broken here, deterministically: edges
//! are taken in `(hlc, event id)` order and an edge that would close a cycle is ignored and flagged.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::crdt::Stamp;
use crate::id::ItemId;
use crate::item::Item;
use crate::policy::StateCategory;
use crate::state::State;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Edge {
    pub from: ItemId,
    pub to: ItemId,
}

#[derive(Clone, Debug, Default)]
pub struct DependencyGraph {
    accepted: BTreeMap<ItemId, BTreeSet<ItemId>>,
    dependents: BTreeMap<ItemId, BTreeSet<ItemId>>,
    ignored: Vec<Edge>,
}

/// Why a dependency is not satisfied yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Blocker {
    pub item: ItemId,
    /// `None` when the dependency's state is unknown under the current policy.
    pub state_category: Option<StateCategory>,
}

/// What an open item is waiting for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Readiness {
    /// Every dependency is done and no session works on the item.
    Ready,
    /// Every dependency is done, and a live session holds the item.
    Claimed,
    /// Some dependency is not done yet; none was cancelled.
    Blocked,
    /// Some dependency was cancelled, so waiting will not free the item; someone has to act.
    Stranded,
}

impl DependencyGraph {
    pub fn depends_on(&self, item: ItemId) -> impl Iterator<Item = ItemId> + '_ {
        self.accepted.get(&item).into_iter().flatten().copied()
    }

    /// Items that depend on `item` ("required by").
    pub fn dependents(&self, item: ItemId) -> impl Iterator<Item = ItemId> + '_ {
        self.dependents.get(&item).into_iter().flatten().copied()
    }

    /// Edges dropped because concurrent links formed a cycle.
    pub fn ignored(&self) -> &[Edge] {
        &self.ignored
    }

    pub fn is_ignored(&self, from: ItemId, to: ItemId) -> bool {
        self.ignored.contains(&Edge { from, to })
    }

    /// Whether `from` depends on `to`, directly or through other items.
    pub fn reaches(&self, from: ItemId, to: ItemId) -> bool {
        let mut stack = vec![from];
        let mut seen = BTreeSet::new();
        while let Some(current) = stack.pop() {
            if current == to {
                return true;
            }
            if seen.insert(current) {
                stack.extend(self.depends_on(current));
            }
        }
        false
    }
}

impl State {
    pub fn dependency_graph(&self) -> DependencyGraph {
        let mut edges: Vec<(Stamp, Edge)> = Vec::new();
        for item in self.raw_items() {
            for target in item.depends_on().values() {
                let stamp = item
                    .depends_on()
                    .newest_stamp(target)
                    .expect("a present element has an add");
                edges.push((
                    stamp,
                    Edge {
                        from: item.id(),
                        to: *target,
                    },
                ));
            }
        }
        edges.sort();

        let mut graph = DependencyGraph::default();
        for (_, edge) in edges {
            if graph.reaches(edge.to, edge.from) {
                graph.ignored.push(edge);
            } else {
                graph.accepted.entry(edge.from).or_default().insert(edge.to);
                graph
                    .dependents
                    .entry(edge.to)
                    .or_default()
                    .insert(edge.from);
            }
        }
        graph
    }

    /// Dependencies of `item` that are not done yet.
    pub fn blockers(&self, graph: &DependencyGraph, item: ItemId) -> Vec<Blocker> {
        let Some(policy) = self.policy() else {
            return Vec::new();
        };
        graph
            .depends_on(item)
            .filter_map(|dependency| {
                let state_category = self
                    .item(dependency)
                    .and_then(|view| view.standing(&policy.policy).state_category());
                (state_category != Some(StateCategory::Done)).then_some(Blocker {
                    item: dependency,
                    state_category,
                })
            })
            .collect()
    }

    /// Where an open item stands with its dependencies; `None` for items that are not open.
    pub fn readiness(&self, graph: &DependencyGraph, item: &Item) -> Option<Readiness> {
        let policy = self.policy()?;
        if item.archived_at().is_some()
            || item.standing(&policy.policy).state_category() != Some(StateCategory::Open)
        {
            return None;
        }
        let blockers = self.blockers(graph, item.id());
        Some(if blockers.is_empty() {
            if self.claim(item.id()).is_some() {
                Readiness::Claimed
            } else {
                Readiness::Ready
            }
        } else if blockers
            .iter()
            .any(|blocker| blocker.state_category == Some(StateCategory::Cancelled))
        {
            Readiness::Stranded
        } else {
            Readiness::Blocked
        })
    }

    /// `ready(i) ⇔ cat(i) = open ∧ every dependency is done ∧ no live session holds i`.
    pub fn is_ready(&self, graph: &DependencyGraph, item: &Item) -> bool {
        self.readiness(graph, item) == Some(Readiness::Ready)
    }

    /// Ready items, most urgent first, oldest first within a priority. Items without a priority,
    /// because their type has none, come last.
    pub fn ready_items(&self) -> Vec<&Item> {
        let graph = self.dependency_graph();
        let mut ready: Vec<&Item> = self
            .live_items()
            .filter(|item| self.is_ready(&graph, item))
            .collect();
        ready.sort_by_key(|item| {
            (
                item.priority().is_none(),
                item.priority(),
                item.created(),
                item.id(),
            )
        });
        ready
    }
}

//! The dependency graph, resolved at query time.
//!
//! `depends_on` must be acyclic, but concurrent links can form a cycle. Rejecting one would depend
//! on arrival order, so every edge is kept and the cycle is broken here, deterministically: edges
//! are taken in `(hlc, event id)` order and an edge that would close a cycle is ignored and flagged.

use std::collections::{BTreeMap, BTreeSet};

use crate::crdt::Stamp;
use crate::id::ItemId;
use crate::item::Item;
use crate::policy::Category;
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
    pub category: Option<Category>,
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
                let category = self
                    .item(dependency)
                    .and_then(|view| view.standing(&policy.policy).category());
                (category != Some(Category::Done)).then_some(Blocker {
                    item: dependency,
                    category,
                })
            })
            .collect()
    }

    /// `ready(i) ⇔ cat(i) = open ∧ every dependency is done`. Live sessions arrive in M2.
    pub fn is_ready(&self, graph: &DependencyGraph, item: &Item) -> bool {
        let Some(policy) = self.policy() else {
            return false;
        };
        item.archived_at().is_none()
            && item.standing(&policy.policy).category() == Some(Category::Open)
            && self.blockers(graph, item.id()).is_empty()
    }

    /// Ready items, most urgent first, oldest first within a priority.
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

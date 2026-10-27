//! Items in lists: one summary per item, grouped as `wf item list` shows them.

use std::collections::BTreeMap;

use serde::Serialize;
use wf_core::graph::{DependencyGraph, Readiness};
use wf_core::id::ItemId;
use wf_core::item::{Item, ItemEntry, ItemState, Priority};
use wf_core::state::State;
use wf_core::stats::ItemStats;

use crate::cli::ItemGrouping;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ItemRefView {
    pub id: ItemId,
    #[serde(rename = "type")]
    pub item_type: String,
    pub title: String,
    pub state: ItemState,
    pub priority: Option<Priority>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ItemSummary {
    pub id: ItemId,
    #[serde(rename = "type")]
    pub item_type: String,
    pub title: String,
    pub state: ItemState,
    pub priority: Option<Priority>,
    /// `None` unless the item is open.
    pub readiness: Option<Readiness>,
    pub blocked_by: Vec<ItemRefView>,
    /// Live items waiting on this one, highest priority first.
    pub blocking: Vec<ItemRefView>,
    pub labels: Vec<String>,
    /// The item's lane, or its type's default lane.
    pub lane: Option<String>,
    /// How many steps of the body are checked off; `None` without steps.
    pub steps: Option<StepsView>,
    pub archived: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct StepsView {
    pub done: usize,
    pub total: usize,
}

pub(super) fn steps_view(item: &Item) -> Option<StepsView> {
    let (done, total) = item.steps().fold((0, 0), |(done, total), step| {
        (done + usize::from(step.done), total + 1)
    });
    (total > 0).then_some(StepsView { done, total })
}

pub fn item_ref_view(view: ItemEntry<'_>) -> ItemRefView {
    let priority = match view {
        ItemEntry::Live(item) => item.priority(),
        ItemEntry::Archived(_) => None,
    };
    ItemRefView {
        id: view.id(),
        item_type: view.item_type().to_string(),
        title: view.title().to_string(),
        state: view.state(),
        priority,
    }
}

/// One line of `wf stats`: an item that was worked on, and how that went.
#[derive(Serialize)]
pub struct ItemStatsView {
    #[serde(flatten)]
    pub item: ItemRefView,
    pub archived: bool,
    #[serde(flatten)]
    pub stats: ItemStats,
}

/// The items that had sessions, archived ones included: the most submissions first, then the
/// most change requests.
pub fn item_stats_views(state: &State) -> Vec<ItemStatsView> {
    let mut views: Vec<ItemStatsView> = state
        .items()
        .filter_map(|view| {
            let stats = state.item_stats(view.id())?;
            (stats.sessions > 0).then(|| ItemStatsView {
                item: item_ref_view(view),
                archived: matches!(view, ItemEntry::Archived(_)),
                stats,
            })
        })
        .collect();
    views.sort_by(|left, right| {
        (right.stats.submissions, right.stats.change_requests)
            .cmp(&(left.stats.submissions, left.stats.change_requests))
            .then_with(|| left.item.id.cmp(&right.item.id))
    });
    views
}

fn blocking_views(state: &State, graph: &DependencyGraph, item: &Item) -> Vec<ItemRefView> {
    let mut waiting: Vec<&Item> = graph
        .dependents(item.id())
        .filter_map(|id| match state.item(id) {
            Some(ItemEntry::Live(waiting)) => Some(waiting),
            Some(ItemEntry::Archived(_)) | None => None,
        })
        .collect();
    waiting.sort_by_key(|waiting| {
        (
            waiting.priority().is_none(),
            waiting.priority(),
            waiting.created(),
            waiting.id(),
        )
    });
    waiting
        .into_iter()
        .map(|waiting| item_ref_view(ItemEntry::Live(waiting)))
        .collect()
}

pub fn summary(state: &State, graph: &DependencyGraph, view: ItemEntry<'_>) -> ItemSummary {
    let policy = &state.policy().expect("a workforce has a policy").policy;
    let steps = match view {
        ItemEntry::Live(item) => steps_view(item),
        ItemEntry::Archived(_) => None,
    };
    let (priority, readiness, blocked_by, blocking, labels, lane, archived) = match view {
        ItemEntry::Live(item) => (
            item.priority(),
            state.readiness(graph, item),
            state
                .unfinished_dependencies(graph, item.id())
                .iter()
                .map(|dependency| {
                    item_ref_view(state.item(dependency.item).expect("dependencies exist"))
                })
                .collect(),
            blocking_views(state, graph, item),
            item.labels().values().map(ToString::to_string).collect(),
            item.effective_lane(policy).map(ToString::to_string),
            false,
        ),
        ItemEntry::Archived(_) => (None, None, Vec::new(), Vec::new(), Vec::new(), None, true),
    };
    ItemSummary {
        id: view.id(),
        item_type: view.item_type().to_string(),
        title: view.title().to_string(),
        state: view.state(),
        priority,
        readiness,
        blocked_by,
        blocking,
        labels,
        lane,
        steps,
        archived,
    }
}

/// One group of `wf item list --group-by`; `group` names the state, readiness or label the items share.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ItemGroup {
    pub group: String,
    pub items: Vec<ItemSummary>,
}

/// Open items by readiness; everything else together.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum ReadinessGroup {
    Open(Readiness),
    NotOpen,
}

fn collect_groups<Key: Ord>(
    entries: Vec<ItemSummary>,
    key: impl Fn(&ItemSummary) -> Key,
    label: impl Fn(&Key) -> String,
) -> Vec<ItemGroup> {
    let mut groups: BTreeMap<Key, Vec<ItemSummary>> = BTreeMap::new();
    for summary in entries {
        groups.entry(key(&summary)).or_default().push(summary);
    }
    groups
        .into_iter()
        .map(|(key, items)| ItemGroup {
            group: label(&key),
            items,
        })
        .collect()
}

/// A group keyed by an optional value. Values sort by their own order; items without a value
/// come last.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Assigned<T> {
    Value(T),
    Unassigned,
}

impl<T> From<Option<T>> for Assigned<T> {
    fn from(value: Option<T>) -> Self {
        value.map_or(Assigned::Unassigned, Assigned::Value)
    }
}

fn assigned_name(key: &Assigned<String>, unassigned: &str) -> String {
    match key {
        Assigned::Value(name) => name.clone(),
        Assigned::Unassigned => unassigned.to_string(),
    }
}

/// An item with several labels is listed under each of them.
fn group_by_label(entries: Vec<ItemSummary>) -> Vec<ItemGroup> {
    let mut groups: BTreeMap<Assigned<String>, Vec<ItemSummary>> = BTreeMap::new();
    for summary in entries {
        if summary.labels.is_empty() {
            groups
                .entry(Assigned::Unassigned)
                .or_default()
                .push(summary);
            continue;
        }
        for label in &summary.labels {
            groups
                .entry(Assigned::Value(label.clone()))
                .or_default()
                .push(summary.clone());
        }
    }
    groups
        .into_iter()
        .map(|(key, items)| ItemGroup {
            group: assigned_name(&key, "unlabeled"),
            items,
        })
        .collect()
}

/// Groups `entries`, keeping their order inside each group.
pub fn group_items(grouping: ItemGrouping, entries: Vec<ItemSummary>) -> Vec<ItemGroup> {
    match grouping {
        ItemGrouping::Label => group_by_label(entries),
        ItemGrouping::Type => collect_groups(
            entries,
            |summary| summary.item_type.clone(),
            |item_type| item_type.clone(),
        ),
        ItemGrouping::Lane => collect_groups(
            entries,
            |summary| Assigned::from(summary.lane.clone()),
            |lane| assigned_name(lane, "no_lane"),
        ),
        ItemGrouping::Priority => collect_groups(
            entries,
            |summary| Assigned::from(summary.priority.map(|priority| priority.to_string())),
            |priority| assigned_name(priority, "no_priority"),
        ),
        ItemGrouping::State => {
            collect_groups(entries, |summary| summary.state, |state| state.to_string())
        }
        ItemGrouping::Readiness => collect_groups(
            entries,
            |summary| match summary.readiness {
                Some(readiness) => ReadinessGroup::Open(readiness),
                None => ReadinessGroup::NotOpen,
            },
            |group| {
                match group {
                    ReadinessGroup::Open(Readiness::Ready) => "ready",
                    ReadinessGroup::Open(Readiness::Claimed) => "claimed",
                    ReadinessGroup::Open(Readiness::Blocked) => "blocked",
                    ReadinessGroup::Open(Readiness::Stranded) => "stranded",
                    ReadinessGroup::NotOpen => "not_open",
                }
                .to_string()
            },
        ),
    }
}

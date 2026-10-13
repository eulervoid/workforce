//! Items in lists: one summary per item, grouped as `wf item list` shows them.

use std::collections::BTreeMap;

use serde::Serialize;
use wf_core::body::Body;
use wf_core::graph::{DependencyGraph, Readiness};
use wf_core::id::HashId;
use wf_core::item::{Item, ItemView, Priority, Standing};
use wf_core::policy::StateCategory;
use wf_core::state::State;

use crate::cli::ItemGrouping;

#[derive(Clone, Serialize)]
pub struct ItemRefView {
    pub id: String,
    #[serde(rename = "type")]
    pub item_type: String,
    pub title: String,
    pub state: Option<String>,
    #[serde(skip)]
    pub state_category: Option<StateCategory>,
    pub priority: Option<Priority>,
}

#[derive(Clone, Serialize)]
pub struct ItemSummary {
    pub id: String,
    #[serde(rename = "type")]
    pub item_type: String,
    pub title: String,
    /// `None` when the item's type was removed from the policy.
    pub state: Option<String>,
    /// `None` when the state is unknown under the current policy.
    #[serde(skip)]
    pub state_category: Option<StateCategory>,
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

#[derive(Clone, Copy, Serialize)]
pub struct StepsView {
    pub done: usize,
    pub total: usize,
}

pub(super) fn steps_view(body: &Body) -> Option<StepsView> {
    let (done, total) = body.steps().fold((0, 0), |(done, total), step| {
        (done + usize::from(step.done), total + 1)
    });
    (total > 0).then_some(StepsView { done, total })
}

pub(super) fn standing_parts(standing: Standing<'_>) -> (Option<String>, Option<StateCategory>) {
    match standing {
        Standing::Known {
            state,
            state_category,
        } => (Some(state.to_string()), Some(state_category)),
        Standing::Unknown { state } => (state.map(ToString::to_string), None),
    }
}

fn item_ref_view(state: &State, view: ItemView<'_>) -> ItemRefView {
    let policy = &state.policy().expect("a workforce has a policy").policy;
    let (state_name, state_category) = standing_parts(view.standing(policy));
    let priority = match view {
        ItemView::Live(item) => item.priority(),
        ItemView::Archived(_) => None,
    };
    ItemRefView {
        id: view.id().to_base32(),
        item_type: view.item_type().to_string(),
        title: view.title().to_string(),
        state: state_name,
        state_category,
        priority,
    }
}

fn blocking_views(state: &State, graph: &DependencyGraph, item: &Item) -> Vec<ItemRefView> {
    let mut waiting: Vec<&Item> = graph
        .dependents(item.id())
        .filter_map(|id| match state.item(id) {
            Some(ItemView::Live(waiting)) => Some(waiting),
            Some(ItemView::Archived(_)) | None => None,
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
        .map(|waiting| item_ref_view(state, ItemView::Live(waiting)))
        .collect()
}

pub fn summary(state: &State, graph: &DependencyGraph, view: ItemView<'_>) -> ItemSummary {
    let policy = &state.policy().expect("a workforce has a policy").policy;
    let (state_name, state_category) = standing_parts(view.standing(policy));
    let steps = match view {
        ItemView::Live(item) => steps_view(&item.body()),
        ItemView::Archived(_) => None,
    };
    let (priority, readiness, blocked_by, blocking, labels, lane, archived) = match view {
        ItemView::Live(item) => (
            item.priority(),
            state.readiness(graph, item),
            state
                .unfinished_dependencies(graph, item.id())
                .iter()
                .map(|dependency| {
                    item_ref_view(
                        state,
                        state.item(dependency.item).expect("dependencies exist"),
                    )
                })
                .collect(),
            blocking_views(state, graph, item),
            item.labels().values().map(ToString::to_string).collect(),
            item.effective_lane(policy).map(ToString::to_string),
            false,
        ),
        ItemView::Archived(_) => (None, None, Vec::new(), Vec::new(), Vec::new(), None, true),
    };
    ItemSummary {
        id: view.id().to_base32(),
        item_type: view.item_type().to_string(),
        title: view.title().to_string(),
        state: state_name,
        state_category,
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
#[derive(Serialize)]
pub struct ItemGroup {
    pub group: String,
    pub items: Vec<ItemSummary>,
}

/// Which state an item sits in, ordered by state category; items with an unknown state come last.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum StateGroup {
    Known {
        state_category: StateCategory,
        state: String,
    },
    Unknown {
        state: Option<String>,
    },
}

/// Open items by readiness; everything else together.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum ReadinessGroup {
    Open(Readiness),
    NotOpen,
}

fn collect_groups<Key: Ord>(
    entries: Vec<(ItemView<'_>, ItemSummary)>,
    key: impl Fn(ItemView<'_>, &ItemSummary) -> Key,
    label: impl Fn(&Key) -> String,
) -> Vec<ItemGroup> {
    let mut groups: BTreeMap<Key, Vec<ItemSummary>> = BTreeMap::new();
    for (view, summary) in entries {
        groups.entry(key(view, &summary)).or_default().push(summary);
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
fn group_by_label(entries: Vec<(ItemView<'_>, ItemSummary)>) -> Vec<ItemGroup> {
    let mut groups: BTreeMap<Assigned<String>, Vec<ItemSummary>> = BTreeMap::new();
    for (_, summary) in entries {
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
pub fn group_items(
    state: &State,
    grouping: ItemGrouping,
    entries: Vec<(ItemView<'_>, ItemSummary)>,
) -> Vec<ItemGroup> {
    let policy = &state.policy().expect("a workforce has a policy").policy;
    match grouping {
        ItemGrouping::Label => group_by_label(entries),
        ItemGrouping::Type => collect_groups(
            entries,
            |_, summary| summary.item_type.clone(),
            |item_type| item_type.clone(),
        ),
        ItemGrouping::Lane => collect_groups(
            entries,
            |_, summary| Assigned::from(summary.lane.clone()),
            |lane| assigned_name(lane, "no_lane"),
        ),
        ItemGrouping::Priority => collect_groups(
            entries,
            |_, summary| Assigned::from(summary.priority.map(|priority| priority.to_string())),
            |priority| assigned_name(priority, "no_priority"),
        ),
        ItemGrouping::State => collect_groups(
            entries,
            |view, _| match view.standing(policy) {
                Standing::Known {
                    state,
                    state_category,
                } => StateGroup::Known {
                    state_category,
                    state: state.to_string(),
                },
                Standing::Unknown { state } => StateGroup::Unknown {
                    state: state.map(ToString::to_string),
                },
            },
            |group| match group {
                StateGroup::Known { state, .. } => state.clone(),
                StateGroup::Unknown { state } => {
                    state.clone().unwrap_or_else(|| "unknown".to_string())
                }
            },
        ),
        ItemGrouping::Readiness => collect_groups(
            entries,
            |_, summary| match summary.readiness {
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

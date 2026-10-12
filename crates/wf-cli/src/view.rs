//! What commands print. Each view is a plain serializable struct: `--json` prints it inside a
//! versioned [`Envelope`], text output is rendered from the same struct in `render`.

use std::collections::{BTreeMap, BTreeSet};

use jiff::Timestamp;
use serde::Serialize;
use wf_core::activity::{Activity, Entry};
use wf_core::autonomy::StartMode;
use wf_core::body::{Body, BodyPart, StepId};
use wf_core::crdt::SetChange;
use wf_core::event::{Change, CommentKind, Event, EventBody, Link};
use wf_core::graph::{DependencyGraph, Readiness};
use wf_core::hlc::Hlc;
use wf_core::id::{CommentId, HashId, ItemId, SessionId};
use wf_core::identity::Actor;
use wf_core::inbox::Waiting;
use wf_core::item::{Item, ItemView, Origin, Priority, Standing};
use wf_core::policy::{Field, ReviewStep, StateCategory};
use wf_core::resolve::MentionTarget;
use wf_core::review::ReviewStatus;
use wf_core::session::{CommitId, EndReason, Session, SessionRole, Verdict};
use wf_core::state::State;
use wf_daemon::proto::HeldPermission;

use crate::cli::ItemGrouping;

/// Bump when a view changes shape incompatibly. Snapshot tests pin the current shapes.
pub const SCHEMA_VERSION: u32 = 3;

#[derive(Serialize)]
pub struct Envelope<T> {
    pub schema: u32,
    pub data: T,
}

impl<T> Envelope<T> {
    pub fn new(data: T) -> Self {
        Self {
            schema: SCHEMA_VERSION,
            data,
        }
    }
}

pub fn timestamp(hlc: Hlc) -> String {
    timestamp_ms(hlc.wall_ms())
}

pub fn timestamp_ms(millis: u64) -> String {
    i64::try_from(millis)
        .ok()
        .and_then(|millis| Timestamp::from_millisecond(millis).ok())
        .map_or_else(|| "unknown".to_string(), |time| time.to_string())
}

pub fn actor_label(state: &State, actor: &Actor) -> String {
    let principal = |key: &wf_core::identity::PrincipalKey| {
        state
            .member(key)
            .map_or_else(|| key.fmt_short(), |member| format!("@{}", member.handle))
    };
    match actor {
        Actor::Principal(key) => principal(key),
        Actor::Daemon(_) => "daemon".to_string(),
        Actor::Agent { agent, session } => {
            format!(
                "{}/{}/{}",
                principal(&agent.principal),
                agent.name,
                session.short()
            )
        }
    }
}

/// `@joshua (3f9a2c)`: whose device it is and which one.
pub fn device_label(state: &State, device: wf_core::identity::DeviceKey) -> String {
    match state.principal_of(&device) {
        Some(principal) => format!(
            "{} ({})",
            actor_label(state, &Actor::Principal(principal)),
            device.fmt_short()
        ),
        None => device.fmt_short(),
    }
}

pub fn timestamp_now() -> String {
    Timestamp::now().to_string()
}

pub fn mention_label(state: &State, target: &MentionTarget) -> String {
    match target {
        MentionTarget::Principal(key) => actor_label(state, &Actor::Principal(*key)),
        MentionTarget::Agent(agent) => {
            let handle = state.member(&agent.principal).map_or_else(
                || agent.principal.fmt_short(),
                |member| member.handle.to_string(),
            );
            format!("@{handle}/{}", agent.name)
        }
    }
}

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

fn steps_view(body: &Body) -> Option<StepsView> {
    let (done, total) = body.steps().fold((0, 0), |(done, total), step| {
        (done + usize::from(step.done), total + 1)
    });
    (total > 0).then_some(StepsView { done, total })
}

fn standing_parts(standing: Standing<'_>) -> (Option<String>, Option<StateCategory>) {
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
                .blockers(graph, item.id())
                .iter()
                .map(|blocker| {
                    item_ref_view(state, state.item(blocker.item).expect("dependencies exist"))
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

#[derive(Serialize)]
pub struct LinkView {
    pub id: String,
    #[serde(rename = "type")]
    pub item_type: String,
    pub title: String,
    pub state: Option<String>,
    #[serde(skip)]
    pub state_category: Option<StateCategory>,
    /// A concurrent link formed a cycle and this edge was dropped.
    pub ignored: bool,
}

fn link_view(state: &State, id: ItemId, ignored: bool) -> LinkView {
    let view = state.item(id).expect("linked items exist");
    let policy = &state.policy().expect("a workforce has a policy").policy;
    let (state_name, state_category) = standing_parts(view.standing(policy));
    LinkView {
        id: id.to_base32(),
        item_type: view.item_type().to_string(),
        title: view.title().to_string(),
        state: state_name,
        state_category,
        ignored,
    }
}

/// An item, or a comment on it, that this item refers to.
#[derive(Serialize)]
pub struct ReferenceView {
    #[serde(flatten)]
    pub item: LinkView,
    /// The short id of the comment referenced, when the reference is to a comment.
    pub comment: Option<String>,
}

/// Where this item, or a comment on it, is referenced.
#[derive(Serialize)]
pub struct BacklinkView {
    #[serde(flatten)]
    pub from: SourceView,
    /// The short id of the comment of this item referenced, when it is one.
    pub comment: Option<String>,
}

#[derive(Serialize)]
pub struct SourceView {
    pub item: String,
    pub title: String,
    /// The comment or move note the reference is in, by id; `None` for the body.
    pub in_comment: Option<String>,
}

pub fn source_view(state: &State, source: &wf_core::resolve::Source) -> SourceView {
    SourceView {
        item: source.item.to_base32(),
        title: state
            .item(source.item)
            .map(|view| view.title().to_string())
            .unwrap_or_default(),
        in_comment: match source.origin {
            Origin::Body => None,
            Origin::Comment(id) => Some(id.to_string()),
        },
    }
}

#[derive(Serialize)]
pub struct CommentView {
    pub id: String,
    /// The short id the comment is resolved and referenced by within its item.
    pub short: String,
    pub kind: CommentKindView,
    pub by: String,
    pub at: String,
    pub text: Option<String>,
    /// The session that wrote it, such as `coder work 7c2k`, when an agent did.
    pub session: Option<String>,
    /// The step it is about.
    pub step: Option<StepRefView>,
    /// The short ids of the blockers and change requests this comment resolves.
    pub resolves: Vec<String>,
    /// Blockers and change requests only: the short id of the comment that resolved it.
    pub resolved_by: Option<String>,
}

/// What a comment is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommentKindView {
    Comment,
    Blocker,
    ChangeRequest,
}

impl From<CommentKind> for CommentKindView {
    fn from(kind: CommentKind) -> Self {
        match kind {
            CommentKind::Plain => CommentKindView::Comment,
            CommentKind::Blocker => CommentKindView::Blocker,
            CommentKind::ChangeRequest => CommentKindView::ChangeRequest,
        }
    }
}

/// What kind of thing happened to an item.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    Created,
    Comment,
    Moved,
    Edited,
    /// A body edit lost to a concurrent one.
    BodyOverwritten,
    DependsOn,
    Supersedes,
    Archived,
    Unarchived,
    SessionStarted,
    SessionEnded,
    Reviewed,
    Merged,
    /// A dependency of the item was done or cancelled.
    DependencyFinished,
    /// Another item now depends, or no longer depends, on this one.
    Dependent,
    SupersededBy,
    ReferencedIn,
    /// An event about the workforce rather than an item, such as a new agent or device; the
    /// summary says what.
    Workforce,
}

/// One thing that happened to an item.
#[derive(Serialize)]
pub struct ActivityView {
    pub at: String,
    pub by: String,
    pub kind: ActivityKind,
    /// What happened, in a few words: `moved to done`, `#k3f9ab now depends on this`.
    pub summary: String,
    /// The comment, for a comment, or the comment that says why for a move or a verdict.
    pub comment: Option<CommentView>,
    /// What an overwritten body edit said, so it is not lost silently.
    pub text: Option<String>,
}

/// One line of `wf watch`: what an event did, and to which item.
#[derive(Serialize)]
pub struct WatchLineView {
    pub event: String,
    pub item: Option<String>,
    pub title: Option<String>,
    #[serde(flatten)]
    pub activity: ActivityView,
}

/// What the `new` events did, one line for each thing, read against `events`: all that is known
/// so far, which the activity of an item needs.
pub fn watch_lines(state: &State, events: &[Event], new: &[Event]) -> Vec<WatchLineView> {
    let mut lines = Vec::new();
    for event in new {
        let live = state.item_of(event).and_then(|id| match state.item(id) {
            Some(ItemView::Live(item)) => Some(item),
            Some(ItemView::Archived(_)) | None => None,
        });
        match live {
            Some(item) => {
                let entries: Vec<Entry> = state
                    .activity(item.id(), events)
                    .into_iter()
                    .filter(|entry| entry.event == event.id())
                    .collect();
                for activity in activity_views(state, item, &entries) {
                    lines.push(WatchLineView {
                        event: event.id().to_string(),
                        item: Some(item.id().to_base32()),
                        title: Some(item.title().to_string()),
                        activity,
                    });
                }
            }
            None => lines.push(WatchLineView {
                event: event.id().to_string(),
                item: state.item_of(event).map(|id| id.to_base32()),
                title: None,
                activity: ActivityView {
                    at: timestamp(event.content.hlc),
                    by: actor_label(state, &event.content.actor),
                    kind: ActivityKind::Workforce,
                    summary: describe(state, &event.content.body),
                    comment: None,
                    text: None,
                },
            }),
        }
    }
    lines
}

/// A blocker or change request nothing has resolved yet.
#[derive(Serialize)]
pub struct OpenCommentView {
    pub id: String,
    pub kind: CommentKindView,
    pub by: String,
}

#[derive(Serialize)]
pub struct StepRefView {
    /// `None` when the step was edited since: the report then points at nothing.
    pub number: Option<usize>,
    pub text: Option<String>,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BodyPartView {
    Markdown {
        text: String,
    },
    Step {
        /// What `wf item check` takes.
        number: usize,
        done: bool,
        text: String,
    },
}

pub fn body_parts(body: &Body) -> Vec<BodyPartView> {
    let mut number = 0;
    body.parts()
        .iter()
        .map(|part| match part {
            BodyPart::Markdown(text) => BodyPartView::Markdown { text: text.clone() },
            BodyPart::Step { text, done } => {
                number += 1;
                BodyPartView::Step {
                    number,
                    done: *done,
                    text: text.clone(),
                }
            }
        })
        .collect()
}

#[derive(Serialize)]
pub struct ItemDetail {
    #[serde(flatten)]
    pub summary: ItemSummary,
    pub body: Vec<BodyPartView>,
    pub author: String,
    pub created: String,
    pub updated: String,
    /// The review level the item needs: its own, or its type's default.
    pub review: Option<String>,
    /// Where the review of the submitted work stands.
    pub review_status: Option<ReviewStatusView>,
    /// What happens when a session ends without finishing: its own level, or its type's.
    pub recovery: Option<String>,
    /// Sessions ended without finishing and the recovery level leaves the item to the principal.
    pub waits_after_failures: Option<String>,
    pub requires: Vec<String>,
    pub depends_on: Vec<LinkView>,
    pub required_by: Vec<LinkView>,
    pub supersedes: Option<LinkView>,
    pub references: Vec<ReferenceView>,
    pub referenced_by: Vec<BacklinkView>,
    pub mentions: Vec<String>,
    /// Everything that happened to the item, oldest first.
    pub activity: Vec<ActivityView>,
    pub open: Vec<OpenCommentView>,
    pub flags: Vec<String>,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ReviewStatusView {
    Awaiting {
        /// What the awaited step is: `principal`, or `agent: review, strong-model`.
        step: String,
    },
    ChangesRequested,
    Approved {
        /// `None` for work submitted without commits.
        commit: Option<String>,
    },
    Merged {
        commit: String,
    },
}

fn review_status_view(status: ReviewStatus) -> ReviewStatusView {
    match status {
        ReviewStatus::Awaiting { step } => ReviewStatusView::Awaiting {
            step: match step {
                ReviewStep::Principal => "principal".to_string(),
                ReviewStep::Agent { requires } => format!(
                    "agent: {}",
                    requires
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            },
        },
        ReviewStatus::ChangesRequested => ReviewStatusView::ChangesRequested,
        ReviewStatus::Approved { commit } => ReviewStatusView::Approved {
            commit: commit.map(|commit| commit.to_string()),
        },
        ReviewStatus::Merged { commit } => ReviewStatusView::Merged {
            commit: commit.to_string(),
        },
    }
}

fn session_label(session: &Session) -> String {
    format!(
        "{} {} {}",
        session.agent().name,
        role_name(session.role()),
        session.id().short()
    )
}

fn role_name(role: SessionRole) -> &'static str {
    match role {
        SessionRole::Work => "work",
        SessionRole::Review { .. } => "review",
        SessionRole::Supervise => "supervise",
        SessionRole::Diagnose => "diagnose",
    }
}

fn activity_views(state: &State, item: &Item, entries: &[Entry]) -> Vec<ActivityView> {
    let short_commit = |commit: &CommitId| commit.to_string()[..8].to_string();
    let label = |id: &ItemId| format!("#{}", id.short());
    let body = item.body();
    let mut views: Vec<ActivityView> = Vec::new();
    for entry in entries {
        let by = actor_label(state, &entry.by);
        let at = timestamp(entry.at);
        let (kind, summary) = match &entry.activity {
            Activity::Created => (
                ActivityKind::Created,
                format!("created the {}", item.item_type()),
            ),
            Activity::Commented {
                comment,
                kind,
                text,
                resolves,
                step,
            } => {
                let view = CommentView {
                    id: comment.to_string(),
                    short: comment.short(),
                    kind: CommentKindView::from(*kind),
                    by: by.clone(),
                    at: at.clone(),
                    text: Some(text.clone()),
                    session: item
                        .comments()
                        .get(comment)
                        .and_then(|folded| folded.session())
                        .and_then(|session| state.session(session))
                        .map(session_label),
                    step: step.map(|step| step_ref(&body, step)),
                    resolves: resolves.iter().map(HashId::short).collect(),
                    resolved_by: item
                        .comments()
                        .get(comment)
                        .and_then(|folded| folded.resolved())
                        .map(|resolved| resolved.by.short()),
                };
                views.push(ActivityView {
                    at,
                    by,
                    kind: ActivityKind::Comment,
                    summary: kind.name().replace('_', " "),
                    comment: Some(view),
                    text: None,
                });
                continue;
            }
            Activity::Moved { to, because } => {
                let mut view = ActivityView {
                    at,
                    by,
                    kind: ActivityKind::Moved,
                    summary: format!("moved to {to}"),
                    comment: None,
                    text: None,
                };
                attach_reason(&mut views, &mut view, *because);
                views.push(view);
                continue;
            }
            Activity::Reviewed {
                commit,
                verdict,
                because,
            } => {
                let on = commit
                    .as_ref()
                    .map_or_else(|| "the work without commits".to_string(), short_commit);
                let summary = match verdict {
                    Verdict::Approve => format!("approved {on}"),
                    Verdict::RequestChanges => format!("requested changes on {on}"),
                    Verdict::Escalate => format!("escalated {on} to the principal"),
                };
                let mut view = ActivityView {
                    at,
                    by,
                    kind: ActivityKind::Reviewed,
                    summary,
                    comment: None,
                    text: None,
                };
                attach_reason(&mut views, &mut view, *because);
                views.push(view);
                continue;
            }
            Activity::Edited(change) => (ActivityKind::Edited, edit_summary(&body, change)),
            Activity::BodyOverwritten { body: lost } => {
                views.push(ActivityView {
                    at,
                    by,
                    kind: ActivityKind::BodyOverwritten,
                    summary: "edited the body, overwritten by a concurrent edit".into(),
                    comment: None,
                    text: Some(lost.clone()),
                });
                continue;
            }
            Activity::DependsOn(SetChange::Add(target)) => (
                ActivityKind::DependsOn,
                format!("now depends on {}", label(target)),
            ),
            Activity::DependsOn(SetChange::Remove { value, .. }) => (
                ActivityKind::DependsOn,
                format!("no longer depends on {}", label(value)),
            ),
            Activity::Supersedes(target) => (
                ActivityKind::Supersedes,
                format!("supersedes {}", label(target)),
            ),
            Activity::Archived => (ActivityKind::Archived, "archived".to_string()),
            Activity::Unarchived => (ActivityKind::Unarchived, "unarchived".to_string()),
            Activity::SessionStarted { role, .. } => (
                ActivityKind::SessionStarted,
                match role {
                    SessionRole::Work => "started work".to_string(),
                    SessionRole::Review {
                        commit: Some(commit),
                    } => {
                        format!("started a review of {}", short_commit(commit))
                    }
                    SessionRole::Review { commit: None } => "started a review".to_string(),
                    SessionRole::Supervise => "started supervising".to_string(),
                    SessionRole::Diagnose => "started looking into what went wrong".to_string(),
                },
            ),
            Activity::SessionEnded {
                reason, moved_to, ..
            } => {
                let ended = match reason {
                    EndReason::Submitted {
                        commit: Some(commit),
                    } => {
                        format!("submitted {}", short_commit(commit))
                    }
                    EndReason::Submitted { commit: None } => "submitted without commits".into(),
                    EndReason::Reviewed => "finished the review".into(),
                    EndReason::Supervised => "finished supervising".into(),
                    EndReason::Released => "released the item".into(),
                    EndReason::TimedOut => "timed out".into(),
                    EndReason::Failed => "failed".into(),
                    EndReason::Cancelled => "was stopped".into(),
                    EndReason::Interrupted => "was interrupted".into(),
                    EndReason::Diagnosed => "said what went wrong".into(),
                };
                let summary = match moved_to {
                    Some(to) => format!("{ended} → {to}"),
                    None => ended,
                };
                (ActivityKind::SessionEnded, summary)
            }
            Activity::Merged { commit } => (
                ActivityKind::Merged,
                format!("merged as {}", short_commit(commit)),
            ),
            Activity::DependencyFinished { item, to, category } => (
                ActivityKind::DependencyFinished,
                match category {
                    StateCategory::Cancelled => {
                        format!("{} {to} · this item is stranded", label(item))
                    }
                    StateCategory::Inactive
                    | StateCategory::Open
                    | StateCategory::Active
                    | StateCategory::Review
                    | StateCategory::Done => format!("{} {to}", label(item)),
                },
            ),
            Activity::Dependent(SetChange::Add(dependent)) => (
                ActivityKind::Dependent,
                format!("{} now depends on this", label(dependent)),
            ),
            Activity::Dependent(SetChange::Remove { value, .. }) => (
                ActivityKind::Dependent,
                format!("{} no longer depends on this", label(value)),
            ),
            Activity::SupersededBy(other) => (
                ActivityKind::SupersededBy,
                format!("superseded by {}", label(other)),
            ),
            Activity::ReferencedIn {
                item: other,
                comment,
            } => (
                ActivityKind::ReferencedIn,
                match comment {
                    Some(comment) => format!("{} referenced in {}", comment.short(), label(other)),
                    None => format!("referenced in {}", label(other)),
                },
            ),
        };
        views.push(ActivityView {
            at,
            by,
            kind,
            summary,
            comment: None,
            text: None,
        });
    }
    views
}

/// A move or verdict names the comment that says why; written just before, it is shown with the
/// move or verdict instead of on its own.
fn attach_reason(
    views: &mut Vec<ActivityView>,
    view: &mut ActivityView,
    because: Option<CommentId>,
) {
    let Some(because) = because else {
        return;
    };
    let names_it = views
        .last()
        .and_then(|last| last.comment.as_ref())
        .is_some_and(|comment| {
            comment.short == because.short() && comment.kind == CommentKindView::Comment
        });
    if names_it {
        view.comment = views.pop().and_then(|last| last.comment);
    }
}

fn edit_summary(body: &Body, change: &Change) -> String {
    match change {
        Change::Title(title) => format!("renamed it to \"{title}\""),
        Change::Body(_) => "edited the body".to_string(),
        Change::Priority(Some(priority)) => format!("set priority {priority}"),
        Change::Priority(None) => "cleared the priority".to_string(),
        Change::Lane(Some(lane)) => format!("moved it to lane {lane}"),
        Change::Lane(None) => "moved it to its type's lane".to_string(),
        Change::Labels(change) => format!("labels {}", set_change(change)),
        Change::Requires(change) => format!("requires {}", set_change(change)),
        Change::Review(Some(level)) => format!("review → {level}"),
        Change::Review(None) => "review → its type's level".to_string(),
        Change::Recovery(Some(level)) => format!("recovery → {level}"),
        Change::Recovery(None) => "recovery → its type's level".to_string(),
        Change::StepDone { step, done } => {
            let verb = if *done { "checked" } else { "unchecked" };
            match step_ref(body, *step) {
                StepRefView {
                    number: Some(number),
                    text: Some(text),
                } => format!(
                    "{verb} step {number}: {}",
                    text.lines().next().unwrap_or_default()
                ),
                StepRefView { .. } => format!("{verb} a step since edited"),
            }
        }
    }
}

fn step_ref(body: &Body, step: StepId) -> StepRefView {
    match body.step(step) {
        Some(found) => StepRefView {
            number: Some(found.number),
            text: Some(found.text.to_string()),
        },
        None => StepRefView {
            number: None,
            text: None,
        },
    }
}

/// The detail of a live item. `events` is the log, for the item's activity.
pub fn item_detail(state: &State, item: &Item, events: &[Event]) -> ItemDetail {
    let graph = state.dependency_graph();
    let policy = &state.policy().expect("a workforce has a policy").policy;
    let mut flags = Vec::new();
    match item.standing(policy) {
        Standing::Known { .. } => {}
        Standing::Unknown { state: Some(name) } => flags.push(format!(
            "unknown state `{name}` under the current policy; move it to a known state"
        )),
        Standing::Unknown { state: None } => flags.push(format!(
            "unknown type `{}` under the current policy",
            item.item_type()
        )),
    }
    if item.priority().is_none()
        && policy
            .type_config(item.item_type())
            .is_some_and(|config| config.allows_field(Field::Priority))
    {
        flags.push(format!(
            "a {} needs a priority; set one with `wf item edit --priority`",
            item.item_type()
        ));
    }
    let mut depends_on = Vec::new();
    for target in item.depends_on().values() {
        let ignored = graph.is_ignored(item.id(), *target);
        if ignored {
            flags.push(format!(
                "dependency on #{} was ignored: concurrent links formed a cycle",
                target.short()
            ));
        }
        depends_on.push(link_view(state, *target, ignored));
    }
    let required_by = graph
        .dependents(item.id())
        .map(|id| link_view(state, id, false))
        .collect();
    if state.readiness(&graph, item) == Some(Readiness::Stranded) {
        flags.push(
            "stranded: a dependency was cancelled; `wf item link --remove-depends-on` frees it"
                .to_string(),
        );
    }

    let activity = activity_views(state, item, &state.activity(item.id(), events));
    let open = item
        .open_comments()
        .into_iter()
        .map(|(id, comment)| OpenCommentView {
            id: id.short(),
            kind: CommentKindView::from(comment.kind),
            by: actor_label(state, &comment.by),
        })
        .collect();

    ItemDetail {
        summary: summary(state, &graph, ItemView::Live(item)),
        body: body_parts(&item.body()),
        author: actor_label(state, item.author()),
        created: timestamp(item.created()),
        updated: timestamp(item.updated()),
        review: item.effective_review(policy).map(ToString::to_string),
        review_status: state.review_status(item.id()).map(review_status_view),
        recovery: item.effective_recovery(policy).map(ToString::to_string),
        waits_after_failures: state
            .inbox()
            .into_iter()
            .filter(|entry| entry.item == item.id())
            .find_map(|entry| match entry.waiting {
                Waiting::Recovery {
                    role,
                    failures,
                    last,
                } => Some(recovery_waits(state, item.id(), role, failures, last)),
                Waiting::Blocker { .. }
                | Waiting::Proposal
                | Waiting::HumanReview { .. }
                | Waiting::Merge { .. } => None,
            }),
        requires: item.requires().values().map(ToString::to_string).collect(),
        depends_on,
        required_by,
        supersedes: item.supersedes().map(|id| link_view(state, id, false)),
        references: state
            .references_from(item)
            .into_iter()
            .map(|reference| ReferenceView {
                item: link_view(state, reference.item(), false),
                comment: reference.comment().map(|comment| comment.short()),
            })
            .collect(),
        referenced_by: state
            .backlinks(item.id())
            .iter()
            .map(|backlink| BacklinkView {
                from: source_view(state, &backlink.from),
                comment: backlink.comment.map(|comment| comment.short()),
            })
            .collect(),
        mentions: state
            .mentions_in(item)
            .iter()
            .map(|target| mention_label(state, target))
            .collect(),
        activity,
        open,
        flags,
    }
}

#[derive(Serialize)]
pub struct MentionsView {
    pub target: String,
    pub sources: Vec<SourceView>,
}

#[derive(Serialize)]
pub struct AgentView {
    pub id: String,
    pub harness: String,
    pub model: Option<String>,
    pub capabilities: Vec<String>,
    pub lanes: Vec<String>,
    pub start: String,
    pub permissions: String,
    pub nudges: u32,
    pub park_timeout: String,
    pub max_duration: Option<String>,
    pub instructions: Option<String>,
}

pub fn agent_views(state: &State) -> Vec<AgentView> {
    state
        .agents()
        .map(|(id, definition)| AgentView {
            id: mention_label(state, &MentionTarget::Agent(id.clone())),
            harness: definition.harness.to_string(),
            model: definition.model.clone(),
            capabilities: definition
                .capabilities
                .iter()
                .map(ToString::to_string)
                .collect(),
            lanes: definition.lanes.iter().map(ToString::to_string).collect(),
            start: definition.autonomy.start.to_string(),
            permissions: definition.autonomy.permissions.to_string(),
            nudges: definition.autonomy.nudges,
            park_timeout: humantime::format_duration(definition.autonomy.park_timeout).to_string(),
            max_duration: definition
                .autonomy
                .max_duration
                .map(|duration| humantime::format_duration(duration).to_string()),
            instructions: definition.instructions.clone(),
        })
        .collect()
}

#[derive(Serialize)]
pub struct EventView {
    pub id: String,
    pub seq: u64,
    pub device: String,
    pub actor: String,
    pub at: String,
    pub kind: String,
    pub summary: String,
    pub verified: bool,
}

pub fn describe(state: &State, body: &EventBody) -> String {
    let item = |id: &ItemId| format!("#{}", id.short());
    match body {
        EventBody::Genesis(signed) => format!("workforce \"{}\"", signed.statement.name),
        EventBody::PolicySet(_) => "policy replaced".to_string(),
        EventBody::AgentDefined(defined) => format!("{} on {}", defined.name, defined.harness),
        EventBody::AgentRemoved(removed) => format!("{} removed", removed.name),
        EventBody::InviteCreated(_) => "device invite".to_string(),
        EventBody::DeviceBound(_) => "device bound".to_string(),
        EventBody::DeviceProfile(profile) => format!("device \"{}\"", profile.name),
        EventBody::ForkAnnounced(announced) => format!("forked to {}", announced.statement.to),
        EventBody::ItemCreated(created) => format!("{} \"{}\"", created.item_type, created.title),
        EventBody::ItemEdited(edited) => {
            let fields: Vec<&str> = edited
                .changes
                .iter()
                .map(|change| match change {
                    Change::Title(_) => "title",
                    Change::Body(_) => "body",
                    Change::Priority(_) => "priority",
                    Change::Lane(_) => "lane",
                    Change::Labels(_) => "labels",
                    Change::Requires(_) => "requires",
                    Change::Review(_) => "review",
                    Change::Recovery(_) => "recovery",
                    Change::StepDone { .. } => "step",
                })
                .collect();
            format!("{} {}", item(&edited.item), fields.join(", "))
        }
        EventBody::ItemMoved(moved) => match moved.by_session {
            Some(session) => format!(
                "{} -> {} for session {}",
                item(&moved.item),
                moved.to,
                session.short()
            ),
            None => format!("{} -> {}", item(&moved.item), moved.to),
        },
        EventBody::ItemLinked(linked) => match &linked.link {
            Link::DependsOn(wf_core::crdt::SetChange::Add(target)) => {
                format!("{} depends on {}", item(&linked.item), item(target))
            }
            Link::DependsOn(wf_core::crdt::SetChange::Remove { value, .. }) => {
                format!(
                    "{} no longer depends on {}",
                    item(&linked.item),
                    item(value)
                )
            }
            Link::Supersedes(target) => {
                format!("{} supersedes {}", item(&linked.item), item(target))
            }
        },
        EventBody::ItemArchived(reference) => format!("{} archived", item(&reference.item)),
        EventBody::ItemUnarchived(reference) => format!("{} unarchived", item(&reference.item)),
        EventBody::Comment(comment) => format!("{} commented", item(&comment.item)),
        EventBody::SessionStarted(started) => {
            format!("{} started on {}", started.agent, item(&started.item))
        }
        EventBody::SessionEnded(ended) => format!(
            "session {} ended: {:?}",
            ended.session.short(),
            ended.reason
        ),
        EventBody::Review(review) => {
            format!("{} reviewed: {:?}", item(&review.item), review.verdict)
        }
        EventBody::Merged(merged) => format!("{} merged as {}", item(&merged.item), merged.commit),
        EventBody::Unknown { tag, .. } => {
            let _ = state;
            format!("unknown body {tag}")
        }
    }
}

pub fn event_view(state: &State, event: &Event, verified: bool) -> EventView {
    EventView {
        id: event.id().to_string(),
        seq: event.content.seq,
        device: event.content.author.fmt_short(),
        actor: actor_label(state, &event.content.actor),
        at: timestamp(event.content.hlc),
        kind: event.content.body.name().to_string(),
        summary: describe(state, &event.content.body),
        verified,
    }
}

#[derive(Serialize)]
pub struct LogView {
    pub workforce: String,
    pub events: Vec<EventView>,
}

#[derive(Serialize)]
pub struct StatusView {
    pub name: String,
    pub id: String,
    pub members: Vec<String>,
    pub agents: usize,
    pub events: usize,
    pub pending: usize,
    pub heads: usize,
    pub archived_items: usize,
    pub forked_from: Option<String>,
    pub devices: Vec<DeviceStatus>,
    /// Whether this command was answered by the daemon of this machine, which starts the agents.
    pub daemon: bool,
    pub ready_items: usize,
    pub live_sessions: usize,
    /// Live work sessions whose item waits on a dependency or an open blocker.
    pub parked_sessions: usize,
    /// Items whose sessions did not finish and whose recovery level leaves them to the principal.
    pub after_failures: Vec<String>,
    /// What waits on the principal, as `wf inbox` lists it.
    pub waiting_on_you: usize,
    pub flagged: Vec<String>,
    /// What to do next, if anything stands out.
    pub next: Option<String>,
    pub refolded: bool,
}

/// What `wf status` knows besides the state.
pub struct StatusContext<'a> {
    pub own: Option<wf_core::identity::DeviceKey>,
    pub last_seen: &'a std::collections::BTreeMap<wf_core::identity::DeviceKey, u64>,
    pub daemon: bool,
    pub waiting_on_you: usize,
    pub refolded: bool,
}

#[derive(Serialize)]
pub struct DeviceStatus {
    pub name: String,
    pub this_device: bool,
    /// Only a running daemon knows.
    pub last_seen: Option<String>,
}

pub fn status(state: &State, context: StatusContext) -> StatusView {
    let StatusContext {
        own,
        last_seen,
        daemon,
        waiting_on_you,
        refolded,
    } = context;
    let policy = &state.policy().expect("a workforce has a policy").policy;
    let graph = state.dependency_graph();
    let mut flagged = BTreeSet::new();
    let mut archived_items = 0;
    for view in state.items() {
        match view {
            ItemView::Archived(_) => archived_items += 1,
            ItemView::Live(item) => match item.standing(policy).state_category() {
                Some(_) => {
                    if state.readiness(&graph, item) == Some(Readiness::Stranded) {
                        flagged.insert(format!(
                            "#{} \"{}\" is stranded: a dependency was cancelled",
                            item.id().short(),
                            item.title()
                        ));
                    }
                }
                None => {
                    flagged.insert(format!(
                        "#{} \"{}\" has an unknown state or type",
                        item.id().short(),
                        item.title()
                    ));
                }
            },
        }
    }
    for item in state.unstaffed_reviews() {
        flagged.insert(format!(
            "the review of #{} waits for an agent step that no agent started on this workforce can take",
            item.short()
        ));
    }
    for (id, definition) in state.agents() {
        if definition.autonomy.start != StartMode::Auto {
            continue;
        }
        if definition.lanes.is_empty() {
            flagged.insert(format!(
                "agent {0} starts automatically but has no lane, so it never takes an item; `wf agent edit {0} --lane default`",
                id.name
            ));
        }
        let hosted = state.devices().any(|device| {
            device.principal == id.principal
                && device
                    .profile
                    .is_some_and(|profile| profile.agents.contains_key(&id.name))
        });
        if !hosted {
            flagged.insert(format!(
                "agent {0} starts automatically but no device hosts it; `wf device edit --agent {0}=1`",
                id.name
            ));
        }
    }
    if let Some(error) = state.unreadable_policy() {
        flagged.insert(format!(
            "this version of wf cannot read the policy set last ({}), so it runs on the last one it could read; update wf, or replace the policy with `wf policy edit`",
            wf_core::report::report(&error)
        ));
    }
    for edge in graph.ignored() {
        flagged.insert(format!(
            "dependency #{} -> #{} ignored: concurrent links formed a cycle",
            edge.from.short(),
            edge.to.short()
        ));
    }
    for (device, seq) in state.equivocations() {
        flagged.insert(format!(
            "device {} wrote two different events with seq {seq}; it was cloned or restored from a snapshot",
            device.fmt_short()
        ));
    }
    for info in state.devices().filter(|info| info.invite_redeemed_twice) {
        flagged.insert(format!(
            "device {} was bound with an invite that two devices used; `wf fork --exclude` removes one",
            device_label(state, info.device)
        ));
    }
    let devices = state
        .devices()
        .map(|info| DeviceStatus {
            name: info
                .profile
                .map_or_else(|| info.device.fmt_short(), |profile| profile.name.clone()),
            this_device: Some(info.device) == own,
            last_seen: last_seen
                .get(&info.device)
                .map(|millis| timestamp_ms(*millis)),
        })
        .collect();
    for announcement in state.announcements() {
        flagged.insert(format!(
            "{} forked this workforce to {}; `wf fork accept {}` follows it",
            actor_label(state, &Actor::Principal(announcement.by)),
            announcement.to,
            announcement.to
        ));
    }
    let live: Vec<_> = state
        .sessions()
        .filter(|session| session.is_live())
        .collect();
    let parked_sessions = live
        .iter()
        .filter(|session| is_parked(state, &graph, session))
        .count();
    let after_failures: Vec<String> = state
        .inbox()
        .into_iter()
        .filter_map(|entry| match entry.waiting {
            Waiting::Recovery {
                role,
                failures,
                last,
            } => {
                let title = match state.item(entry.item) {
                    Some(ItemView::Live(item)) => item.title().to_string(),
                    Some(ItemView::Archived(_)) | None => "(archived)".to_string(),
                };
                Some(format!(
                    "#{} \"{title}\": {}",
                    entry.item.short(),
                    recovery_waits(state, entry.item, role, failures, last)
                ))
            }
            Waiting::Blocker { .. }
            | Waiting::Proposal
            | Waiting::HumanReview { .. }
            | Waiting::Merge { .. } => None,
        })
        .collect();
    let ready_items = state.ready_items().len();
    let auto_agents = state
        .agents()
        .any(|(_, definition)| definition.autonomy.start == StartMode::Auto);
    let next = if state.agents().next().is_none() {
        Some("add an agent: `wf agent add`".to_string())
    } else if !daemon && auto_agents {
        Some("start the daemon, which starts the agents: `wf daemon start`".to_string())
    } else if waiting_on_you > 0 {
        Some(format!("{waiting_on_you} waiting on you: `wf inbox`"))
    } else if !live.is_empty() {
        Some("follow the agents: `wf watch`".to_string())
    } else if ready_items == 0 {
        Some("add an item: `wf item add`".to_string())
    } else {
        None
    };
    StatusView {
        name: state.name().unwrap_or_default().to_string(),
        id: state
            .workforce_id()
            .map(|id| id.to_string())
            .unwrap_or_default(),
        members: state
            .members()
            .map(|(_, member)| format!("@{} ({:?})", member.handle, member.role).to_lowercase())
            .collect(),
        agents: state.agents().count(),
        events: state.event_count(),
        pending: state.pending_events().count(),
        heads: state.heads().len(),
        archived_items,
        forked_from: state.fork_point().map(|(parent, _)| parent.to_string()),
        devices,
        daemon,
        ready_items,
        live_sessions: live.len(),
        parked_sessions,
        after_failures,
        waiting_on_you,
        flagged: flagged.into_iter().collect(),
        next,
        refolded,
    }
}

/// A history line for `wf item show --history`: like [`event_view`] but with the values written,
/// so an edit that lost a concurrent write is still readable.
fn set_change<T: std::fmt::Display>(change: &wf_core::crdt::SetChange<T>) -> String {
    match change {
        wf_core::crdt::SetChange::Add(value) => format!("+{value}"),
        wf_core::crdt::SetChange::Remove { value, .. } => format!("-{value}"),
    }
}

#[derive(Serialize)]
pub struct SessionView {
    pub id: String,
    pub agent: String,
    pub item: String,
    pub title: String,
    /// `work` or `review`.
    pub role: String,
    /// `live`, `parked` while it waits for an answer, or why it ended.
    pub status: String,
    pub started: String,
    pub ended: Option<String>,
    /// What the session submitted, or the commit a review session reviews.
    pub commit: Option<String>,
    /// How many steps of the item are checked off.
    pub steps: Option<StepsView>,
}

/// Whether `session` is a live work session whose item waits on a dependency or on an open
/// blocker, such as its own question.
fn is_parked(
    state: &State,
    graph: &wf_core::graph::DependencyGraph,
    session: &wf_core::session::Session,
) -> bool {
    let blocked = match state.item(session.item()) {
        Some(ItemView::Live(item)) => item.has_open_blocker(),
        Some(ItemView::Archived(_)) | None => false,
    };
    session.is_live()
        && session.role() == SessionRole::Work
        && (blocked || !state.blockers(graph, session.item()).is_empty())
}

/// Live sessions first, then the others, each the newest first.
pub fn session_views(state: &State) -> Vec<SessionView> {
    let graph = state.dependency_graph();
    let mut sessions: Vec<_> = state.sessions().collect();
    sessions.sort_by_key(|session| (!session.is_live(), std::cmp::Reverse(session.started())));
    sessions
        .into_iter()
        .map(|session| {
            let title = match state.item(session.item()) {
                Some(ItemView::Live(item)) => item.title().to_string(),
                Some(ItemView::Archived(_)) | None => "(archived)".to_string(),
            };
            let commit = match (session.role(), session.ended().map(|ended| ended.reason)) {
                (SessionRole::Review { commit }, _) => commit.map(|commit| commit.to_string()),
                (SessionRole::Work, Some(EndReason::Submitted { commit })) => {
                    commit.map(|commit| commit.to_string())
                }
                (SessionRole::Work | SessionRole::Supervise | SessionRole::Diagnose, _) => None,
            };
            let steps = match state.item(session.item()) {
                Some(ItemView::Live(item)) => steps_view(&item.body()),
                Some(ItemView::Archived(_)) | None => None,
            };
            let parked = is_parked(state, &graph, session);
            SessionView {
                id: session.id().to_base32(),
                agent: mention_label(state, &MentionTarget::Agent(session.agent().clone())),
                item: session.item().to_base32(),
                title,
                role: role_name(session.role()).to_string(),
                status: match session.ended() {
                    Some(ended) => ended.reason.name().to_string(),
                    None if parked => "parked".to_string(),
                    None => "live".to_string(),
                },
                started: timestamp(session.started().hlc),
                ended: session.ended().map(|ended| timestamp(ended.at())),
                commit,
                steps,
            }
        })
        .collect()
}

#[derive(Serialize)]
pub struct InboxEntryView {
    /// `blocker`, `proposal`, `human_review`, `merge`, `recovery` or `permission`.
    pub kind: String,
    pub item: Option<String>,
    pub session: Option<String>,
    pub title: String,
    pub what: String,
    /// Permission requests only: why each decider of the machine passed it on, as `decider: why`.
    pub passed: Vec<String>,
}

/// What a recovery step that waits on the principal says: who ended last and how, and what to do.
pub fn recovery_waits(
    state: &State,
    item: ItemId,
    role: SessionRole,
    failures: usize,
    last: SessionId,
) -> String {
    let sessions = match (role, failures) {
        (SessionRole::Work, 1) => "a session".to_string(),
        (SessionRole::Work, _) => format!("{failures} sessions"),
        (SessionRole::Review { .. }, 1) => "a review session".to_string(),
        (SessionRole::Review { .. }, _) => format!("{failures} review sessions"),
        (SessionRole::Supervise, 1) => "a supervise session".to_string(),
        (SessionRole::Supervise, _) => format!("{failures} supervise sessions"),
        (SessionRole::Diagnose, 1) => "a diagnose session".to_string(),
        (SessionRole::Diagnose, _) => format!("{failures} diagnose sessions"),
    };
    let ended = state
        .session(last)
        .and_then(|session| {
            let reason = session.ended()?.reason.name().replace('_', " ");
            Some(format!("@{} {reason}", session.agent().name))
        })
        .unwrap_or_else(|| "the last one ended".to_string());
    let next = match role {
        SessionRole::Work | SessionRole::Diagnose => format!(
            "comment with what to do differently, or `wf session start {}`",
            item.short()
        ),
        SessionRole::Review { .. } => {
            "review it yourself, or comment to have it tried again".into()
        }
        SessionRole::Supervise => "answer the blocker, or comment to have it tried again".into(),
    };
    format!("{sessions} did not finish (last: {ended}); {next}")
}

/// What waits on the principal: what the state says, and the permission requests the daemon holds.
pub fn inbox_views(state: &State, held: &[HeldPermission]) -> Vec<InboxEntryView> {
    let policy = &state.policy().expect("a workforce has a policy").policy;
    let title_of = |item: ItemId| match state.item(item) {
        Some(ItemView::Live(live)) => live.title().to_string(),
        Some(ItemView::Archived(_)) | None => "(archived)".to_string(),
    };
    let mut entries: Vec<InboxEntryView> = state
        .inbox()
        .into_iter()
        .map(|entry| {
            let (kind, what) = match entry.waiting {
                Waiting::Blocker { comment } => (
                    "blocker",
                    format!(
                        "blocker {}: answer with `wf comment {} \"...\" --resolve {}`",
                        comment.short(),
                        entry.item.short(),
                        comment.short()
                    ),
                ),
                Waiting::Proposal => {
                    let state_name = match state.item(entry.item) {
                        Some(ItemView::Live(live)) => match live.standing(policy) {
                            Standing::Known { state, .. } => state.to_string(),
                            Standing::Unknown { state } => state.map_or_else(
                                || "an unknown state".to_string(),
                                ToString::to_string,
                            ),
                        },
                        Some(ItemView::Archived(_)) | None => "an unknown state".to_string(),
                    };
                    ("proposal", format!("{state_name}; `wf item move` it on"))
                }
                Waiting::HumanReview { commit } => (
                    "human_review",
                    match commit {
                        Some(commit) => format!(
                            "review of {}: `wf item diff`, then `wf review`",
                            &commit.to_string()[..8]
                        ),
                        None => {
                            "review of work without commits: read it, then `wf review`".to_string()
                        }
                    },
                ),
                Waiting::Merge { commit } => (
                    "merge",
                    format!(
                        "{} is ready to merge, but {} is checked out with changes",
                        &commit.to_string()[..8],
                        policy.merge.target
                    ),
                ),
                Waiting::Recovery {
                    role,
                    failures,
                    last,
                } => (
                    "recovery",
                    recovery_waits(state, entry.item, role, failures, last),
                ),
            };
            InboxEntryView {
                kind: kind.to_string(),
                item: Some(entry.item.to_base32()),
                session: None,
                title: title_of(entry.item),
                what,
                passed: Vec::new(),
            }
        })
        .collect();
    entries.extend(held.iter().map(|request| {
        let session = state.session(request.session);
        InboxEntryView {
            kind: "permission".to_string(),
            item: session.map(|session| session.item().to_base32()),
            session: Some(request.session.to_base32()),
            title: session.map_or_else(String::new, |session| title_of(session.item())),
            what: format!(
                "{} asks to {}; `wf session allow` or `deny {}`",
                session.map_or_else(
                    || "an agent".to_string(),
                    |session| session.agent().name.to_string()
                ),
                request.what,
                request.session.short()
            ),
            passed: request.passed.clone(),
        }
    }));
    entries
}

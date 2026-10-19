//! One item in full, as `wf item show` shows it: its body, links, comments and activity.

use std::collections::BTreeMap;

use serde::Serialize;
use wf_core::activity::{Activity, Entry};
use wf_core::body::{Body, BodyPart, StepId};
use wf_core::crdt::SetChange;
use wf_core::event::{Change, CommentKind, Event};
use wf_core::graph::Readiness;
use wf_core::id::{CommentId, HashId, ItemId};
use wf_core::identity::Actor;
use wf_core::inbox::{InboxKind, Waiting};
use wf_core::item::{Item, ItemState, ItemView, WrittenIn};
use wf_core::policy::{Field, ReviewStep};
use wf_core::review::ReviewStatus;
use wf_core::session::{CommitId, EndReason, SessionRole, Verdict};
use wf_core::state::State;
use wf_core::stats::ItemStats;
use wf_core::supervision::WaitsOn;

use super::item::{ItemSummary, summary};
use super::session::session_label;
use super::{ActorView, actor_view, mention_view, set_change, timestamp};

#[derive(Serialize)]
pub struct LinkView {
    pub id: String,
    #[serde(rename = "type")]
    pub item_type: String,
    pub title: String,
    pub state: ItemState,
    /// A concurrent link formed a cycle and this edge was dropped.
    pub ignored: bool,
}

fn link_view(state: &State, id: ItemId, ignored: bool) -> LinkView {
    let view = state.item(id).expect("linked items exist");
    let policy = &state.policy().expect("a workforce has a policy").policy;
    LinkView {
        id: id.to_base32(),
        item_type: view.item_type().to_string(),
        title: view.title().to_string(),
        state: view.state(policy),
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

/// An item that references this one, or a comment on it; once, however often it does so.
#[derive(Serialize)]
pub struct BacklinkView {
    pub item: String,
    pub title: String,
    /// The short id of the comment of this item referenced, when it is one.
    pub comment: Option<String>,
}

/// One [`BacklinkView`] per referencing item and comment of this item referenced.
fn backlink_views(state: &State, target: ItemId) -> Vec<BacklinkView> {
    let mut views = BTreeMap::new();
    for backlink in state.backlinks(target) {
        views
            .entry((backlink.from.item, backlink.comment))
            .or_insert_with(|| BacklinkView {
                item: backlink.from.item.to_base32(),
                title: state
                    .item(backlink.from.item)
                    .map(|view| view.title().to_string())
                    .unwrap_or_default(),
                comment: backlink.comment.map(|comment| comment.short()),
            });
    }
    views.into_values().collect()
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
        in_comment: match source.written_in {
            WrittenIn::Body => None,
            WrittenIn::Comment(id) => Some(id.to_string()),
        },
    }
}

#[derive(Serialize)]
pub struct CommentView {
    pub id: String,
    /// The short id the comment is resolved and referenced by within its item.
    pub short: String,
    pub kind: CommentKindView,
    pub by: ActorView,
    pub at: String,
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
            CommentKind::ChangeRequest | CommentKind::CheckFailure => {
                CommentKindView::ChangeRequest
            }
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
    pub by: ActorView,
    pub kind: ActivityKind,
    /// What happened, in a few words: `moved to done`, `#k3f9ab now depends on this`.
    pub summary: String,
    /// The comment, for a comment, or the comment that says why for a move or a verdict.
    pub comment: Option<CommentView>,
    /// The text that goes with what happened, shown below it: what a comment says, the step that
    /// was checked or unchecked, or what an overwritten body edit said, so it is not lost silently.
    pub text: Option<String>,
}

/// A blocker or change request nothing has resolved yet.
#[derive(Serialize)]
pub struct OpenCommentView {
    pub id: String,
    pub kind: CommentKindView,
    pub by: ActorView,
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
    pub author: ActorView,
    pub created: String,
    pub updated: String,
    /// The review level the item needs: its own, or its type's default.
    pub review: Option<String>,
    /// Where the review of the submitted work stands.
    pub review_status: Option<ReviewStatusView>,
    /// What happens when a session ends without finishing: its own level, or its type's.
    pub recovery: Option<String>,
    /// How many rounds the work went through, and how long it took.
    pub stats: ItemStats,
    /// What about the item waits on the principal, as `wf inbox` lists it, in full.
    pub waiting: Vec<WaitingView>,
    pub requires: Vec<String>,
    pub depends_on: Vec<LinkView>,
    pub required_by: Vec<LinkView>,
    pub supersedes: Option<LinkView>,
    pub references: Vec<ReferenceView>,
    pub referenced_by: Vec<BacklinkView>,
    pub mentions: Vec<ActorView>,
    /// Everything that happened to the item, oldest first.
    pub activity: Vec<ActivityView>,
    pub open: Vec<OpenCommentView>,
    pub flags: Vec<String>,
}

/// Something about the item that waits on the principal, with what it points to, so nothing has
/// to be looked up.
#[derive(Serialize)]
pub struct WaitingView {
    pub kind: InboxKind,
    /// A few words that head it: `merge waits`, `blocker`, `2 sessions failed`.
    pub heading: String,
    /// What `wf inbox` says about it.
    pub what: String,
    /// What it points to, each a paragraph of markdown: the daemon's comment that names the
    /// changed files of a held merge, how the last failed session ended, who a blocker is from
    /// and what it says.
    pub points_to: Vec<String>,
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

pub(super) fn activity_views(state: &State, item: &Item, entries: &[Entry]) -> Vec<ActivityView> {
    let short_commit = |commit: &CommitId| commit.short();
    let label = |id: &ItemId| format!("#{}", id.short());
    let body = item.body();
    let mut views: Vec<ActivityView> = Vec::new();
    for entry in entries {
        let by = actor_view(state, &entry.by);
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
                    summary: kind.label(),
                    comment: Some(view),
                    text: Some(text.clone()),
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
            Activity::Edited(change) => {
                let (summary, text) = edit_summary(&body, change);
                views.push(ActivityView {
                    at,
                    by,
                    kind: ActivityKind::Edited,
                    summary,
                    comment: None,
                    text,
                });
                continue;
            }
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
                reason,
                moved_to,
                because,
                ..
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
                    EndReason::MachineFault(fault) => {
                        format!("was ended by the machine: {}", fault.label())
                    }
                };
                let summary = match moved_to {
                    Some(to) => format!("{ended} → {to}"),
                    None => ended,
                };
                let mut view = ActivityView {
                    at,
                    by,
                    kind: ActivityKind::SessionEnded,
                    summary,
                    comment: None,
                    text: None,
                };
                attach_reason(&mut views, &mut view, *because);
                views.push(view);
                continue;
            }
            Activity::Merged { commit } => (
                ActivityKind::Merged,
                format!("merged as {}", short_commit(commit)),
            ),
            Activity::DependencyFinished { item, to } => (
                ActivityKind::DependencyFinished,
                match to {
                    ItemState::Cancelled => {
                        format!("{} {to} · this item is stranded", label(item))
                    }
                    ItemState::Draft
                    | ItemState::Open
                    | ItemState::Active
                    | ItemState::Review
                    | ItemState::Done => format!("{} {to}", label(item)),
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

/// A move, verdict or end of a session names the comment that says why; written just before, it is
/// shown with it instead of on its own.
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
    if names_it && let Some(reason) = views.pop() {
        view.comment = reason.comment;
        view.text = reason.text;
    }
}

/// What an edit did, in a few words, and the text that goes with it: the step, for a step that was
/// checked or unchecked.
fn edit_summary(body: &Body, change: &Change) -> (String, Option<String>) {
    let summary = match change {
        Change::Title(title) => format!("renamed it to \"{title}\""),
        Change::Body(_) => "edited the body".to_string(),
        Change::Priority(Some(priority)) => format!("set priority {priority}"),
        Change::Priority(None) => "cleared the priority".to_string(),
        Change::Lane(Some(lane)) => format!("moved it to lane {lane}"),
        Change::Lane(None) => "moved it to its type's lane".to_string(),
        Change::Labels(change) => format!("labels {}", set_change(change)),
        Change::Requires(change) => format!("requires {}", set_change(change)),
        Change::ReviewLevel(Some(level)) => format!("review → {level}"),
        Change::ReviewLevel(None) => "review → its type's level".to_string(),
        Change::RecoveryLevel(Some(level)) => format!("recovery → {level}"),
        Change::RecoveryLevel(None) => "recovery → its type's level".to_string(),
        Change::StepDone { step, done } => {
            let verb = if *done { "checked" } else { "unchecked" };
            return match step_ref(body, *step) {
                StepRefView {
                    number: Some(number),
                    text,
                } => (format!("{verb} step {number}"), text),
                StepRefView { .. } => (format!("{verb} a step since edited"), None),
            };
        }
    };
    (summary, None)
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

/// What about `item` waits on the principal, each with what it points to, taken from the item's
/// activity `entries` and its sessions.
fn waiting_views(state: &State, item: &Item, entries: &[Entry]) -> Vec<WaitingView> {
    let comments = || {
        entries.iter().filter_map(|entry| {
            let Activity::Commented { comment, text, .. } = &entry.activity else {
                return None;
            };
            Some((entry, *comment, text.as_str()))
        })
    };
    state
        .inbox()
        .into_iter()
        .filter(|entry| entry.item == item.id())
        .map(|entry| {
            let (heading, points_to) = match entry.waiting {
                Waiting::Blocker { comment } => {
                    let from = comments().find(|(_, id, _)| *id == comment);
                    let waits_on = state
                        .open_blockers_of(item)
                        .into_iter()
                        .find(|blocker| blocker.comment == comment)
                        .map(|blocker| match blocker.waits_on {
                            WaitsOn::Principal(key) => state.actor_label(&Actor::Principal(key)),
                            WaitsOn::Agent(agent) => agent.name.to_string(),
                            WaitsOn::Supervisor => "a supervisor".to_string(),
                        });
                    let mut points_to = Vec::new();
                    if let Some((entry, _, text)) = from {
                        let by = state.actor_label(&entry.by);
                        points_to.push(match waits_on {
                            Some(waits_on) => format!("from {by}, waits on {waits_on}:"),
                            None => format!("from {by}:"),
                        });
                        points_to.push(quoted(text));
                    }
                    ("blocker".to_string(), points_to)
                }
                Waiting::Proposal => ("proposal".to_string(), Vec::new()),
                Waiting::PrincipalReview { .. } => ("review".to_string(), Vec::new()),
                Waiting::Merge { .. } => {
                    // Only a comment on this round's merge: one from before the latest submission
                    // was about an earlier round, and the daemon may not have tried this one yet.
                    let submitted = state.submission(item.id()).map(|(_, stamp)| stamp.hlc);
                    let named = comments()
                        .rev()
                        .filter(|(entry, _, _)| submitted.is_some_and(|at| entry.at > at))
                        .find(|(entry, _, _)| matches!(entry.by, Actor::Daemon(_)))
                        .map(|(_, _, text)| text.to_string());
                    ("merge waits".to_string(), named.into_iter().collect())
                }
                Waiting::Recovery {
                    role,
                    failures,
                    last,
                } => {
                    let sessions = match role {
                        SessionRole::Work => "session",
                        SessionRole::Review { .. } => "review session",
                        SessionRole::Supervise => "supervise session",
                        SessionRole::Diagnose => "diagnose session",
                    };
                    let heading = match failures {
                        1 => format!("a {sessions} failed"),
                        failures => format!("{failures} {sessions}s failed"),
                    };
                    let mut points_to = Vec::new();
                    if let Some(session) = state.session(last) {
                        let ended = session
                            .ended()
                            .map_or_else(|| "ended".to_string(), |ended| ended.reason.label());
                        points_to.push(format!("the last, {}: {ended}", session_label(session)));
                        if let Some((_, _, text)) = comments().rev().find(|(entry, _, _)| {
                            matches!(entry.by, Actor::Agent { session, .. } if session == last)
                        })
                        {
                            points_to.push(format!("its last comment:\n\n{}", quoted(text)));
                        }
                    }
                    (heading, points_to)
                }
            };
            WaitingView {
                kind: entry.waiting.kind(),
                heading,
                what: state.what_waits(&entry),
                points_to,
            }
        })
        .collect()
}

/// `text` as a markdown quote.
fn quoted(text: &str) -> String {
    text.trim_end()
        .lines()
        .map(|line| format!("> {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The detail of a live item. `events` is the log, for the item's activity.
pub fn item_detail(state: &State, item: &Item, events: &[Event]) -> ItemDetail {
    let graph = state.dependency_graph();
    let policy = &state.policy().expect("a workforce has a policy").policy;
    let mut flags = Vec::new();
    if let Some(name) = item.unknown_state() {
        flags.push(format!(
            "unknown state `{name}`, so it counts as a draft; `wf item open` or `wf item cancel` it"
        ));
    }
    match policy.type_config(item.item_type()) {
        None => flags.push(format!(
            "unknown type `{}` under the current policy",
            item.item_type()
        )),
        Some(config) if !config.uses(item.state(policy)) => flags.push(format!(
            "a {} is no longer `{}` under the current policy; move it on",
            item.item_type(),
            item.state(policy)
        )),
        Some(_) => {}
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
    if state.unstaffed_reviews().contains(&item.id()) {
        flags.push(
            "the review waits for an agent step that no agent started on this workforce can take"
                .to_string(),
        );
    }

    let entries = state.activity(item.id(), events);
    let activity = activity_views(state, item, &entries);
    let open = item
        .open_comments()
        .into_iter()
        .map(|(id, comment)| OpenCommentView {
            id: id.short(),
            kind: CommentKindView::from(comment.kind),
            by: actor_view(state, &comment.by),
        })
        .collect();

    ItemDetail {
        summary: summary(state, &graph, ItemView::Live(item)),
        body: body_parts(&item.body()),
        author: actor_view(state, item.author()),
        created: timestamp(item.created()),
        updated: timestamp(item.updated()),
        stats: state
            .item_stats(item.id())
            .expect("a live item of the state has stats"),
        review: item.effective_review_level(policy).map(ToString::to_string),
        review_status: state.review_status(item.id()).map(review_status_view),
        recovery: item
            .effective_recovery_level(policy)
            .map(ToString::to_string),
        waiting: waiting_views(state, item, &entries),
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
        referenced_by: backlink_views(state, item.id()),
        mentions: state
            .mentions_in(item)
            .iter()
            .map(|target| mention_view(state, target))
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use wf_core::autonomy::StartMode;
    use wf_core::event::{DeviceProfile, EventBody};
    use wf_core::id::SessionId;
    use wf_core::name::AgentName;
    use wf_core::testing::*;

    use super::*;

    const UNSTAFFED: &str =
        "the review waits for an agent step that no agent started on this workforce can take";

    fn flags(state: &State, item: ItemId) -> Vec<String> {
        let ItemView::Live(item) = state.item(item).unwrap() else {
            unreachable!("the item is live")
        };
        item_detail(state, item, &[]).flags
    }

    #[test]
    fn a_check_failure_is_shown_as_a_change_request() {
        assert_eq!(
            CommentKindView::from(CommentKind::CheckFailure),
            CommentKindView::ChangeRequest
        );
    }

    #[test]
    fn a_review_no_hosted_agent_can_take_is_flagged_on_the_item() {
        let fixture = Fixture::new(1);
        let mut state = fixture.state();
        let mut now = 2_000;
        let mut apply = |state: &mut State, body| {
            now += 1;
            fixture.apply(state, 0, now, body)
        };
        apply(&mut state, define_agent_with("coder", &[]));
        let EventBody::AgentDefined(mut reviewer) =
            define_agent_with("reviewer", &["review", "strong-model"])
        else {
            unreachable!("`define_agent_with` makes an `AgentDefined`")
        };
        reviewer.autonomy.start = StartMode::Auto;
        apply(&mut state, EventBody::AgentDefined(reviewer));
        let item: ItemId = apply(&mut state, create_reviewed("task", "work", "standard"))
            .id()
            .into();
        let session: SessionId = apply(&mut state, start_session("coder", item)).id().into();
        apply(&mut state, moved(item, ItemState::Review));
        apply(
            &mut state,
            end_session(session, EndReason::Submitted { commit: None }),
        );
        assert_eq!(state.unstaffed_reviews(), [item]);
        assert!(flags(&state, item).contains(&UNSTAFFED.to_string()));

        apply(
            &mut state,
            EventBody::DeviceProfile(DeviceProfile {
                name: "laptop".into(),
                hardware: String::new(),
                agents: BTreeMap::from([(AgentName::new("reviewer").unwrap(), 1)]),
                max_sessions: 1,
            }),
        );
        assert!(!flags(&state, item).contains(&UNSTAFFED.to_string()));
    }
}

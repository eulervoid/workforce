//! Events, as `wf log` and the dashboard show them.

use std::collections::HashMap;

use jiff::Timestamp;
use serde::Serialize;
use wf_core::activity::Entry;
use wf_core::event::{Change, Event, EventBody, Link};
use wf_core::id::{CommentId, HashId, ItemId, SessionId};
use wf_core::item::ItemEntry;
use wf_core::session::{EndReason, Verdict};
use wf_core::state::State;

use super::detail::{ActivityKind, ActivityView, activity_views};
use super::timestamp;

/// One line of `wf log --follow` and the event stream of the dashboard: what an event did, and to which item.
#[derive(Serialize)]
pub struct EventLineView {
    pub event: String,
    pub item: Option<ItemId>,
    pub title: Option<String>,
    #[serde(flatten)]
    pub activity: ActivityView,
}

/// What the `new` events did, one line for each thing, read against `events`: all that is known
/// so far, which the activity of an item needs.
pub fn event_lines(state: &State, events: &[Event], new: &[Event]) -> Vec<EventLineView> {
    let mut lines = Vec::new();
    // The activity of an item is worked out from all events: once for each item, not each event.
    let mut activities: HashMap<ItemId, Vec<Entry>> = HashMap::new();
    for event in new {
        let live = state.item_of(event).and_then(|id| match state.item(id) {
            Some(ItemEntry::Live(item)) => Some(item),
            Some(ItemEntry::Archived(_)) | None => None,
        });
        match live {
            Some(item) => {
                let entries: Vec<Entry> = activities
                    .entry(item.id())
                    .or_insert_with(|| state.activity(item.id(), events))
                    .iter()
                    .filter(|entry| entry.event == event.id())
                    .cloned()
                    .collect();
                for activity in activity_views(state, item, &entries) {
                    lines.push(EventLineView {
                        event: event.id().to_string(),
                        item: Some(item.id()),
                        title: Some(item.title().to_string()),
                        activity,
                    });
                }
            }
            None => lines.push(EventLineView {
                event: event.id().to_string(),
                item: state.item_of(event),
                title: None,
                activity: ActivityView {
                    at: timestamp(event.content.hlc),
                    by: super::actor_view(state, &event.content.actor),
                    kind: ActivityKind::Workforce,
                    summary: describe(&event.content.body),
                    comment: None,
                    text: None,
                },
            }),
        }
    }
    lines
}

/// One event of `wf log`.
#[derive(Serialize)]
pub struct EventView {
    pub id: String,
    pub seq: u64,
    pub device: String,
    pub actor: String,
    pub at: Timestamp,
    pub kind: String,
    /// The item the event is about, and its title.
    pub item: Option<ItemId>,
    pub title: Option<String>,
    pub summary: String,
    /// How a review or a session came out.
    pub outcome: Option<OutcomeView>,
    pub verified: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeView {
    Reviewed {
        /// The commit judged; `None` for work submitted without commits.
        commit: Option<String>,
        verdict: VerdictView,
        /// The text of the comment that says why.
        because: Option<String>,
    },
    SessionEnded {
        session: SessionId,
        /// `submitted`, `released`, `failed` and so on.
        reason: &'static str,
        /// The commit submitted, for a submission with commits.
        commit: Option<String>,
        because: Option<String>,
    },
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictView {
    Approve,
    RequestChanges,
    Escalate,
}

impl From<Verdict> for VerdictView {
    fn from(verdict: Verdict) -> Self {
        match verdict {
            Verdict::Approve => VerdictView::Approve,
            Verdict::RequestChanges => VerdictView::RequestChanges,
            Verdict::Escalate => VerdictView::Escalate,
        }
    }
}

pub fn describe(body: &EventBody) -> String {
    let item = |id: &ItemId| format!("#{}", id.short());
    match body {
        EventBody::Genesis(signed) => format!("workforce \"{}\"", signed.statement.name),
        EventBody::PolicySet(_) | EventBody::BoundPolicySet(_) => "policy replaced".to_string(),
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
                    Change::ReviewLevel(_) => "review",
                    Change::RecoveryLevel(_) => "recovery",
                    Change::StepDone { .. } => "step",
                })
                .collect();
            format!("{} {}", item(&edited.item), fields.join(", "))
        }
        EventBody::ItemMoved(moved) => match moved.by_session {
            Some(session) => format!(
                "{} -> {} for session {}",
                item(&moved.item),
                moved.to.state(),
                session.short()
            ),
            None => format!("{} -> {}", item(&moved.item), moved.to.state()),
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
        EventBody::Commented(comment) => format!("{} commented", item(&comment.item)),
        EventBody::SessionStarted(started) => {
            format!("{} started on {}", started.agent, item(&started.item))
        }
        EventBody::SessionEnded(ended) => format!(
            "session {} ended: {}",
            ended.session.short(),
            ended.reason.name()
        ),
        EventBody::Reviewed(review) => {
            let verdict = match review.verdict {
                Verdict::Approve => "approved",
                Verdict::RequestChanges => "changes requested",
                Verdict::Escalate => "escalated",
            };
            format!("{} reviewed: {verdict}", item(&review.item))
        }
        EventBody::Merged(merged) => format!("{} merged as {}", item(&merged.item), merged.commit),
        EventBody::Unknown { tag, .. } => {
            format!("unknown body {tag}")
        }
    }
}

/// `comments` holds the text of the comments in the log, for the comment that says why.
pub fn event_view(
    state: &State,
    event: &Event,
    verified: bool,
    comments: &HashMap<CommentId, String>,
) -> EventView {
    let item = state.item_of(event);
    let because = |comment: Option<CommentId>| comment.and_then(|id| comments.get(&id).cloned());
    let outcome = match &event.content.body {
        EventBody::Reviewed(review) => Some(OutcomeView::Reviewed {
            commit: review.commit.map(|commit| commit.to_string()),
            verdict: review.verdict.into(),
            because: because(review.because),
        }),
        EventBody::SessionEnded(ended) => Some(OutcomeView::SessionEnded {
            session: ended.session,
            reason: ended.reason.name(),
            commit: match ended.reason {
                EndReason::Submitted { commit } => commit.map(|commit| commit.to_string()),
                EndReason::Reviewed
                | EndReason::Supervised
                | EndReason::Released
                | EndReason::TimedOut
                | EndReason::Failed
                | EndReason::Cancelled
                | EndReason::Interrupted
                | EndReason::Diagnosed
                | EndReason::MachineFault(_) => None,
            },
            because: because(ended.because),
        }),
        EventBody::Genesis(_)
        | EventBody::PolicySet(_)
        | EventBody::BoundPolicySet(_)
        | EventBody::AgentDefined(_)
        | EventBody::AgentRemoved(_)
        | EventBody::InviteCreated(_)
        | EventBody::DeviceBound(_)
        | EventBody::DeviceProfile(_)
        | EventBody::ForkAnnounced(_)
        | EventBody::ItemCreated(_)
        | EventBody::ItemEdited(_)
        | EventBody::ItemMoved(_)
        | EventBody::ItemLinked(_)
        | EventBody::ItemArchived(_)
        | EventBody::ItemUnarchived(_)
        | EventBody::Commented(_)
        | EventBody::SessionStarted(_)
        | EventBody::Merged(_)
        | EventBody::Unknown { .. } => None,
    };
    EventView {
        id: event.id().to_string(),
        seq: event.content.seq,
        device: event.content.author.fmt_short(),
        actor: state.actor_label(&event.content.actor),
        at: timestamp(event.content.hlc),
        kind: event.content.body.name().to_string(),
        item,
        title: item
            .and_then(|id| state.item(id))
            .map(|view| view.title().to_string()),
        summary: describe(&event.content.body),
        outcome,
        verified,
    }
}

/// The text of each comment in `events`.
pub fn comment_texts(events: &[Event]) -> HashMap<CommentId, String> {
    events
        .iter()
        .filter_map(|event| {
            let EventBody::Commented(comment) = &event.content.body else {
                return None;
            };
            Some((event.id().into(), comment.text_with_lines()))
        })
        .collect()
}

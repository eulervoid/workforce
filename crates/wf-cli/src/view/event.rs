//! Events, as `wf log` and `wf watch` show them.

use serde::Serialize;
use wf_core::activity::Entry;
use wf_core::event::{Change, Event, EventBody, Link};
use wf_core::id::{HashId, ItemId};
use wf_core::item::ItemView;
use wf_core::state::State;

use super::detail::{ActivityKind, ActivityView, activity_views};
use super::{actor_label, timestamp};

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
                    by: super::actor_view(state, &event.content.actor),
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
        EventBody::Commented(comment) => format!("{} commented", item(&comment.item)),
        EventBody::SessionStarted(started) => {
            format!("{} started on {}", started.agent, item(&started.item))
        }
        EventBody::SessionEnded(ended) => format!(
            "session {} ended: {:?}",
            ended.session.short(),
            ended.reason
        ),
        EventBody::Reviewed(review) => {
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

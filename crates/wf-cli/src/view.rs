//! What commands print. Each view is a plain serializable struct: `--json` prints it inside a
//! versioned [`Envelope`], text output is rendered from the same struct in `render`.

use std::collections::BTreeSet;

use jiff::Timestamp;
use serde::Serialize;
use wf_core::event::{Change, Event, EventBody, Link};
use wf_core::graph::{Blocker, DependencyGraph};
use wf_core::hlc::Hlc;
use wf_core::id::{EventId, HashId, ItemId};
use wf_core::identity::Actor;
use wf_core::item::{Item, ItemView, Origin, Standing};
use wf_core::policy::{Category, Field};
use wf_core::resolve::MentionTarget;
use wf_core::state::State;

/// Bump when a view changes shape incompatibly. Snapshot tests pin the current shapes.
pub const SCHEMA_VERSION: u32 = 1;

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
        Actor::Human(key) => principal(key),
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
            actor_label(state, &Actor::Human(principal)),
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
        MentionTarget::Principal(key) => actor_label(state, &Actor::Human(*key)),
        MentionTarget::Agent(agent) => {
            let handle = state.member(&agent.principal).map_or_else(
                || agent.principal.fmt_short(),
                |member| member.handle.to_string(),
            );
            format!("@{handle}/{}", agent.name)
        }
    }
}

#[derive(Serialize)]
pub struct BlockerView {
    pub id: String,
    #[serde(rename = "type")]
    pub item_type: String,
    pub title: String,
    pub state: Option<String>,
    pub category: Option<String>,
}

#[derive(Serialize)]
pub struct ItemSummary {
    pub id: String,
    #[serde(rename = "type")]
    pub item_type: String,
    pub title: String,
    /// `None` when the item's type was removed from the policy.
    pub state: Option<String>,
    /// `None` when the state is unknown under the current policy.
    pub category: Option<String>,
    pub priority: Option<String>,
    pub ready: bool,
    pub blocked_by: Vec<BlockerView>,
    pub archived: bool,
}

fn standing_parts(standing: Standing<'_>) -> (Option<String>, Option<String>) {
    match standing {
        Standing::Known { state, category } => {
            (Some(state.to_string()), Some(category.to_string()))
        }
        Standing::Unknown { state } => (state.map(ToString::to_string), None),
    }
}

fn blocker_view(state: &State, blocker: &Blocker) -> BlockerView {
    let view = state.item(blocker.item).expect("dependencies exist");
    let policy = &state.policy().expect("a workforce has a policy").policy;
    let (state_name, category) = standing_parts(view.standing(policy));
    BlockerView {
        id: blocker.item.to_base32(),
        item_type: view.item_type().to_string(),
        title: view.title().to_string(),
        state: state_name,
        category,
    }
}

pub fn summary(state: &State, graph: &DependencyGraph, view: ItemView<'_>) -> ItemSummary {
    let policy = &state.policy().expect("a workforce has a policy").policy;
    let (state_name, category) = standing_parts(view.standing(policy));
    let (priority, ready, blocked_by, archived) = match view {
        ItemView::Live(item) => (
            item.priority().map(|priority| priority.to_string()),
            state.is_ready(graph, item),
            state
                .blockers(graph, item.id())
                .iter()
                .map(|blocker| blocker_view(state, blocker))
                .collect(),
            false,
        ),
        ItemView::Archived(_) => (None, false, Vec::new(), true),
    };
    ItemSummary {
        id: view.id().to_base32(),
        item_type: view.item_type().to_string(),
        title: view.title().to_string(),
        state: state_name,
        category,
        priority,
        ready,
        blocked_by,
        archived,
    }
}

#[derive(Serialize)]
pub struct LinkView {
    pub id: String,
    #[serde(rename = "type")]
    pub item_type: String,
    pub title: String,
    pub state: Option<String>,
    pub category: Option<String>,
    /// A concurrent link formed a cycle and this edge was dropped.
    pub ignored: bool,
}

fn link_view(state: &State, id: ItemId, ignored: bool) -> LinkView {
    let view = state.item(id).expect("linked items exist");
    let policy = &state.policy().expect("a workforce has a policy").policy;
    let (state_name, category) = standing_parts(view.standing(policy));
    LinkView {
        id: id.to_base32(),
        item_type: view.item_type().to_string(),
        title: view.title().to_string(),
        state: state_name,
        category,
        ignored,
    }
}

#[derive(Serialize)]
pub struct SourceView {
    pub item: String,
    pub title: String,
    /// `body`, or the id of the comment or transition note.
    pub origin: String,
}

pub fn source_view(state: &State, source: &wf_core::resolve::Source) -> SourceView {
    SourceView {
        item: source.item.to_base32(),
        title: state
            .item(source.item)
            .map(|view| view.title().to_string())
            .unwrap_or_default(),
        origin: match source.origin {
            Origin::Body => "body".to_string(),
            Origin::Note(id) => id.to_string(),
        },
    }
}

#[derive(Serialize)]
pub struct NoteView {
    pub id: String,
    pub kind: String,
    pub by: String,
    pub at: String,
    pub text: Option<String>,
}

#[derive(Serialize)]
pub struct ItemDetail {
    #[serde(flatten)]
    pub summary: ItemSummary,
    pub body: String,
    pub author: String,
    pub created: String,
    pub updated: String,
    pub lane: Option<String>,
    pub labels: Vec<String>,
    pub requires: Vec<String>,
    pub depends_on: Vec<LinkView>,
    pub required_by: Vec<LinkView>,
    pub supersedes: Option<LinkView>,
    pub references: Vec<LinkView>,
    pub referenced_by: Vec<SourceView>,
    pub mentions: Vec<String>,
    pub notes: Vec<NoteView>,
    pub flags: Vec<String>,
}

/// Text of a comment or transition note, read from the event that carries it.
pub fn note_text(event: &Event) -> Option<String> {
    match &event.content.body {
        EventBody::Comment(comment) => Some(comment.body.clone()),
        EventBody::ItemTransitioned(transition) => transition.note.clone(),
        _ => None,
    }
}

pub fn item_detail(
    state: &State,
    item: &Item,
    load_event: impl Fn(&EventId) -> Option<Event>,
) -> ItemDetail {
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
    let blockers = state.blockers(&graph, item.id());
    if blockers
        .iter()
        .any(|blocker| blocker.category == Some(Category::Cancelled))
    {
        flags.push("a dependency was cancelled; it does not unblock this item".to_string());
    }

    let notes = item
        .notes()
        .iter()
        .map(|(id, note)| NoteView {
            id: id.to_string(),
            kind: format!("{:?}", note.kind).to_lowercase(),
            by: actor_label(state, &note.by),
            at: timestamp(note.at),
            text: load_event(id).as_ref().and_then(note_text),
        })
        .collect::<Vec<_>>();
    let mut notes = notes;
    notes.sort_by(|left, right| left.at.cmp(&right.at).then(left.id.cmp(&right.id)));

    ItemDetail {
        summary: summary(state, &graph, ItemView::Live(item)),
        body: item.body().text().to_string(),
        author: actor_label(state, item.author()),
        created: timestamp(item.created()),
        updated: timestamp(item.updated()),
        lane: item.effective_lane(policy).map(ToString::to_string),
        labels: item.labels().values().map(ToString::to_string).collect(),
        requires: item.requires().values().map(ToString::to_string).collect(),
        depends_on,
        required_by,
        supersedes: item.supersedes().map(|id| link_view(state, id, false)),
        references: state
            .references_from(item)
            .into_iter()
            .map(|id| link_view(state, id, false))
            .collect(),
        referenced_by: state
            .backlinks(item.id())
            .iter()
            .map(|source| source_view(state, source))
            .collect(),
        mentions: state
            .mentions_in(item)
            .iter()
            .map(|target| mention_label(state, target))
            .collect(),
        notes,
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
                })
                .collect();
            format!("{} {}", item(&edited.item), fields.join(", "))
        }
        EventBody::ItemTransitioned(transition) => {
            format!("{} -> {}", item(&transition.item), transition.to)
        }
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
    pub items_by_category: std::collections::BTreeMap<String, usize>,
    pub archived_items: usize,
    pub flagged: Vec<String>,
    pub refolded: bool,
}

pub fn status(state: &State, refolded: bool) -> StatusView {
    let policy = &state.policy().expect("a workforce has a policy").policy;
    let graph = state.dependency_graph();
    let mut by_category = std::collections::BTreeMap::new();
    let mut flagged = BTreeSet::new();
    let mut archived_items = 0;
    for view in state.items() {
        match view {
            ItemView::Archived(_) => archived_items += 1,
            ItemView::Live(item) => match item.standing(policy).category() {
                Some(category) => *by_category.entry(category.to_string()).or_insert(0) += 1,
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
        items_by_category: by_category,
        archived_items,
        flagged: flagged.into_iter().collect(),
        refolded,
    }
}

/// A history line for `wf item show --history`: like [`event_view`] but with the values written,
/// so an edit that lost a concurrent write is still readable.
pub fn history_entry(state: &State, event: &Event) -> EventView {
    let mut entry = event_view(state, event, true);
    if let EventBody::ItemEdited(edited) = &event.content.body {
        let changes: Vec<String> = edited
            .changes
            .iter()
            .map(|change| match change {
                Change::Title(title) => format!("title = {title:?}"),
                Change::Body(body) => format!("body = {body:?}"),
                Change::Priority(priority) => format!(
                    "priority = {}",
                    priority.map_or("none".to_string(), |p| p.to_string())
                ),
                Change::Lane(lane) => format!(
                    "lane = {}",
                    lane.as_ref()
                        .map_or("none".to_string(), ToString::to_string)
                ),
                Change::Labels(change) => format!("labels {}", set_change(change)),
                Change::Requires(change) => format!("requires {}", set_change(change)),
            })
            .collect();
        entry.summary = format!("edited: {}", changes.join("; "));
    } else if let EventBody::ItemTransitioned(transition) = &event.content.body {
        entry.summary = match &transition.note {
            Some(note) => format!("moved to {} ({note})", transition.to),
            None => format!("moved to {}", transition.to),
        };
    }
    entry
}

fn set_change<T: std::fmt::Display>(change: &wf_core::crdt::SetChange<T>) -> String {
    match change {
        wf_core::crdt::SetChange::Add(value) => format!("+{value}"),
        wf_core::crdt::SetChange::Remove { value, .. } => format!("-{value}"),
    }
}

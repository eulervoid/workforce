//! What commands print. Each view is a plain serializable struct: `--json` prints it inside a
//! versioned [`Envelope`], text output is rendered from the same struct in `render`.

use std::collections::{BTreeMap, BTreeSet};

use jiff::Timestamp;
use serde::Serialize;
use wf_core::body::{Body, BodyPart, StepId};
use wf_core::event::{Change, Event, EventBody, Link};
use wf_core::graph::{DependencyGraph, Readiness};
use wf_core::hlc::Hlc;
use wf_core::id::{EventId, HashId, ItemId};
use wf_core::identity::Actor;
use wf_core::item::{Item, ItemView, NoteKind, Origin, Standing};
use wf_core::policy::{Field, StateCategory};
use wf_core::resolve::MentionTarget;
use wf_core::session::{EndReason, Session, SessionRole};
use wf_core::state::State;

use crate::cli::ItemGrouping;

/// Bump when a view changes shape incompatibly. Snapshot tests pin the current shapes.
pub const SCHEMA_VERSION: u32 = 2;

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

#[derive(Clone, Serialize)]
pub struct ItemRefView {
    pub id: String,
    #[serde(rename = "type")]
    pub item_type: String,
    pub title: String,
    pub state: Option<String>,
    #[serde(skip)]
    pub state_category: Option<String>,
    pub priority: Option<String>,
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
    pub state_category: Option<String>,
    pub priority: Option<String>,
    /// `None` unless the item is open.
    pub readiness: Option<Readiness>,
    pub blocked_by: Vec<ItemRefView>,
    /// Live items waiting on this one, highest priority first.
    pub blocking: Vec<ItemRefView>,
    pub labels: Vec<String>,
    /// The item's lane, or its type's default lane.
    pub lane: Option<String>,
    pub archived: bool,
}

fn standing_parts(standing: Standing<'_>) -> (Option<String>, Option<String>) {
    match standing {
        Standing::Known {
            state,
            state_category,
        } => (Some(state.to_string()), Some(state_category.to_string())),
        Standing::Unknown { state } => (state.map(ToString::to_string), None),
    }
}

fn item_ref_view(state: &State, view: ItemView<'_>) -> ItemRefView {
    let policy = &state.policy().expect("a workforce has a policy").policy;
    let (state_name, state_category) = standing_parts(view.standing(policy));
    let priority = match view {
        ItemView::Live(item) => item.priority().map(|priority| priority.to_string()),
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
            _ => None,
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
    let (priority, readiness, blocked_by, blocking, labels, lane, archived) = match view {
        ItemView::Live(item) => (
            item.priority().map(|priority| priority.to_string()),
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
            |_, summary| Assigned::from(summary.priority.clone()),
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
    pub state_category: Option<String>,
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
    /// Reports only: the session that wrote it, such as `coder work 7c2k`.
    pub session: Option<String>,
    /// Reports only: the step it is about.
    pub step: Option<StepRefView>,
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
    pub lane: Option<String>,
    /// The review level the item needs: its own, or its type's default.
    pub review: Option<String>,
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
fn session_label(session: &Session) -> String {
    let role = match session.role() {
        SessionRole::Work => "work",
        SessionRole::Review { .. } => "review",
    };
    format!("{} {role} {}", session.agent().name, session.id().short())
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

pub fn note_text(event: &Event) -> Option<String> {
    match &event.content.body {
        EventBody::Comment(comment) => Some(comment.body.clone()),
        EventBody::ItemTransitioned(transition) => transition.note.clone(),
        EventBody::Report(report) => Some(report.note.clone()),
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
    if state.readiness(&graph, item) == Some(Readiness::Stranded) {
        flags.push(
            "stranded: a dependency was cancelled; `wf item link --remove-depends-on` frees it"
                .to_string(),
        );
    }

    let notes = item
        .notes()
        .iter()
        .map(|(id, note)| {
            let (session, step) = match note.kind {
                NoteKind::Report { session, step } => (
                    state.session(session).map(session_label),
                    step.map(|step| step_ref(&item.body(), step)),
                ),
                NoteKind::Comment | NoteKind::Transition => (None, None),
            };
            NoteView {
                id: id.to_string(),
                kind: note.kind.name().to_string(),
                by: actor_label(state, &note.by),
                at: timestamp(note.at),
                text: load_event(id).as_ref().and_then(note_text),
                session,
                step,
            }
        })
        .collect::<Vec<_>>();
    let mut notes = notes;
    notes.sort_by(|left, right| left.at.cmp(&right.at).then(left.id.cmp(&right.id)));

    ItemDetail {
        summary: summary(state, &graph, ItemView::Live(item)),
        body: body_parts(&item.body()),
        author: actor_label(state, item.author()),
        created: timestamp(item.created()),
        updated: timestamp(item.updated()),
        lane: item.effective_lane(policy).map(ToString::to_string),
        review: item.effective_review(policy).map(ToString::to_string),
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
    pub start: String,
    pub permissions: String,
    pub nudges: u32,
    pub park_timeout: String,
    pub max_duration: Option<String>,
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
                    Change::Review(_) => "review",
                    Change::StepDone { .. } => "step",
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
        EventBody::Report(report) => match state.session(report.session) {
            Some(session) => format!("{} reported", item(&session.item())),
            None => format!("session {} reported", report.session.short()),
        },
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
    pub flagged: Vec<String>,
    pub refolded: bool,
}

#[derive(Serialize)]
pub struct DeviceStatus {
    pub name: String,
    pub this_device: bool,
    /// Only a running daemon knows.
    pub last_seen: Option<String>,
}

pub fn status(
    state: &State,
    own: Option<wf_core::identity::DeviceKey>,
    last_seen: &std::collections::BTreeMap<wf_core::identity::DeviceKey, u64>,
    refolded: bool,
) -> StatusView {
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
            actor_label(state, &Actor::Human(announcement.by)),
            announcement.to,
            announcement.to
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
        archived_items,
        forked_from: state.fork_point().map(|(parent, _)| parent.to_string()),
        devices,
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
                Change::Review(review) => format!(
                    "review = {}",
                    review
                        .as_ref()
                        .map_or("none".to_string(), ToString::to_string)
                ),
                Change::StepDone { step, done } => {
                    format!("step {step} {}", if *done { "done" } else { "not done" })
                }
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

#[derive(Serialize)]
pub struct SessionView {
    pub id: String,
    pub agent: String,
    pub item: String,
    pub title: String,
    /// `work` or `review`.
    pub role: String,
    /// `live`, or why it ended.
    pub status: String,
    pub started: String,
    pub ended: Option<String>,
    /// What the session submitted, or the commit a review session reviews.
    pub commit: Option<String>,
}

/// Live sessions first, then the others, each the newest first.
pub fn session_views(state: &State) -> Vec<SessionView> {
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
                (SessionRole::Review { commit }, _) => Some(commit.to_string()),
                (SessionRole::Work, Some(EndReason::Submitted { commit })) => {
                    commit.map(|commit| commit.to_string())
                }
                (SessionRole::Work, _) => None,
            };
            SessionView {
                id: session.id().to_base32(),
                agent: mention_label(state, &MentionTarget::Agent(session.agent().clone())),
                item: session.item().to_base32(),
                title,
                role: match session.role() {
                    SessionRole::Work => "work",
                    SessionRole::Review { .. } => "review",
                }
                .to_string(),
                status: session
                    .ended()
                    .map_or("live", |ended| ended.reason.name())
                    .to_string(),
                started: timestamp(session.started().hlc),
                ended: session.ended().map(|ended| timestamp(ended.at())),
                commit,
            }
        })
        .collect()
}

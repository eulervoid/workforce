//! `wf item`: creating, showing, editing, moving and linking items.

use std::cmp::Ordering;
use std::collections::BTreeSet;

use anyhow::{Context, bail};
use serde::Serialize;
use wf_core::crdt::SetChange;
use wf_core::event::{Change, EventBody, ItemCreated, ItemLinked, ItemMoved, ItemRef, Link};
use wf_core::id::{HashId, ItemId};
use wf_core::item::{Item, ItemState, ItemView, Merge, Priority};
use wf_core::policy::Field;
use wf_core::state::State;
use wf_core::validate::OneOf;

use super::comment::comment_why;
use super::vcs::{checkout, diff, path};
use super::{Done, done, edit_until_valid, emit, emit_paged};
use crate::cli::*;
use crate::draft::{Draft, NewItemDraft};
use crate::interactive::{self, Choice, Mode};
use crate::render;
use crate::view::{self, ItemSummary};
use crate::workspace::Workspace;

/// The items, the live ones in `order` and the archived ones after them by when they were
/// archived.
pub(crate) fn sorted_views(
    state: &State,
    include_archived: bool,
    order: ItemOrder,
) -> Vec<ItemView<'_>> {
    let mut views: Vec<ItemView<'_>> = state
        .items()
        .filter(|view| include_archived || matches!(view, ItemView::Live(_)))
        .collect();
    views.sort_by(|first, second| match (first, second) {
        (ItemView::Live(first), ItemView::Live(second)) => compare_items(order, first, second),
        (ItemView::Live(_), ItemView::Archived(_)) => Ordering::Less,
        (ItemView::Archived(_), ItemView::Live(_)) => Ordering::Greater,
        (ItemView::Archived(first), ItemView::Archived(second)) => {
            (first.archived_at(), first.id()).cmp(&(second.archived_at(), second.id()))
        }
    });
    views
}

fn compare_items(order: ItemOrder, first: &Item, second: &Item) -> Ordering {
    let by_priority = |item: &Item| {
        (
            item.priority().is_none(),
            item.priority(),
            item.created(),
            item.id(),
        )
    };
    match order {
        ItemOrder::Priority => by_priority(first).cmp(&by_priority(second)),
        ItemOrder::Created => (first.created(), first.id()).cmp(&(second.created(), second.id())),
        ItemOrder::Merged => {
            let merged_at = |item: &Item| item.latest_merge().map(Merge::merged_at);
            match (merged_at(first), merged_at(second)) {
                (Some(first_at), Some(second_at)) => {
                    (first_at, first.id()).cmp(&(second_at, second.id()))
                }
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => by_priority(first).cmp(&by_priority(second)),
            }
        }
    }
}

/// Which items a picker offers.
#[derive(Clone, Copy)]
pub(crate) enum Scope {
    Live,
    /// Live items that no live work session holds, to start one on.
    Unclaimed,
    Archived,
    Any,
}

impl Scope {
    fn includes(self, state: &State, view: &ItemView<'_>) -> bool {
        match (self, view) {
            (Scope::Unclaimed, ItemView::Live(item)) => state.claim(item.id()).is_none(),
            (Scope::Any, _)
            | (Scope::Live, ItemView::Live(_))
            | (Scope::Archived, ItemView::Archived(_)) => true,
            (Scope::Live | Scope::Unclaimed, ItemView::Archived(_))
            | (Scope::Archived, ItemView::Live(_)) => false,
        }
    }
}

fn item_choice(summary: &ItemSummary) -> Choice {
    Choice {
        value: summary.id.clone(),
        label: render::item_choice(summary),
    }
}

fn pick_item(
    workspace: &Workspace,
    mode: Mode,
    scope: Scope,
    what: &str,
) -> anyhow::Result<ItemId> {
    if mode == Mode::Scripted {
        bail!("missing {what}; pass an item id, or run at a terminal to pick one");
    }
    let state = workspace.state();
    let graph = state.dependency_graph();
    let choices = sorted_views(state, true, ItemOrder::Priority)
        .into_iter()
        .filter(|view| scope.includes(state, view))
        .map(|view| item_choice(&view::summary(state, &graph, view)))
        .collect();
    let picked = interactive::pick(what, choices)?;
    workspace.resolve_item(&picked)
}

pub(crate) fn choose_item(
    workspace: &Workspace,
    mode: Mode,
    given: Option<String>,
    scope: Scope,
) -> anyhow::Result<ItemId> {
    match given {
        Some(item) => workspace.resolve_item(&item),
        None => pick_item(workspace, mode, scope, "item"),
    }
}

pub(super) fn item(
    json: bool,
    mode: Mode,
    command: ItemCommand,
    workspace: &mut Workspace,
) -> anyhow::Result<()> {
    match command {
        ItemCommand::Add(args) => {
            let wants_editor = args.edit || (args.title.is_none() && mode == Mode::Interactive);
            if args.title.is_none() && !wants_editor {
                bail!("missing --title; run at a terminal to write the item in your editor");
            }
            let mut new_item = NewItemDraft {
                item_type: args.item_type,
                state: args.draft.then_some(ItemState::Draft),
                draft: Draft {
                    title: args.title.unwrap_or_default(),
                    body: args.body.unwrap_or_default(),
                    priority: args.priority,
                    lane: args.lane,
                    labels: args.labels.into_iter().collect(),
                    requires: args.requires.into_iter().collect(),
                    review: args.review,
                    recovery: args.recovery,
                },
            };
            let has_priorities = workspace
                .state()
                .policy()
                .and_then(|document| document.policy.type_config(&new_item.item_type))
                .is_some_and(|config| config.allows_field(Field::Priority));
            if has_priorities && new_item.draft.priority.is_none() {
                new_item.draft.priority = Some(Priority::DEFAULT);
            }
            if wants_editor {
                new_item =
                    edit_until_valid(&new_item.to_markdown()?, ".md", NewItemDraft::from_markdown)?;
            }
            let draft = new_item.draft;
            let initial = workspace
                .state()
                .policy()
                .and_then(|document| document.policy.type_config(&new_item.item_type))
                .map(|config| config.initial);
            let state = new_item
                .state
                .or(initial)
                .with_context(|| format!("there is no item type `{}`", new_item.item_type))?;
            if let Some(recovery) = &draft.recovery {
                let policy = &workspace
                    .state()
                    .policy()
                    .expect("a workforce has a policy")
                    .policy;
                if policy.recovery_level(recovery).is_none() {
                    let valid = OneOf::new(policy.recovery.iter().map(|level| &level.name));
                    bail!(
                        "there is no recovery level `{recovery}`, so nothing was created; {valid}"
                    );
                }
            }
            let event = workspace.author(EventBody::ItemCreated(ItemCreated {
                item_type: new_item.item_type,
                title: draft.title,
                body: draft.body,
                priority: draft.priority,
                lane: draft.lane,
                labels: draft.labels.into_iter().collect(),
                requires: draft.requires.into_iter().collect(),
                review: draft.review,
                state: Some(state.into()),
            }))?;
            let id = ItemId::from(event.id());
            // The recovery level is no field of a new item, so that events keep their layout.
            if let Some(recovery) = draft.recovery {
                workspace.author(EventBody::ItemEdited(wf_core::event::ItemEdited {
                    item: id,
                    changes: vec![Change::RecoveryLevel(Some(recovery))],
                }))?;
            }
            let state = workspace.state();
            let graph = state.dependency_graph();
            let created = view::summary(state, &graph, state.item(id).expect("just created"));
            emit(json, &created, |created| {
                format!(
                    "created #{}\n{}",
                    wf_core::id::short(&created.id),
                    render::item_list(std::slice::from_ref(created))
                )
            })
        }
        ItemCommand::List(args) => {
            let state = workspace.state();
            check_list_filters(state, &args)?;
            let graph = state.dependency_graph();
            let views = sorted_views(state, args.archived, args.sort);
            let dispatchable: Option<BTreeSet<String>> = match &args.for_agent {
                None => None,
                Some(name) => {
                    let device = workspace.device_key();
                    let principal = state
                        .principal_of(&device)
                        .context("this device is not bound to a member")?;
                    if state.agent(name, principal).is_none() {
                        bail!("{name} is not an agent of yours; see `wf agent list`");
                    }
                    let agent = wf_core::identity::AgentId {
                        principal,
                        name: name.clone(),
                    };
                    Some(
                        state
                            .dispatchable(device, &agent)
                            .iter()
                            .map(|item| item.id().to_base32())
                            .collect(),
                    )
                }
            };
            let entries: Vec<ItemSummary> = views
                .into_iter()
                .map(|view| view::summary(state, &graph, view))
                .filter(|summary| {
                    args.item_type
                        .as_ref()
                        .is_none_or(|wanted| summary.item_type == wanted.as_str())
                })
                .filter(|summary| args.state.is_empty() || args.state.contains(&summary.state))
                .filter(|summary| args.accepts(summary.readiness))
                .filter(|summary| {
                    args.label
                        .as_ref()
                        .is_none_or(|wanted| summary.labels.contains(&wanted.to_string()))
                })
                .filter(|summary| {
                    args.lane
                        .as_ref()
                        .is_none_or(|wanted| summary.lane.as_deref() == Some(wanted.as_str()))
                })
                .filter(|summary| {
                    args.priority
                        .is_none_or(|wanted| summary.priority == Some(wanted))
                })
                .filter(|summary| {
                    dispatchable
                        .as_ref()
                        .is_none_or(|offered| offered.contains(&summary.id))
                })
                .collect();
            match args.group_by {
                None => {
                    emit(json, &entries, |items| render::item_list(items))
                }
                Some(grouping) => {
                    let groups = view::group_items(grouping, entries);
                    emit(json, &groups, |groups| {
                        render::item_groups(grouping, groups)
                    })
                }
            }
        }
        ItemCommand::Show { item } => {
            let id = choose_item(workspace, mode, item, Scope::Any)?;
            show(json, mode, workspace, id)
        }
        ItemCommand::Edit(args) => edit(json, mode, workspace, args),
        ItemCommand::Check(args) => check(json, workspace, args),
        ItemCommand::Open(args) => move_items(json, mode, workspace, args, ItemState::Open),
        ItemCommand::Draft(args) => move_items(json, mode, workspace, args, ItemState::Draft),
        ItemCommand::Done(args) => move_items(json, mode, workspace, args, ItemState::Done),
        ItemCommand::Cancel { item, note } => {
            let args = ItemMoveArgs {
                items: item.into_iter().collect(),
                note,
            };
            move_items(json, mode, workspace, args, ItemState::Cancelled)
        }
        ItemCommand::Link(args) => link(json, mode, workspace, args),
        ItemCommand::Diff { item, git_args } => {
            // `--json` after the item is wf's, not git's, though clap passes it on.
            let (json_flags, git_args): (Vec<String>, Vec<String>) =
                git_args.into_iter().partition(|arg| arg == "--json");
            let json = json || !json_flags.is_empty();
            let mode = if json { Mode::Scripted } else { mode };
            let id = choose_item(workspace, mode, item, Scope::Live)?;
            diff(json, mode, workspace, id, &git_args)
        }
        ItemCommand::Checkout { item } => checkout(json, mode, workspace, item),
        ItemCommand::Path { item } => path(json, mode, workspace, item),
        ItemCommand::Archive {
            done: archive_done,
            cancelled,
            ..
        } if archive_done || cancelled => {
            let states: Vec<ItemState> = [
                (archive_done, ItemState::Done),
                (cancelled, ItemState::Cancelled),
            ]
            .into_iter()
            .filter_map(|(wanted, state)| wanted.then_some(state))
            .collect();
            archive_finished(json, workspace, &states)
        }
        ItemCommand::Archive { item, .. } => {
            let id = choose_item(workspace, mode, item, Scope::Live)?;
            let event = workspace.author(EventBody::ItemArchived(ItemRef { item: id }))?;
            done(json, format!("archived #{}", id.short()), &event, Some(id))
        }
        ItemCommand::Unarchive { item } => {
            let id = choose_item(workspace, mode, item, Scope::Archived)?;
            let event = workspace.author(EventBody::ItemUnarchived(ItemRef { item: id }))?;
            done(
                json,
                format!("unarchived #{}", id.short()),
                &event,
                Some(id),
            )
        }
    }
}

#[derive(Serialize)]
struct Moved {
    message: String,
    /// One `ItemMoved` event per item moved, in the order of `items`.
    events: Vec<String>,
    items: Vec<String>,
    /// Items that were in the state already, so nothing was written for them.
    unchanged: Vec<String>,
}

/// What the verb for moving an item to `state` says it did.
fn moved_verb(state: ItemState) -> &'static str {
    match state {
        ItemState::Draft => "held back",
        ItemState::Open => "opened",
        ItemState::Active => "made active",
        ItemState::Review => "put in review",
        ItemState::Done => "done",
        ItemState::Cancelled => "cancelled",
    }
}

/// Moves each of `args.items`, or one picked at a terminal, to `to`. Every item is checked before
/// anything is written, so a typo moves none of them.
fn move_items(
    json: bool,
    mode: Mode,
    workspace: &mut Workspace,
    args: ItemMoveArgs,
    to: ItemState,
) -> anyhow::Result<()> {
    let ids: Vec<ItemId> = if args.items.is_empty() {
        vec![choose_item(workspace, mode, None, Scope::Live)?]
    } else {
        let mut ids = Vec::new();
        for item in &args.items {
            let id = workspace.resolve_item(item)?;
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        ids
    };
    let mut moving = Vec::new();
    let mut unchanged = Vec::new();
    {
        let state = workspace.state();
        let policy = &state.policy().expect("a workforce has a policy").policy;
        for id in &ids {
            let ItemView::Live(live) = state.item(*id).expect("resolved ids exist") else {
                bail!("#{} is archived; unarchive it first", id.short());
            };
            if live.state(policy) == to && live.unknown_state().is_none() {
                unchanged.push(*id);
                continue;
            }
            let probe = EventBody::ItemMoved(ItemMoved {
                item: *id,
                to: to.into(),
                by_session: None,
                because: None,
            });
            state
                .check_policy(&probe)
                .with_context(|| format!("#{} cannot be moved, so nothing was", id.short()))?;
            moving.push(*id);
        }
    }
    let mut events = Vec::with_capacity(moving.len());
    for id in &moving {
        let because = comment_why(workspace, *id, args.note.clone())?;
        let event = workspace.author(EventBody::ItemMoved(ItemMoved {
            item: *id,
            to: to.into(),
            by_session: None,
            because,
        }))?;
        events.push(event.id().to_string());
    }
    let list = |ids: &[ItemId]| {
        ids.iter()
            .map(|id| format!("#{}", id.short()))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut message = match moving.as_slice() {
        [] => String::new(),
        moved => format!("{} {}", moved_verb(to), list(moved)),
    };
    if !unchanged.is_empty() {
        if !message.is_empty() {
            message.push_str("; ");
        }
        message.push_str(&format!("{} already {to}", list(&unchanged)));
    }
    emit(
        json,
        &Moved {
            message,
            events,
            items: moving.iter().map(|id| id.to_base32()).collect(),
            unchanged: unchanged.iter().map(|id| id.to_base32()).collect(),
        },
        |moved| format!("{}\n", moved.message),
    )
}

#[derive(Serialize)]
struct ArchivedFinished {
    message: String,
    /// One `ItemArchived` event per item, in the order of `items`.
    events: Vec<String>,
    items: Vec<String>,
}

/// Archives every live item whose state is one of `states`.
fn archive_finished(
    json: bool,
    workspace: &mut Workspace,
    states: &[ItemState],
) -> anyhow::Result<()> {
    let ids: Vec<ItemId> = {
        let state = workspace.state();
        let policy = &state.policy().expect("a workforce has a policy").policy;
        sorted_views(state, false, ItemOrder::Priority)
            .into_iter()
            .filter(|view| states.contains(&view.state(policy)))
            .map(|view| view.id())
            .collect()
    };
    let mut events = Vec::with_capacity(ids.len());
    for id in &ids {
        let event = workspace.author(EventBody::ItemArchived(ItemRef { item: *id }))?;
        events.push(event.id().to_string());
    }
    let which = states
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" or ");
    let message = match ids.len() {
        0 => format!("nothing to archive: no live item is {which}"),
        count => format!(
            "archived {count} {}: {}",
            if count == 1 { "item" } else { "items" },
            ids.iter()
                .map(|id| format!("#{}", id.short()))
                .collect::<Vec<_>>()
                .join(" ")
        ),
    };
    emit(
        json,
        &ArchivedFinished {
            message,
            events,
            items: ids.iter().map(|id| id.to_base32()).collect(),
        },
        |archived| format!("{}\n", archived.message),
    )
}

#[derive(Serialize)]
struct ArchivedShown {
    #[serde(flatten)]
    summary: ItemSummary,
    archived_at: String,
    note: &'static str,
}

#[derive(Serialize)]
#[serde(untagged)]
enum Shown {
    Live(Box<view::ItemDetail>),
    Archived(Box<ArchivedShown>),
}

fn show(json: bool, mode: Mode, workspace: &Workspace, id: ItemId) -> anyhow::Result<()> {
    let state = workspace.state();
    match state.item(id).expect("resolved ids exist") {
        ItemView::Archived(stub) => {
            let graph = state.dependency_graph();
            let shown = Shown::Archived(Box::new(ArchivedShown {
                summary: view::summary(state, &graph, ItemView::Archived(stub)),
                archived_at: view::timestamp(stub.archived_at()),
                note: "archived: body and discussion load from the log on demand; `wf item unarchive` restores the item",
            }));
            emit_paged(json, mode, &shown, |shown| match shown {
                Shown::Archived(archived) => format!(
                    "{}archived {}\n{}\n",
                    render::item_list(std::slice::from_ref(&archived.summary)),
                    archived.archived_at,
                    archived.note
                ),
                Shown::Live(_) => unreachable!(),
            })
        }
        ItemView::Live(item) => {
            let events = workspace.events()?;
            let shown = Shown::Live(Box::new(view::item_detail(state, item, &events)));
            emit_paged(json, mode, &shown, |shown| match shown {
                Shown::Live(detail) => render::item_detail(detail),
                Shown::Archived(_) => unreachable!(),
            })
        }
    }
}

fn edit(
    json: bool,
    mode: Mode,
    workspace: &mut Workspace,
    args: ItemEditArgs,
) -> anyhow::Result<()> {
    let id = choose_item(workspace, mode, args.item.clone(), Scope::Live)?;
    let changes = {
        let state = workspace.state();
        let ItemView::Live(item) = state.item(id).expect("resolved ids exist") else {
            bail!("#{} is archived; unarchive it to edit", id.short());
        };
        let current = Draft::of(item);
        let wants_editor = args.edit || (!args.has_changes() && mode == Mode::Interactive);
        if !args.has_changes() && !wants_editor {
            bail!("nothing to change; pass flags, or run at a terminal to edit in your editor");
        }
        let mut draft = current;
        if let Some(title) = args.title {
            draft.title = title;
        }
        if let Some(body) = args.body {
            draft.body = body;
        }
        if let Some(priority) = args.priority {
            draft.priority = Some(priority);
        }
        if let Some(lane) = args.lane {
            draft.lane = Some(lane);
        }
        if let Some(review) = args.review {
            draft.review = review.into_level();
        }
        if let Some(recovery) = args.recovery {
            draft.recovery = recovery.into_level();
        }
        draft.labels.extend(args.add_labels);
        for label in &args.remove_labels {
            draft.labels.remove(label);
        }
        draft.requires.extend(args.add_requires);
        for requirement in &args.remove_requires {
            draft.requires.remove(requirement);
        }
        if wants_editor {
            draft = edit_until_valid(&draft.to_markdown()?, ".md", Draft::from_markdown)?;
        }
        draft.changes(item)
    };
    if changes.is_empty() {
        return emit(
            json,
            &Done {
                message: "nothing changed".into(),
                event: None,
                session: None,
                item: Some(id.to_base32()),
            },
            |done| format!("{}\n", done.message),
        );
    }
    let event = workspace.author(EventBody::ItemEdited(wf_core::event::ItemEdited {
        item: id,
        changes,
    }))?;
    done(json, format!("edited #{}", id.short()), &event, Some(id))
}

fn check(json: bool, workspace: &mut Workspace, args: ItemCheckArgs) -> anyhow::Result<()> {
    let id = workspace.resolve_item(&args.item)?;
    let ticked = !args.remove;
    let (changes, message) = {
        let ItemView::Live(item) = workspace.state().item(id).expect("resolved ids exist") else {
            bail!("#{} is archived; unarchive it to edit", id.short());
        };
        let body = item.body();
        let steps: Vec<_> = body.steps().collect();
        let mut chosen = Vec::new();
        for number in args
            .numbers
            .iter()
            .map(|number| number.get())
            .collect::<BTreeSet<_>>()
        {
            match steps.get(number - 1) {
                Some(step) => chosen.push(*step),
                None if steps.is_empty() => bail!("#{} has no steps", id.short()),
                None => bail!(
                    "#{} has {} steps, there is no step {number}",
                    id.short(),
                    steps.len()
                ),
            }
        }
        let verb = if ticked { "checked" } else { "unchecked" };
        let (changing, unchanged): (Vec<_>, Vec<_>) =
            chosen.into_iter().partition(|step| step.done != ticked);
        let label = |step: &wf_core::body::Step<'_>| {
            let first_line = step.text.lines().next().unwrap_or_default();
            format!("[{}] {first_line}", step.number)
        };
        if changing.is_empty() {
            let labels: Vec<String> = unchanged.iter().map(label).collect();
            return emit(
                json,
                &Done {
                    message: format!("nothing changed: {} already {verb}", labels.join(", ")),
                    event: None,
                    session: None,
                    item: Some(id.to_base32()),
                },
                |done| format!("{}\n", done.message),
            );
        }
        let done_count = steps.iter().filter(|step| step.done).count();
        let done_count = if ticked {
            done_count + changing.len()
        } else {
            done_count - changing.len()
        };
        let labels: Vec<String> = changing.iter().map(label).collect();
        let changes: Vec<Change> = changing
            .iter()
            .map(|step| Change::StepDone {
                step: step.id,
                done: ticked,
            })
            .collect();
        (
            changes,
            format!(
                "{verb} {} ({done_count}/{} done)",
                labels.join(", "),
                steps.len()
            ),
        )
    };
    let event = workspace.author(EventBody::ItemEdited(wf_core::event::ItemEdited {
        item: id,
        changes,
    }))?;
    done(json, message, &event, Some(id))
}

/// Refuses a `--type` or `--lane` that no item can have, instead of listing nothing.
fn check_list_filters(state: &State, args: &ItemListArgs) -> anyhow::Result<()> {
    let policy = &state.policy().expect("a workforce has a policy").policy;
    if let Some(name) = &args.item_type
        && policy.type_config(name).is_none()
    {
        bail!(
            "there is no item type `{name}`; {}",
            OneOf::new(policy.types.keys())
        );
    }
    if let Some(lane) = &args.lane
        && !policy.lanes.contains_key(lane)
    {
        bail!(
            "there is no lane `{lane}`; {}",
            OneOf::new(policy.lanes.keys())
        );
    }
    Ok(())
}

fn link(
    json: bool,
    mode: Mode,
    workspace: &mut Workspace,
    args: ItemLinkArgs,
) -> anyhow::Result<()> {
    let id = choose_item(workspace, mode, args.item, Scope::Live)?;
    let (link, message) = if let Some(target) = args.depends_on {
        depends_on(id, workspace.resolve_item(&target)?)
    } else if let Some(target) = args.remove_depends_on {
        remove_depends_on(workspace, id, workspace.resolve_item(&target)?)?
    } else if let Some(target) = args.supersedes {
        supersedes(id, workspace.resolve_item(&target)?)
    } else if mode == Mode::Scripted {
        bail!("missing link; pass --depends-on, --remove-depends-on or --supersedes");
    } else {
        let action = interactive::pick(
            "link",
            ["depends on", "no longer depends on", "supersedes"]
                .into_iter()
                .map(|label| Choice {
                    value: label.to_string(),
                    label: label.to_string(),
                })
                .collect(),
        )?;
        match action.as_str() {
            "depends on" => {
                let target = pick_item(workspace, mode, Scope::Live, "item to depend on")?;
                depends_on(id, target)
            }
            "no longer depends on" => {
                let target = pick_dependency(workspace, id)?;
                remove_depends_on(workspace, id, target)?
            }
            _ => supersedes(
                id,
                pick_item(workspace, mode, Scope::Live, "item to supersede")?,
            ),
        }
    };
    let event = workspace.author(EventBody::ItemLinked(ItemLinked { item: id, link }))?;
    done(json, message, &event, Some(id))
}

fn depends_on(id: ItemId, target: ItemId) -> (Link, String) {
    (
        Link::DependsOn(SetChange::Add(target)),
        format!("#{} now depends on #{}", id.short(), target.short()),
    )
}

fn remove_depends_on(
    workspace: &Workspace,
    id: ItemId,
    target: ItemId,
) -> anyhow::Result<(Link, String)> {
    let removal = match workspace.state().item(id).expect("resolved ids exist") {
        ItemView::Live(item) => item.depends_on().removal(&target),
        ItemView::Archived(_) => bail!("#{} is archived; unarchive it first", id.short()),
    };
    Ok((
        Link::DependsOn(removal),
        format!("#{} no longer depends on #{}", id.short(), target.short()),
    ))
}

fn supersedes(id: ItemId, target: ItemId) -> (Link, String) {
    (
        Link::Supersedes(target),
        format!("#{} supersedes #{}", id.short(), target.short()),
    )
}

fn pick_dependency(workspace: &Workspace, id: ItemId) -> anyhow::Result<ItemId> {
    let state = workspace.state();
    let graph = state.dependency_graph();
    let ItemView::Live(item) = state.item(id).expect("resolved ids exist") else {
        bail!("#{} is archived; unarchive it first", id.short());
    };
    let choices = item
        .depends_on()
        .values()
        .filter_map(|dependency| state.item(*dependency))
        .map(|view| item_choice(&view::summary(state, &graph, view)))
        .collect();
    workspace.resolve_item(&interactive::pick("dependency", choices)?)
}

#[cfg(test)]
mod tests {
    use wf_core::session::CommitId;
    use wf_core::testing::{Fixture, create, define_agent, edit, merged, start_session};

    use super::*;

    #[test]
    fn the_start_picker_leaves_out_an_item_with_a_live_session() {
        let fixture = Fixture::new(1);
        let mut state = fixture.state();
        fixture.apply(&mut state, 0, 2_000, define_agent("coder"));
        let busy = ItemId::from(
            fixture
                .apply(&mut state, 0, 2_001, create("task", "Busy"))
                .id(),
        );
        let free = ItemId::from(
            fixture
                .apply(&mut state, 0, 2_002, create("task", "Free"))
                .id(),
        );
        fixture.apply(&mut state, 0, 3_000, start_session("coder", busy));
        let offered = |scope: Scope| -> Vec<ItemId> {
            state
                .items()
                .filter(|view| scope.includes(&state, view))
                .map(|view| view.id())
                .collect()
        };
        assert_eq!(offered(Scope::Unclaimed), vec![free]);
        assert_eq!(offered(Scope::Live).len(), 2);
    }

    fn commit(byte: u8) -> CommitId {
        let text: String = std::iter::repeat_n(format!("{byte:02x}"), 20).collect();
        CommitId::Git(text.parse().unwrap())
    }

    #[test]
    fn items_are_sorted_by_priority_creation_or_merge() {
        let fixture = Fixture::new(1);
        let mut state = fixture.state();
        let mut clock = 2_000;
        let mut apply = |state: &mut State, body: EventBody| {
            clock += 1;
            fixture.apply(state, 0, clock, body)
        };
        let mut add = |state: &mut State, title: &str, priority: Option<Priority>| {
            let id = ItemId::from(apply(state, create("task", title)).id());
            apply(state, edit(id, Change::Priority(priority)));
            id
        };
        let low = add(&mut state, "Low", Some(Priority::P2));
        let urgent = add(&mut state, "Urgent", Some(Priority::P0));
        let unprioritized = add(&mut state, "Unprioritized", None);
        let high = add(&mut state, "High", Some(Priority::P1));
        let sorted = |state: &State, order: ItemOrder| -> Vec<ItemId> {
            sorted_views(state, false, order)
                .into_iter()
                .map(|view| view.id())
                .collect()
        };
        assert_eq!(
            sorted(&state, ItemOrder::Priority),
            [urgent, high, low, unprioritized]
        );
        assert_eq!(
            sorted(&state, ItemOrder::Created),
            [low, urgent, unprioritized, high]
        );
        apply(&mut state, merged(high, commit(1), commit(2)));
        apply(&mut state, merged(low, commit(3), commit(4)));
        assert_eq!(
            sorted(&state, ItemOrder::Merged),
            [high, low, urgent, unprioritized],
            "merged items in the order they went in, the rest by priority"
        );
        apply(&mut state, merged(high, commit(5), commit(6)));
        assert_eq!(
            sorted(&state, ItemOrder::Merged),
            [low, high, urgent, unprioritized],
            "by the latest merge"
        );
    }
}

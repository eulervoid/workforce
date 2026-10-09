use std::path::Path;

use anyhow::{Context, anyhow, bail};
use serde::Serialize;
use wf_core::crdt::SetChange;
use wf_core::event::{
    AgentDefined, Comment, Event, EventBody, Genesis, ItemCreated, ItemLinked, ItemRef,
    ItemTransitioned, Link, PolicySet,
};
use wf_core::id::{HashId, ItemId};
use wf_core::identity::{Actor, MemberSeed, Signed};
use wf_core::item::ItemView;
use wf_core::name::{AgentName, Handle, HarnessName, LaneName, StateName};
use wf_core::policy::{DEFAULT_POLICY_TOML, Policy};
use wf_core::refs::{self, Mention, Token};
use wf_core::state::{State, Trust};
use wf_store::{EventStore, RedbStore, Replica};

use crate::cli::*;
use crate::draft::{Draft, NewItemDraft};
use crate::harness;
use crate::interactive::{self, Choice, Mode};
use crate::keys::DataDir;
use crate::render;
use crate::view::{self, Envelope, ItemSummary, LogView, MentionsView};
use crate::workspace::{self, WORKFORCE_FILE, WorkforceFile, Workspace};

fn emit<T: Serialize>(json: bool, data: &T, text: impl FnOnce(&T) -> String) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&Envelope::new(data))?);
    } else {
        anstream::print!("{}", text(data));
    }
    Ok(())
}

/// Like [`emit`], but long text goes through the pager when a person is at the terminal.
fn emit_paged<T: Serialize>(
    json: bool,
    mode: Mode,
    data: &T,
    text: impl FnOnce(&T) -> String,
) -> anyhow::Result<()> {
    match mode {
        Mode::Interactive => interactive::page(&text(data)),
        Mode::Scripted => emit(json, data, text),
    }
}

#[derive(Serialize)]
pub struct Done {
    pub message: String,
    pub event: String,
    pub item: Option<String>,
}

fn done(json: bool, message: String, event: &Event, item: Option<ItemId>) -> anyhow::Result<()> {
    emit(
        json,
        &Done {
            message,
            event: event.id().to_string(),
            item: item.map(|id| id.to_base32()),
        },
        |done| format!("{}\n", done.message),
    )
}

pub fn run(cli: Cli, data: DataDir, cwd: &Path) -> anyhow::Result<()> {
    let json = cli.json;
    let mode = Mode::detect(json);
    match cli.command {
        Command::Init(args) => init(json, args, data, cwd),
        Command::Status { refold } => {
            let workspace = Workspace::open(data, cwd, refold)?;
            emit(
                json,
                &view::status(workspace.state(), refold),
                render::status,
            )
        }
        command => {
            let mut workspace = Workspace::open(data, cwd, false)?;
            match command {
                Command::Init(_) | Command::Status { .. } => unreachable!("handled above"),
                Command::Agent(command) => agent(json, command, &mut workspace),
                Command::Item(command) => item(json, mode, command, &mut workspace),
                Command::Comment(args) => comment(json, mode, &mut workspace, args),
                Command::Mentions { target } => mentions(json, &workspace, &target),
                Command::Policy(command) => policy(json, command, &mut workspace),
                Command::Log => log(json, mode, &workspace),
            }
        }
    }
}

fn handle_from_environment() -> Handle {
    std::env::var("USER")
        .ok()
        .map(|user| user.to_lowercase().replace([' ', '.'], "-"))
        .and_then(|user| Handle::new(user).ok())
        .unwrap_or_else(|| Handle::new("me").expect("a valid handle"))
}

#[derive(Serialize)]
struct InitView {
    name: String,
    id: String,
    handle: String,
    agents: Vec<String>,
}

fn init(json: bool, args: InitArgs, data: DataDir, cwd: &Path) -> anyhow::Result<()> {
    if workspace::find_workforce(cwd).is_ok() {
        bail!("this directory already belongs to a workforce ({WORKFORCE_FILE})");
    }
    let handle = match args.handle {
        Some(handle) => Handle::new(handle)?,
        None => handle_from_environment(),
    };
    let principal = data.principal(true)?.expect("created on demand");
    let device = data.device(true)?.expect("created on demand");

    let genesis = Genesis {
        name: args.name.clone(),
        founder: MemberSeed {
            principal: principal.key(),
            handle: handle.clone(),
            devices: vec![device.key()],
        },
        members: Vec::new(),
        policy: DEFAULT_POLICY_TOML.to_string(),
        forked_from: None,
    };
    let body = EventBody::Genesis(Signed::sign(&principal, None, genesis)?);
    let actor = Actor::Human(principal.key());
    let genesis_event =
        wf_core::event::EventBuilder::genesis(device.key(), actor.clone(), workspace::now_ms())
            .sign(&device, body)?;
    let id: wf_core::id::WorkforceId = genesis_event.id().into();

    let mut replica = Replica::open(RedbStore::open(workspace::store_path(&data, &id)?)?)?;
    if replica.state().workforce_id().is_some() {
        bail!("a local log for workforce {id} already exists");
    }
    replica.receive(genesis_event)?;

    let mut agents = Vec::new();
    if !args.no_agents {
        let path = std::env::var_os("PATH").unwrap_or_default();
        for (index, harness) in harness::detect(&path).into_iter().enumerate() {
            let name = if index == 0 {
                "coder".to_string()
            } else {
                format!("coder-{harness}")
            };
            replica.author(
                &device,
                actor.clone(),
                workspace::now_ms(),
                EventBody::AgentDefined(AgentDefined {
                    name: AgentName::new(&name)?,
                    harness: HarnessName::new(harness)?,
                    model: None,
                    capabilities: Vec::new(),
                    lanes: vec![LaneName::new("default")?],
                }),
            )?;
            agents.push(format!("@{handle}/{name} ({harness})"));
        }
    }

    let file = toml::to_string(&WorkforceFile { id })?;
    std::fs::write(cwd.join(WORKFORCE_FILE), file)?;
    emit(
        json,
        &InitView {
            name: args.name,
            id: id.to_string(),
            handle: handle.to_string(),
            agents,
        },
        |init| {
            let mut out = format!(
                "created workforce \"{}\" ({})\nyou are @{}\n",
                init.name, init.id, init.handle
            );
            if init.agents.is_empty() {
                out.push_str("no agents defined; add one with `wf agent add`\n");
            }
            for agent in &init.agents {
                out.push_str(&format!("defined agent {agent}\n"));
            }
            out
        },
    )
}

fn agent(json: bool, command: AgentCommand, workspace: &mut Workspace) -> anyhow::Result<()> {
    let principal = workspace
        .state()
        .principal_of(&workspace_device(workspace)?)
        .context("device is not bound")?;
    match command {
        AgentCommand::List => {
            let agents = view::agent_views(workspace.state());
            emit(json, &agents, |agents| render::agents(agents))
        }
        AgentCommand::Add(args) => {
            if workspace.state().agent(&args.name, principal).is_some() {
                bail!("agent `{}` already exists; use `wf agent edit`", args.name);
            }
            let event = workspace.author(EventBody::AgentDefined(AgentDefined {
                name: args.name.clone(),
                harness: args.harness,
                model: args.model,
                capabilities: args.capabilities,
                lanes: args.lanes,
            }))?;
            done(json, format!("defined agent {}", args.name), &event, None)
        }
        AgentCommand::Edit(args) => {
            let current = workspace
                .state()
                .agent(&args.name, principal)
                .ok_or_else(|| anyhow!("there is no agent `{}`", args.name))?
                .clone();
            let event = workspace.author(EventBody::AgentDefined(AgentDefined {
                name: args.name.clone(),
                harness: args.harness.unwrap_or(current.harness),
                model: args.model.or(current.model),
                capabilities: if args.capabilities.is_empty() {
                    current.capabilities
                } else {
                    args.capabilities
                },
                lanes: if args.lanes.is_empty() {
                    current.lanes
                } else {
                    args.lanes
                },
            }))?;
            done(json, format!("updated agent {}", args.name), &event, None)
        }
    }
}

fn workspace_device(workspace: &Workspace) -> anyhow::Result<wf_core::identity::DeviceKey> {
    Ok(workspace
        .data
        .device(false)?
        .context("no device key")?
        .key())
}

fn sorted_views(state: &State, include_archived: bool) -> Vec<ItemView<'_>> {
    let mut views: Vec<ItemView<'_>> = state
        .items()
        .filter(|view| include_archived || matches!(view, ItemView::Live(_)))
        .collect();
    views.sort_by_key(|view| match view {
        ItemView::Live(item) => (
            false,
            item.priority().is_none(),
            item.priority(),
            item.created(),
            item.id(),
        ),
        ItemView::Archived(stub) => (true, true, None, stub.archived_at(), stub.id()),
    });
    views
}

/// Which items a picker offers.
#[derive(Clone, Copy)]
enum Scope {
    Live,
    Archived,
    Any,
}

impl Scope {
    fn includes(self, view: &ItemView<'_>) -> bool {
        match (self, view) {
            (Scope::Any, _)
            | (Scope::Live, ItemView::Live(_))
            | (Scope::Archived, ItemView::Archived(_)) => true,
            (Scope::Live, ItemView::Archived(_)) | (Scope::Archived, ItemView::Live(_)) => false,
        }
    }
}

fn item_choice(summary: &ItemSummary) -> Choice {
    Choice {
        value: summary.id.clone(),
        label: format!(
            "#{}  {:<8}  {:<12}  {:<2}  {}",
            &summary.id[..wf_core::id::SHORT_ID_LEN],
            summary.item_type,
            summary.state.as_deref().unwrap_or("?"),
            summary.priority.as_deref().unwrap_or(""),
            summary.title
        ),
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
    let choices = sorted_views(state, true)
        .into_iter()
        .filter(|view| scope.includes(view))
        .map(|view| item_choice(&view::summary(state, &graph, view)))
        .collect();
    let picked = interactive::pick(what, choices)?;
    workspace.resolve_item(&picked)
}

fn choose_item(
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

fn choose_state(
    workspace: &Workspace,
    mode: Mode,
    id: ItemId,
    given: Option<StateName>,
) -> anyhow::Result<StateName> {
    if let Some(state) = given {
        return Ok(state);
    }
    if mode == Mode::Scripted {
        bail!("missing state; pass one, or run at a terminal to pick one");
    }
    let state = workspace.state();
    let item_type = state.item(id).expect("resolved ids exist").item_type();
    let policy = &state.policy().expect("a workforce has a policy").policy;
    let config = policy
        .type_config(item_type)
        .ok_or_else(|| anyhow!("no item type `{item_type}` in the policy"))?;
    let choices = config
        .states
        .iter()
        .map(|state| Choice {
            value: state.name.to_string(),
            label: format!("{}  ({})", state.name, state.category),
        })
        .collect();
    Ok(StateName::new(interactive::pick("state", choices)?)?)
}

fn item(
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
                draft: Draft {
                    title: args.title.unwrap_or_default(),
                    body: args.body.unwrap_or_default(),
                    priority: args.priority,
                    lane: args.lane,
                    labels: args.labels.into_iter().collect(),
                    requires: args.requires.into_iter().collect(),
                },
            };
            if wants_editor {
                new_item =
                    NewItemDraft::from_markdown(&edit_in_editor(&new_item.to_markdown()?, ".md")?)?;
            }
            let draft = new_item.draft;
            if draft.title.trim().is_empty() {
                bail!("the title is empty; nothing was created");
            }
            let event = workspace.author(EventBody::ItemCreated(ItemCreated {
                item_type: new_item.item_type,
                title: draft.title,
                body: draft.body,
                priority: draft.priority,
                lane: draft.lane,
                labels: draft.labels.into_iter().collect(),
                requires: draft.requires.into_iter().collect(),
            }))?;
            let id = ItemId::from(event.id());
            let state = workspace.state();
            let graph = state.dependency_graph();
            let created = view::summary(state, &graph, state.item(id).expect("just created"));
            emit(json, &created, |created| {
                format!(
                    "created #{}\n{}",
                    &created.id[..wf_core::id::SHORT_ID_LEN],
                    render::item_list(std::slice::from_ref(created))
                )
            })
        }
        ItemCommand::List(args) => {
            let state = workspace.state();
            let graph = state.dependency_graph();
            let views = sorted_views(state, args.archived);
            let items: Vec<ItemSummary> = views
                .into_iter()
                .map(|view| view::summary(state, &graph, view))
                .filter(|summary| {
                    args.item_type
                        .as_ref()
                        .is_none_or(|wanted| summary.item_type == wanted.as_str())
                })
                .filter(|summary| {
                    args.state
                        .as_ref()
                        .is_none_or(|wanted| summary.state.as_deref() == Some(wanted.as_str()))
                })
                .collect();
            emit(json, &items, |items| render::item_list(items))
        }
        ItemCommand::Ready => {
            let state = workspace.state();
            let graph = state.dependency_graph();
            let items: Vec<ItemSummary> = state
                .ready_items()
                .into_iter()
                .map(|item| view::summary(state, &graph, ItemView::Live(item)))
                .collect();
            emit(json, &items, |items| render::item_list(items))
        }
        ItemCommand::Show { item, history } => {
            let id = choose_item(workspace, mode, item, Scope::Any)?;
            show(json, mode, workspace, id, history)
        }
        ItemCommand::Edit(args) => edit(json, mode, workspace, args),
        ItemCommand::Move { item, state, note } => {
            let id = choose_item(workspace, mode, item, Scope::Live)?;
            let state = choose_state(workspace, mode, id, state)?;
            let event = workspace.author(EventBody::ItemTransitioned(ItemTransitioned {
                item: id,
                to: state.clone(),
                note,
            }))?;
            done(
                json,
                format!("moved #{} to {state}", id.short()),
                &event,
                Some(id),
            )
        }
        ItemCommand::Link(args) => link(json, mode, workspace, args),
        ItemCommand::Archive { item } => {
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
struct ArchivedShown {
    #[serde(flatten)]
    summary: ItemSummary,
    archived_at: String,
    note: &'static str,
}

#[derive(Serialize)]
#[serde(untagged)]
enum Shown {
    Live {
        #[serde(flatten)]
        detail: Box<view::ItemDetail>,
        history: Option<Vec<view::EventView>>,
    },
    Archived(ArchivedShown),
}

fn show(
    json: bool,
    mode: Mode,
    workspace: &Workspace,
    id: ItemId,
    history: bool,
) -> anyhow::Result<()> {
    let state = workspace.state();
    let store = workspace.replica.store();
    match state.item(id).expect("resolved ids exist") {
        ItemView::Archived(stub) => {
            let graph = state.dependency_graph();
            let shown = Shown::Archived(ArchivedShown {
                summary: view::summary(state, &graph, ItemView::Archived(stub)),
                archived_at: view::timestamp(stub.archived_at()),
                note: "archived: body and discussion load from the log on demand; `wf item unarchive` restores the item",
            });
            emit_paged(json, mode, &shown, |shown| match shown {
                Shown::Archived(archived) => format!(
                    "{}archived {}\n{}\n",
                    render::item_list(std::slice::from_ref(&archived.summary)),
                    archived.archived_at,
                    archived.note
                ),
                Shown::Live { .. } => unreachable!(),
            })
        }
        ItemView::Live(item) => {
            let detail =
                view::item_detail(state, item, |event_id| store.event(event_id).ok().flatten());
            let history = if history {
                let mut entries = Vec::new();
                for event in store.events()? {
                    if event_touches(&event, id) {
                        entries.push(view::history_entry(state, &event));
                    }
                }
                Some(entries)
            } else {
                None
            };
            let shown = Shown::Live {
                detail: Box::new(detail),
                history,
            };
            emit_paged(json, mode, &shown, |shown| match shown {
                Shown::Live { detail, history } => {
                    let mut out = render::item_detail(detail);
                    if let Some(history) = history {
                        out.push_str("\nhistory:\n");
                        for entry in history {
                            out.push_str(&format!(
                                "  {} {} {}\n",
                                entry.at, entry.actor, entry.summary
                            ));
                        }
                    }
                    out
                }
                Shown::Archived(_) => unreachable!(),
            })
        }
    }
}

fn event_touches(event: &Event, id: ItemId) -> bool {
    match &event.content.body {
        EventBody::ItemCreated(_) => ItemId::from(event.id()) == id,
        EventBody::ItemEdited(edited) => edited.item == id,
        EventBody::ItemTransitioned(transition) => transition.item == id,
        EventBody::ItemLinked(linked) => linked.item == id,
        EventBody::ItemArchived(reference) | EventBody::ItemUnarchived(reference) => {
            reference.item == id
        }
        EventBody::Comment(comment) => comment.item == id,
        EventBody::Genesis(_)
        | EventBody::PolicySet(_)
        | EventBody::AgentDefined(_)
        | EventBody::Unknown { .. } => false,
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
        if args.clear_priority {
            draft.priority = None;
        }
        if let Some(priority) = args.priority {
            draft.priority = Some(priority);
        }
        if let Some(lane) = args.lane {
            draft.lane = Some(lane);
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
            draft = Draft::from_markdown(&edit_in_editor(&draft.to_markdown()?, ".md")?)?;
        }
        draft.changes(item)
    };
    if changes.is_empty() {
        return emit(
            json,
            &Done {
                message: "nothing changed".into(),
                event: String::new(),
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

fn comment(
    json: bool,
    mode: Mode,
    workspace: &mut Workspace,
    args: CommentArgs,
) -> anyhow::Result<()> {
    let id = choose_item(workspace, mode, args.item, Scope::Any)?;
    let mut text = args.text.join(" ");
    if args.edit || (text.is_empty() && mode == Mode::Interactive) {
        text = edit_in_editor(&text, ".md")?;
    }
    let text = text.trim();
    if text.is_empty() {
        bail!("the comment is empty; nothing was sent");
    }
    let event = workspace.author(EventBody::Comment(Comment {
        item: id,
        body: text.to_string(),
    }))?;
    done(
        json,
        format!("commented on #{}", id.short()),
        &event,
        Some(id),
    )
}

fn mentions(json: bool, workspace: &Workspace, target: &str) -> anyhow::Result<()> {
    let state = workspace.state();
    let mention = match refs::parse(target).into_iter().next() {
        Some(Token::Mention(mention)) => mention,
        _ => bail!("`{target}` is not a mention; write it like @joshua or @joshua/coder"),
    };
    let resolved: Option<_> = state.resolver().mention(&mention);
    let resolved = resolved.ok_or_else(|| {
        anyhow!(
            "{} does not name anyone in this workforce",
            Mention::to_string(&mention)
        )
    })?;
    let sources = state.mentions_of(&resolved);
    let mentions = MentionsView {
        target: view::mention_label(state, &resolved),
        sources: sources
            .iter()
            .map(|source| view::source_view(state, source))
            .collect(),
    };
    emit(json, &mentions, render::mentions)
}

fn edit_in_editor(initial: &str, suffix: &str) -> anyhow::Result<String> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());
    let file = tempfile::Builder::new().suffix(suffix).tempfile()?;
    std::fs::write(file.path(), initial)?;
    let mut words = editor.split_whitespace();
    let program = words.next().context("$EDITOR is empty")?;
    let status = std::process::Command::new(program)
        .args(words)
        .arg(file.path())
        .status()
        .with_context(|| format!("starting editor `{editor}`"))?;
    if !status.success() {
        bail!("editor `{editor}` failed; nothing was changed");
    }
    Ok(std::fs::read_to_string(file.path())?)
}

fn policy(json: bool, command: PolicyCommand, workspace: &mut Workspace) -> anyhow::Result<()> {
    let current = workspace
        .state()
        .policy()
        .expect("a workforce has a policy")
        .source
        .clone();
    match command {
        PolicyCommand::Show => emit(json, &current, |source| source.clone()),
        PolicyCommand::Apply { file } => {
            let source = std::fs::read_to_string(&file)
                .with_context(|| format!("reading {}", file.display()))?;
            apply_policy(json, workspace, source)
        }
        PolicyCommand::Edit => {
            let edited = edit_in_editor(&current, ".toml")?;
            if edited == current {
                return emit(json, &"policy unchanged", |message| format!("{message}\n"));
            }
            apply_policy(json, workspace, edited)
        }
    }
}

fn apply_policy(json: bool, workspace: &mut Workspace, source: String) -> anyhow::Result<()> {
    Policy::parse(&source)?;
    let principal = workspace.principal_signer()?;
    let workforce = workspace
        .state()
        .workforce_id()
        .expect("a workforce has an id");
    let statement = Signed::sign(&principal, Some(workforce), PolicySet { policy: source })?;
    let event = workspace.author(EventBody::PolicySet(statement))?;
    done(json, "policy replaced".to_string(), &event, None)
}

fn log(json: bool, mode: Mode, workspace: &Workspace) -> anyhow::Result<()> {
    let state = workspace.state();
    let events = workspace.replica.store().events()?;
    // Replay into an empty state with full verification: signatures, feeds and authority.
    let mut replay = State::new();
    let mut views = Vec::new();
    for event in events {
        let verified = event.verify_signature().is_ok()
            && replay.receive(event.clone(), Trust::Untrusted).is_ok();
        views.push(view::event_view(state, &event, verified));
    }
    if replay.pending_events().next().is_some() {
        bail!("the log is incomplete: some events are missing their parents");
    }
    let log = LogView {
        workforce: state
            .workforce_id()
            .map(|id| id.to_string())
            .unwrap_or_default(),
        events: views,
    };
    emit_paged(json, mode, &log, render::log)
}

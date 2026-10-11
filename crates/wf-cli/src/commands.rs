use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, anyhow, bail};
use serde::Serialize;
use wf_core::autonomy::Autonomy;
use wf_core::crdt::SetChange;
use wf_core::event::{
    AgentDefined, Change, Comment, CommentKind, Event, EventBody, Genesis, ItemCreated, ItemLinked,
    ItemMoved, ItemRef, Link, PolicySet,
};
use wf_core::id::{CommentId, HashId, ItemId, SessionId};
use wf_core::identity::{Actor, MemberSeed, PrincipalSigner, Signed};
use wf_core::item::{ItemView, Priority};
use wf_core::name::{AgentName, Handle, HarnessName, LaneName, StateName};
use wf_core::policy::Field;
use wf_core::policy::{DEFAULT_POLICY_TOML, Policy};
use wf_core::refs::{self, Mention, Token};
use wf_core::session::{SessionRole, Verdict};
use wf_core::state::{State, Trust};
use wf_daemon::proto::Finish;
use wf_store::{RedbStore, Replica};

use crate::cli::*;
use crate::draft::{Draft, NewItemDraft};
use crate::harness;
use crate::interactive::{self, Choice, Mode};
use crate::keys::DataDir;
use crate::render;
use crate::view::{self, Envelope, ItemSummary, LogView, MentionsView};
use crate::workspace::{self, WORKFORCE_FILE, WorkforceFile, Workspace};

pub(crate) fn emit<T: Serialize>(
    json: bool,
    data: &T,
    text: impl FnOnce(&T) -> String,
) -> anyhow::Result<()> {
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
        Command::Join(args) => crate::network::join(json, mode, args, data, cwd),
        Command::Fork {
            command: Some(ForkCommand::Accept { id, relay }),
            ..
        } => crate::fork::accept(json, data, cwd, &id, relay.as_deref()),
        Command::Daemon(args) => crate::network::daemon(json, args, data),
        Command::Permissions(command) => crate::permissions::permissions(json, command, cwd),
        Command::Status { refold } => {
            let workspace = Workspace::open(data, cwd, refold)?;
            emit(
                json,
                &view::status(
                    workspace.state(),
                    Some(workspace.device_key()),
                    &workspace.last_seen(),
                    refold,
                ),
                render::status,
            )
        }
        command => {
            let mut workspace = Workspace::open(data, cwd, false)?;
            match command {
                Command::Init(_)
                | Command::Join(_)
                | Command::Daemon(_)
                | Command::Permissions(_)
                | Command::Status { .. } => unreachable!("handled above"),
                Command::Session(command) => {
                    crate::session::session(json, mode, command, &mut workspace, cwd)
                }
                Command::Work(command) => work(json, command, &mut workspace),
                Command::Review(args) => review(json, mode, &mut workspace, args),
                Command::Inbox => {
                    let views = view::inbox_views(workspace.state(), &workspace.held_permissions());
                    emit(json, &views, |views| render::inbox(views))
                }
                Command::Agent(command) => agent(json, command, &mut workspace),
                Command::Item(command) => item(json, mode, command, &mut workspace),
                Command::Comment(args) => comment(json, mode, &mut workspace, args),
                Command::Mentions { target } => mentions(json, &workspace, &target),
                Command::Policy(command) => policy(json, command, &mut workspace),
                Command::Log => log(json, mode, &workspace),
                Command::Device(command) => crate::devices::device(json, command, &mut workspace),
                Command::Members => crate::devices::members(json, &workspace),
                Command::Fork {
                    command: Some(ForkCommand::List),
                    ..
                } => crate::fork::list(json, &workspace),
                Command::Fork {
                    args,
                    command: None,
                } => crate::fork::fork(json, &mut workspace, args, cwd),
                Command::Fork {
                    command: Some(ForkCommand::Accept { .. }),
                    ..
                } => unreachable!("handled above"),
                Command::Sync(args) => crate::network::sync(json, workspace, args),
                Command::Invite(InviteCommand::Device(args)) => {
                    crate::network::invite_device(json, &mut workspace, args)
                }
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
    let principal = PrincipalSigner::generate();
    let device = data.device(true)?.expect("created on demand");

    let genesis = Genesis {
        name: args.name.clone(),
        founder: MemberSeed {
            principal: principal.key(),
            handle: handle.clone(),
            devices: vec![device.key()],
            previous: Vec::new(),
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

    data.store_principal(&id, &principal)?;
    let mut replica = Replica::open(RedbStore::open(workspace::store_path(&data, &id)?)?, id)?;
    if replica.state().workforce_id().is_some() {
        bail!("a local log for workforce {id} already exists");
    }
    replica.receive(genesis_event)?;

    replica.author(
        &device,
        actor.clone(),
        workspace::now_ms(),
        EventBody::DeviceProfile(wf_core::event::DeviceProfile {
            name: args.device_name.unwrap_or_else(crate::network::host_name),
            hardware: crate::network::detect_hardware(),
            agents: Default::default(),
            max_sessions: 1,
        }),
    )?;

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
                    autonomy: Autonomy::default(),
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
                autonomy: args.autonomy.applied_to(Autonomy::default()),
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
                autonomy: args.autonomy.applied_to(current.autonomy),
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
pub(crate) enum Scope {
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
    let choices = sorted_views(state, true)
        .into_iter()
        .filter(|view| scope.includes(view))
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
            label: render::state_choice(&state.name.to_string(), state.state_category),
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
                    review: args.review,
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
                review: draft.review,
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
            let entries: Vec<(ItemView<'_>, ItemSummary)> = views
                .into_iter()
                .map(|view| (view, view::summary(state, &graph, view)))
                .filter(|(_, summary)| {
                    args.item_type
                        .as_ref()
                        .is_none_or(|wanted| summary.item_type == wanted.as_str())
                })
                .filter(|(_, summary)| {
                    args.state
                        .as_ref()
                        .is_none_or(|wanted| summary.state.as_deref() == Some(wanted.as_str()))
                })
                .filter(|(_, summary)| args.accepts(summary.readiness))
                .filter(|(_, summary)| {
                    dispatchable
                        .as_ref()
                        .is_none_or(|offered| offered.contains(&summary.id))
                })
                .collect();
            match args.group_by {
                None => {
                    let items: Vec<ItemSummary> =
                        entries.into_iter().map(|(_, summary)| summary).collect();
                    emit(json, &items, |items| render::item_list(items))
                }
                Some(grouping) => {
                    let groups = view::group_items(state, grouping, entries);
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
        ItemCommand::Move { item, state, note } => {
            let id = choose_item(workspace, mode, item, Scope::Live)?;
            let state = choose_state(workspace, mode, id, state)?;
            let because = comment_why(workspace, id, note)?;
            let event = workspace.author(EventBody::ItemMoved(ItemMoved {
                item: id,
                to: state.clone(),
                by_session: None,
                because,
            }))?;
            done(
                json,
                format!("moved #{} to {state}", id.short()),
                &event,
                Some(id),
            )
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
        match args.review {
            Some(ReviewChoice::Default) => draft.review = None,
            Some(ReviewChoice::Level(review)) => draft.review = Some(review),
            None => {}
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
                    event: String::new(),
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

fn work(json: bool, command: WorkCommand, workspace: &mut Workspace) -> anyhow::Result<()> {
    let session = workspace.session().ok_or_else(|| {
        anyhow!(
            "`wf work` is for agents inside a session; {} is not set",
            workspace::SESSION_VARIABLE
        )
    })?;
    match command {
        WorkCommand::Submit { note } => end(
            json,
            workspace,
            session,
            Finish::Submit { note },
            "submitted",
        ),
        WorkCommand::Release { note } => end(
            json,
            workspace,
            session,
            Finish::Release { note },
            "released",
        ),
    }
}

fn review(
    json: bool,
    mode: Mode,
    workspace: &mut Workspace,
    args: ReviewArgs,
) -> anyhow::Result<()> {
    let own_session = workspace.session().and_then(|id| {
        workspace
            .state()
            .session(id)
            .map(|session| (id, session.item(), session.role()))
    });
    let item = match (&args.item, own_session) {
        (Some(text), _) => workspace.resolve_item(text)?,
        (None, Some((_, item, _))) => item,
        (None, None) => choose_item(workspace, mode, None, Scope::Live)?,
    };
    let commit = match own_session {
        Some((_, own, SessionRole::Review { commit })) if own == item => commit,
        Some(_) | None => {
            workspace
                .state()
                .submission(item)
                .ok_or_else(|| anyhow!("#{} has no submitted work to review", item.short()))?
                .0
        }
    };
    let verdict = if args.approve {
        Verdict::Approve
    } else if args.request_changes {
        Verdict::RequestChanges
    } else {
        Verdict::Escalate
    };
    let because = comment_why(workspace, item, args.note)?;
    let event = workspace.author(EventBody::Review(wf_core::event::Review {
        item,
        commit,
        verdict,
        because,
    }))?;
    if verdict == Verdict::RequestChanges {
        let open = {
            let state = workspace.state();
            let policy = &state.policy().expect("a workforce has a policy").policy;
            let ItemView::Live(live) = state.item(item).expect("resolved ids exist") else {
                bail!("#{} is archived", item.short());
            };
            policy
                .type_config(live.item_type())
                .and_then(|config| config.first_state(wf_core::policy::StateCategory::Open))
                .cloned()
        };
        if let Some(open) = open {
            workspace.author(EventBody::ItemMoved(ItemMoved {
                item,
                to: open,
                by_session: None,
                because: None,
            }))?;
        }
    }
    if let Some((session, _, SessionRole::Review { .. })) = own_session {
        workspace.end_session(session, Finish::Reviewed)?;
    }
    let message = match verdict {
        Verdict::Approve => format!("approved #{}", item.short()),
        Verdict::RequestChanges => format!("requested changes on #{}", item.short()),
        Verdict::Escalate => format!("escalated #{} to a person", item.short()),
    };
    done(json, message, &event, Some(item))
}

#[derive(Serialize)]
struct DiffView {
    item: String,
    commit: String,
    diff: String,
}

/// The repository of the workforce here: the directory of its `.workforce.toml`.
fn repository() -> anyhow::Result<wf_vcs::GitCli> {
    let (file, _) = workspace::find_workforce(&std::env::current_dir()?)?;
    let repository = file
        .parent()
        .ok_or_else(|| anyhow!("the workforce file has no directory"))?;
    Ok(wf_vcs::GitCli::open(repository)?)
}

/// The latest commit of the work on an item: the head of its branch, or after the branch is
/// gone, what was submitted. A merged item says where its work went.
fn work_commit(
    workspace: &Workspace,
    git: &wf_vcs::GitCli,
    id: ItemId,
) -> anyhow::Result<wf_core::session::CommitId> {
    match wf_vcs::VcsAdapter::branch_head(git, &wf_vcs::item_branch(id)) {
        Ok(commit) => return Ok(commit),
        Err(wf_vcs::VcsError::NoBranch(_)) => {}
        Err(error) => return Err(error.into()),
    }
    match (
        workspace.state().review_status(id),
        workspace.state().submission(id),
    ) {
        (Some(wf_core::review::ReviewStatus::Merged { commit }), _) => bail!(
            "the work of #{} was merged as {}",
            id.short(),
            &commit.to_string()[..8]
        ),
        (_, Some((Some(commit), _))) => Ok(commit),
        (_, Some((None, _))) => bail!(
            "#{} was submitted without commits, there is no work to show",
            id.short()
        ),
        (_, None) => bail!("#{} has no work yet", id.short()),
    }
}

fn target(workspace: &Workspace) -> String {
    workspace
        .state()
        .policy()
        .expect("a workforce has a policy")
        .policy
        .merge
        .target
        .clone()
}

fn diff(
    json: bool,
    mode: Mode,
    workspace: &Workspace,
    id: ItemId,
    git_args: &[String],
) -> anyhow::Result<()> {
    let git = repository()?;
    let commit = work_commit(workspace, &git, id)?;
    let target = target(workspace);
    match mode {
        Mode::Interactive => Ok(wf_vcs::VcsAdapter::show_diff(
            &git, &target, commit, git_args,
        )?),
        Mode::Scripted => {
            let view = DiffView {
                item: id.to_base32(),
                commit: commit.to_string(),
                diff: wf_vcs::VcsAdapter::diff(&git, &target, commit, git_args)?,
            };
            emit(json, &view, |view| view.diff.clone())
        }
    }
}

#[derive(Serialize)]
struct CheckoutView {
    /// The item checked out, or `None` after going back.
    item: Option<String>,
    commit: Option<String>,
}

fn checkout(
    json: bool,
    mode: Mode,
    workspace: &Workspace,
    item: Option<String>,
) -> anyhow::Result<()> {
    let git = repository()?;
    if item.as_deref() == Some("-") {
        wf_vcs::VcsAdapter::checkout_previous(&git)?;
        let view = CheckoutView {
            item: None,
            commit: None,
        };
        return emit(json, &view, |_| {
            "back where the checkout was before\n".to_string()
        });
    }
    let id = choose_item(workspace, mode, item, Scope::Live)?;
    let commit = work_commit(workspace, &git, id)?;
    wf_vcs::VcsAdapter::checkout_detached(&git, commit)?;
    let view = CheckoutView {
        item: Some(id.to_base32()),
        commit: Some(commit.to_string()),
    };
    emit(json, &view, |view| {
        format!(
            "checked out the work of #{} at {}, detached; `wf item checkout -` goes back\n",
            id.short(),
            &view.commit.as_deref().unwrap_or_default()[..8]
        )
    })
}

#[derive(Serialize)]
struct PathView {
    item: String,
    path: String,
}

fn path(json: bool, mode: Mode, workspace: &Workspace, item: Option<String>) -> anyhow::Result<()> {
    let id = choose_item(workspace, mode, item, Scope::Live)?;
    let path = workspace
        .data
        .workforce(&workspace.workforce())
        .join("worktrees")
        .join(wf_vcs::item_name(id));
    if !path.is_dir() {
        let merged = match workspace.state().review_status(id) {
            Some(wf_core::review::ReviewStatus::Merged { commit }) => {
                format!("; its work was merged as {}", &commit.to_string()[..8])
            }
            _ => String::new(),
        };
        bail!("#{} has no worktree on this machine{merged}", id.short());
    }
    let view = PathView {
        item: id.to_base32(),
        path: path.display().to_string(),
    };
    emit(json, &view, |view| format!("{}\n", view.path))
}

fn end(
    json: bool,
    workspace: &mut Workspace,
    session: SessionId,
    finish: Finish,
    verb: &str,
) -> anyhow::Result<()> {
    let item = workspace
        .state()
        .session(session)
        .expect("the session was resolved when the workspace was opened")
        .item();
    workspace.end_session(session, finish)?;
    emit(
        json,
        &Done {
            message: format!("{verb} #{}", item.short()),
            event: String::new(),
            item: Some(item.to_base32()),
        },
        |done| format!("{}\n", done.message),
    )
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
    let id = choose_item(workspace, mode, args.item.clone(), Scope::Any)?;
    let resolves = comments_to_resolve(workspace, id, &args)?;
    let mut text = args.text.join(" ");
    if args.edit || (text.is_empty() && resolves.is_empty() && mode == Mode::Interactive) {
        text = edit_in_editor(&text, ".md")?;
    }
    let text = text.trim();
    if text.is_empty() && resolves.is_empty() {
        bail!("the comment is empty; nothing was sent");
    }
    let kind = if args.block {
        CommentKind::Blocker
    } else if args.request_change {
        CommentKind::ChangeRequest
    } else {
        CommentKind::Plain
    };
    let step = args
        .step
        .map(|number| step_numbered(workspace, id, number))
        .transpose()?;
    let event = workspace.author(EventBody::Comment(Comment {
        item: id,
        text: text.to_string(),
        kind,
        resolves: resolves.clone(),
        step,
    }))?;
    if let (Some(step), true) = (step, args.done) {
        workspace.author(EventBody::ItemEdited(wf_core::event::ItemEdited {
            item: id,
            changes: vec![Change::StepDone { step, done: true }],
        }))?;
    }
    let comment = CommentId::from(event.id());
    let mut message = format!(
        "wrote {} {} on #{}",
        kind.name().replace('_', " "),
        comment.short(),
        id.short()
    );
    if !resolves.is_empty() {
        let ids: Vec<String> = resolves.iter().map(HashId::short).collect();
        message.push_str(&format!(", resolving {}", ids.join(", ")));
    }
    match (args.step, args.done) {
        (Some(number), true) => message.push_str(&format!(", and checked step {number}")),
        (Some(number), false) => message.push_str(&format!(", on step {number}")),
        (None, _) => {}
    }
    // A supervise session is over once it has answered or escalated.
    let supervising = workspace.session().filter(|session| {
        workspace
            .state()
            .session(*session)
            .is_some_and(|session| session.item() == id && session.role() == SessionRole::Supervise)
    });
    if let (Some(session), false) = (supervising, resolves.is_empty()) {
        workspace.end_session(session, Finish::Supervised)?;
    }
    done(json, message, &event, Some(id))
}

/// The id of a step of the item's body, numbered as `wf item show` prints them.
fn step_numbered(
    workspace: &Workspace,
    item: ItemId,
    number: std::num::NonZeroUsize,
) -> anyhow::Result<wf_core::body::StepId> {
    let state = workspace.state();
    let Some(ItemView::Live(live)) = state.item(item) else {
        bail!("#{} is archived", item.short());
    };
    let body = live.body();
    let steps: Vec<_> = body.steps().collect();
    match steps.get(number.get() - 1) {
        Some(step) => Ok(step.id),
        None if steps.is_empty() => bail!("#{} has no steps", item.short()),
        None => bail!(
            "#{} has {} steps, there is no step {number}",
            item.short(),
            steps.len()
        ),
    }
}

/// Writes why a move or verdict is made as a plain comment on the item, for the move or verdict
/// to name.
fn comment_why(
    workspace: &mut Workspace,
    item: ItemId,
    why: Option<String>,
) -> anyhow::Result<Option<CommentId>> {
    let Some(why) = why.filter(|why| !why.trim().is_empty()) else {
        return Ok(None);
    };
    let written = workspace.author(EventBody::Comment(Comment::plain(item, why.trim())))?;
    Ok(Some(CommentId::from(written.id())))
}

/// The blockers and change requests a comment resolves: those named, and every open one of the
/// kinds asked for, as they are open right now. The event names them, so one written meanwhile
/// on another device stays open.
fn comments_to_resolve(
    workspace: &Workspace,
    item: ItemId,
    args: &CommentArgs,
) -> anyhow::Result<Vec<CommentId>> {
    let state = workspace.state();
    let Some(ItemView::Live(live)) = state.item(item) else {
        if args.resolve.is_empty() && !args.resolve_blockers && !args.resolve_change_requests {
            return Ok(Vec::new());
        }
        bail!(
            "#{} is archived; its comments cannot be resolved",
            item.short()
        );
    };
    let mut resolves = Vec::new();
    for prefix in &args.resolve {
        let comments = live.comments().keys().copied();
        resolves.push(wf_core::id::resolve_prefix(prefix, comments)?);
    }
    for (wanted, flag) in [
        (CommentKind::Blocker, args.resolve_blockers),
        (CommentKind::ChangeRequest, args.resolve_change_requests),
    ] {
        if !flag {
            continue;
        }
        let open: Vec<CommentId> = live
            .open_comments()
            .into_iter()
            .filter(|(_, comment)| comment.kind == wanted)
            .map(|(id, _)| id)
            .collect();
        if open.is_empty() {
            bail!(
                "#{} has no open {}s",
                item.short(),
                wanted.name().replace('_', " ")
            );
        }
        resolves.extend(open);
    }
    resolves.sort();
    resolves.dedup();
    Ok(resolves)
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
    // Replay in the order the events were applied into an empty state with full verification:
    // signatures, feeds and authority. A fork takes in its history newest event first, so this
    // is not the order they are shown in.
    let mut replay = match state.workforce_id() {
        Some(id) => State::expecting(id),
        None => State::new(),
    };
    let mut views = Vec::new();
    for event in workspace.events()? {
        let verified = event.verify_signature().is_ok()
            && replay.receive(event.clone(), Trust::Untrusted).is_ok();
        views.push((
            (event.content.hlc, event.id()),
            view::event_view(state, &event, verified),
        ));
    }
    if replay.pending_events().next().is_some() || replay.history_incomplete() {
        bail!("the log is incomplete: some events are missing their parents");
    }
    views.sort_by_key(|(order, _)| *order);
    let views = views.into_iter().map(|(_, view)| view).collect();
    let log = LogView {
        workforce: state
            .workforce_id()
            .map(|id| id.to_string())
            .unwrap_or_default(),
        events: views,
    };
    emit_paged(json, mode, &log, render::log)
}

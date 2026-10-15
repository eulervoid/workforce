//! The commands of `wf`: `run` hands each to its module; the output helpers they share live here.

mod agent;
mod check;
mod comment;
mod init;
mod item;
mod log;
mod policy;
mod review;
mod vcs;
mod work;

pub(crate) use item::{Scope, choose_item, sorted_views};
pub(crate) use review::review;
pub(crate) use vcs::{repository, target, work_commit};

use std::path::Path;

use anyhow::{Context, bail};
use serde::Serialize;
use wf_core::event::Event;
use wf_core::id::{HashId, ItemId};
use wf_core::name::Handle;

use crate::cli::*;
use crate::data_dir::DataDir;
use crate::harness;
use crate::interactive::{self, Mode};
use crate::render;
use crate::view::{self, Envelope};
use crate::workspace::Workspace;
use agent::agent;
use check::check;
use comment::{comment, mentions};
use init::init;
use item::item;
use log::log;
use policy::policy;
use work::work;

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
/// One JSON object per line with `--json`, so a reader can follow; otherwise `text`.
pub(crate) fn emit_line<T: Serialize>(
    json: bool,
    data: &T,
    text: impl FnOnce(&T) -> String,
) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string(&Envelope::new(data))?);
    } else {
        anstream::print!("{}", text(data));
    }
    std::io::Write::flush(&mut std::io::stdout())?;
    Ok(())
}

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
    /// The event the command wrote; `None` when it wrote none.
    pub event: Option<String>,
    pub item: Option<String>,
}

fn done(json: bool, message: String, event: &Event, item: Option<ItemId>) -> anyhow::Result<()> {
    emit(
        json,
        &Done {
            message,
            event: Some(event.id().to_string()),
            item: item.map(|id| id.to_base32()),
        },
        |done| format!("{}\n", done.message),
    )
}

pub fn run(cli: Cli, data: DataDir, cwd: &Path) -> anyhow::Result<()> {
    let json = cli.json;
    let mode = Mode::detect(json);
    let Some(command) = cli.command else {
        return dashboard(mode, data, cwd, cli.item.as_deref());
    };
    match command {
        Command::Init(args) => init(json, mode, args, data, cwd),
        Command::Join(args) => crate::network::join(json, mode, args, data, cwd),
        Command::Fork {
            command: Some(ForkCommand::Accept { id, relay }),
            ..
        } => crate::fork::accept(json, data, cwd, &id, relay.as_deref()),
        Command::Daemon(command) => crate::daemon::daemon(json, command, data),
        Command::Import { file } => crate::export::import(json, &file, &data),
        Command::Permissions(command) => crate::permissions::permissions(json, command, cwd),
        Command::Harness(HarnessCommand::Models { harness }) => harness::models(json, &harness),
        Command::Status { refold } => {
            let workspace = Workspace::open(data, cwd, refold)?;
            let waiting_on_you = view::inbox_views(
                workspace.state(),
                &workspace.held_permissions(),
                &workspace.machine_faults(),
            )
            .len();
            let context = view::StatusContext {
                own: Some(workspace.device_key()),
                last_seen: &workspace.last_seen(),
                daemon: workspace.follows_daemon(),
                waiting_on_you,
                refolded: refold,
                caches: cache_usage(&workspace),
            };
            emit(
                json,
                &view::status(workspace.state(), context),
                render::status,
            )
        }
        command => {
            let mut workspace = Workspace::open(data, cwd, false)?;
            match command {
                Command::Init(_)
                | Command::Join(_)
                | Command::Daemon(_)
                | Command::Import { .. }
                | Command::Permissions(_)
                | Command::Harness(_)
                | Command::Status { .. } => unreachable!("handled above"),
                Command::Session(command) => {
                    crate::session::session(json, mode, command, &mut workspace, cwd)
                }
                Command::Work(command) => work(json, command, &mut workspace),
                Command::Review(args) => review(json, mode, &mut workspace, args),
                Command::Check { names } => check(json, &workspace, cwd, names),
                Command::Inbox => {
                    let views = view::inbox_views(
                        workspace.state(),
                        &workspace.held_permissions(),
                        &workspace.machine_faults(),
                    );
                    emit(json, &views, |views| render::inbox(views))
                }
                Command::Agent(command) => agent(json, command, &mut workspace),
                Command::Item(command) => item(json, mode, command, &mut workspace),
                Command::Comment(args) => comment(json, mode, &mut workspace, args),
                Command::Mentions { target } => mentions(json, &workspace, &target),
                Command::Policy(command) => policy(json, command, &mut workspace),
                Command::Log(args) => log(json, mode, &mut workspace, args),
                Command::Stats => {
                    let views = view::item_stats_views(workspace.state());
                    emit_paged(json, mode, &views, |views| render::item_stats_list(views))
                }
                Command::Export => crate::export::export(&workspace),
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

/// What the caches in the worktrees of the workforce take on this machine, when the machine
/// config sets a budget for them.
fn cache_usage(workspace: &Workspace) -> Option<view::CacheUsage> {
    let path = wf_daemon::machine_config::MachineConfig::path_from_env()?;
    let storage = wf_daemon::machine_config::MachineConfig::load(&path)
        .ok()?
        .storage?;
    let worktrees = wf_vcs::worktrees(&workspace.data.workforce(&workspace.workforce()));
    let measured = wf_daemon::caches::measure(&worktrees, &storage.caches);
    Some(view::CacheUsage {
        used_bytes: wf_daemon::caches::total(&measured),
        budget_bytes: storage.budget.as_u64(),
    })
}

/// Plain `wf`: the dashboard at a terminal. Anywhere else nobody could use it, so the help.
fn dashboard(mode: Mode, data: DataDir, cwd: &Path, item: Option<&str>) -> anyhow::Result<()> {
    match (mode, item) {
        (Mode::Interactive, _) => {}
        (Mode::Scripted, Some(item)) => {
            bail!(
                "the dashboard needs a terminal; `wf log --follow --item {item}` prints the events of the item"
            )
        }
        (Mode::Scripted, None) => {
            anstream::eprint!("{}", crate::cli::command().render_help().ansi());
            std::process::exit(2);
        }
    }
    let mut workspace = Workspace::open(data, cwd, false)?;
    if !workspace.follows_daemon() {
        bail!("the dashboard follows the daemon; start it with `wf daemon start`");
    }
    let only = item.map(|text| workspace.resolve_item(text)).transpose()?;
    crate::dashboard::run(&mut workspace, only)
}

fn handle_from_environment() -> Handle {
    std::env::var("USER")
        .ok()
        .map(|user| user.to_lowercase().replace([' ', '.'], "-"))
        .and_then(|user| Handle::new(user).ok())
        .unwrap_or_else(|| Handle::new("me").expect("a valid handle"))
}

pub(crate) fn edit_in_editor(initial: &str, suffix: &str) -> anyhow::Result<String> {
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

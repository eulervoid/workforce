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
pub(crate) use vcs::{DeviceDiff, ItemDiff, item_diff, item_work};

use std::path::Path;

use anyhow::{Context, bail};
use serde::Serialize;
use wf_core::event::Event;
use wf_core::id::{HashId, ItemId};
use wf_core::name::Handle;

use crate::cli::*;
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
use wf_daemon::DataDir;
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

/// Like [`emit`], but one JSON object per line with `--json`, so a reader can follow, and flushed
/// at once.
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

/// Like [`emit`], but long text goes through the pager when a terminal is attached.
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
    /// The session the command started; `None` when it started none.
    pub session: Option<String>,
    pub item: Option<String>,
}

fn done(json: bool, message: String, event: &Event, item: Option<ItemId>) -> anyhow::Result<()> {
    emit(
        json,
        &Done {
            message,
            event: Some(event.id().to_string()),
            session: None,
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
        Command::GitBridge { socket } => Ok(wf_daemon::run_git_bridge(&socket)?),
        Command::Import { file } => crate::export::import(json, &file, &data),
        Command::Permissions(command) => crate::permissions::permissions(json, command, cwd),
        Command::Harness(HarnessCommand::Models { harness }) => harness::models(json, &harness),
        Command::Status { refold } => {
            let workspace = Workspace::open(data, cwd, refold)?;
            let machine_faults = workspace.machine_faults()?;
            let waiting_on_you = view::inbox_count(
                workspace.state(),
                &workspace.held_permissions()?,
                &machine_faults,
            );
            let awaited = workspace.awaited_commits()?;
            let context = view::StatusContext {
                machine_faults: &machine_faults,
                awaited: &awaited,
                own: Some(workspace.device_key()),
                last_seen: &workspace.last_seen()?,
                daemon: workspace.follows_daemon(),
                waiting_on_you,
                refolded: refold,
                caches: workspace.cache_usage()?,
            };
            emit(
                json,
                &view::status(
                    workspace.state(),
                    &workspace.state().dependency_graph(),
                    context,
                ),
                render::status,
            )
        }
        Command::Session(command) => {
            crate::session::session(json, mode, command, &mut open(data, cwd)?)
        }
        Command::Work(command) => work(json, command, &mut open(data, cwd)?),
        Command::Review(args) => review(json, mode, &mut open(data, cwd)?, args),
        Command::Check { names } => check(json, &open(data, cwd)?, cwd, names),
        Command::Inbox => {
            let workspace = open(data, cwd)?;
            let views = view::inbox_views(
                workspace.state(),
                &workspace.held_permissions()?,
                &workspace.machine_faults()?,
            );
            emit(json, &views, |views| render::inbox(views))
        }
        Command::Agent(command) => agent(json, command, &mut open(data, cwd)?),
        Command::Item(command) => item(json, mode, command, &mut open(data, cwd)?),
        Command::Comment(args) => comment(json, mode, &mut open(data, cwd)?, args),
        Command::Mentions { target } => mentions(json, &open(data, cwd)?, &target),
        Command::Policy(command) => policy(json, command, &mut open(data, cwd)?),
        Command::Log(args) => log(json, mode, &mut open(data, cwd)?, args),
        Command::Stats => {
            let workspace = open(data, cwd)?;
            let views = view::item_stats_views(workspace.state());
            emit_paged(json, mode, &views, |views| render::item_stats_list(views))
        }
        Command::Export => crate::export::export(&open(data, cwd)?),
        Command::Device(command) => crate::devices::device(json, command, &mut open(data, cwd)?),
        Command::Members => crate::devices::members(json, &open(data, cwd)?),
        Command::Fork {
            command: Some(ForkCommand::List),
            ..
        } => crate::fork::list(json, &open(data, cwd)?),
        Command::Fork {
            args,
            command: None,
        } => crate::fork::fork(json, &mut open(data, cwd)?, args, cwd),
        Command::Sync(args) => crate::network::sync(json, open(data, cwd)?, args),
        Command::Invite(InviteCommand::Device(args)) => {
            crate::network::invite_device(json, &mut open(data, cwd)?, args)
        }
    }
}

/// The workforce of the working directory, for the commands that read or write its log.
fn open(data: DataDir, cwd: &Path) -> anyhow::Result<Workspace> {
    Workspace::open(data, cwd, false)
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

/// The command line of the first of `names` that is set and not empty, else `default`.
pub(crate) fn command_line_from_environment(names: &[&str], default: &str) -> String {
    names
        .iter()
        .find_map(|name| {
            std::env::var(name)
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_else(|| default.to_string())
}

/// Run `command_line` as git runs its editor and pager: through `/bin/sh` (git's default
/// `SHELL_PATH`, so it works with any `PATH`), with `arguments` appended, so quoting and paths
/// with spaces work.
pub(crate) fn shell_command(command_line: &str, arguments: &[&Path]) -> std::process::Command {
    let mut command = std::process::Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(format!("{command_line} \"$@\""))
        .arg("sh")
        .args(arguments);
    command
}

pub(crate) fn edit_in_editor(initial: &str, suffix: &str) -> anyhow::Result<String> {
    let editor = command_line_from_environment(&["VISUAL", "EDITOR"], "vi");
    let file = tempfile::Builder::new().suffix(suffix).tempfile()?;
    std::fs::write(file.path(), initial)?;
    let status = shell_command(&editor, &[file.path()])
        .status()
        .with_context(|| format!("starting editor `{editor}`"))?;
    if !status.success() {
        bail!("editor `{editor}` failed; nothing was changed");
    }
    Ok(std::fs::read_to_string(file.path())?)
}

const ERROR_MARKER: &str = "wf-error:";

/// Edit `initial` in the editor until `parse` accepts it. When it does not, the editor reopens
/// with the text and the error as a comment (`#` for TOML, `<!-- -->` for markdown). Leaving the
/// text as it was shown gives up with the error.
pub(crate) fn edit_until_valid<T>(
    initial: &str,
    suffix: &str,
    parse: impl Fn(&str) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    edit_until_valid_with(
        |shown| edit_in_editor(shown, suffix),
        initial,
        suffix,
        parse,
    )
}

fn edit_until_valid_with<T>(
    mut edit: impl FnMut(&str) -> anyhow::Result<String>,
    initial: &str,
    suffix: &str,
    parse: impl Fn(&str) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let toml_marker = format!("# {ERROR_MARKER}");
    let markdown_marker = format!("<!-- {ERROR_MARKER}");
    let mut shown = initial.to_string();
    loop {
        let edited = edit(&shown)?;
        let mut text = edited
            .lines()
            .filter(|line| !line.starts_with(&toml_marker) && !line.starts_with(&markdown_marker))
            .collect::<Vec<_>>()
            .join("\n");
        if edited.ends_with('\n') {
            text.push('\n');
        }
        let error = match parse(&text) {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        if edited == shown && shown != initial {
            return Err(error.context("the text is still invalid; nothing was changed"));
        }
        let message = format!("{error:#}");
        let comment = if suffix == ".toml" {
            message
                .lines()
                .map(|line| format!("{toml_marker} {line}\n"))
                .collect::<String>()
        } else {
            format!(
                "{markdown_marker} {} -->\n",
                message.replace('\n', " ").replace("-->", "--")
            )
        };
        if !text.ends_with('\n') {
            text.push('\n');
        }
        shown = text + &comment;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_variables_fall_back() {
        assert_eq!(
            command_line_from_environment(&["WF_TEST_UNSET_A", "WF_TEST_UNSET_B"], "vi"),
            "vi"
        );
    }

    #[test]
    fn commands_run_through_the_shell_with_the_argument_appended() {
        let output = shell_command("printf '%s|' --flag", &[Path::new("a file")])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout), "--flag|a file|");
    }

    type Shown = std::rc::Rc<std::cell::RefCell<Vec<String>>>;

    fn scripted(edits: &[&str]) -> (Shown, impl FnMut(&str) -> anyhow::Result<String>) {
        let shown: Shown = std::rc::Rc::default();
        let mut edits = edits
            .iter()
            .map(|edit| edit.to_string())
            .collect::<Vec<_>>();
        edits.reverse();
        let record = std::rc::Rc::clone(&shown);
        let edit = move |text: &str| {
            record.borrow_mut().push(text.to_string());
            Ok(edits.pop().expect("an edit left"))
        };
        (shown, edit)
    }

    fn parse_number(text: &str) -> anyhow::Result<u32> {
        text.trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("not a number"))
    }

    #[test]
    fn invalid_text_reopens_with_the_error_as_a_comment() {
        let (shown, edit) = scripted(&["abc\n", "42\n# wf-error: not a number\n"]);
        let value = edit_until_valid_with(edit, "1\n", ".toml", parse_number).unwrap();
        assert_eq!(value, 42);
        assert_eq!(shown.borrow()[1], "abc\n# wf-error: not a number\n");
    }

    #[test]
    fn markdown_gets_a_single_line_comment() {
        let (shown, edit) = scripted(&["abc\n", "7\n"]);
        edit_until_valid_with(edit, "1\n", ".md", parse_number).unwrap();
        assert_eq!(shown.borrow()[1], "abc\n<!-- wf-error: not a number -->\n");
    }

    #[test]
    fn leaving_invalid_text_unchanged_gives_up() {
        let (_, edit) = scripted(&["abc\n", "abc\n# wf-error: not a number\n"]);
        let error = edit_until_valid_with(edit, "1\n", ".toml", parse_number).unwrap_err();
        assert!(format!("{error:#}").contains("still invalid"));
    }

    #[test]
    fn only_the_appended_comment_lines_are_removed() {
        let (_, edit) = scripted(&["x\n"]);
        let kept = std::cell::RefCell::new(String::new());
        edit_until_valid_with(edit, "1\n", ".md", |text| {
            *kept.borrow_mut() = text.to_string();
            Ok(())
        })
        .unwrap();
        assert_eq!(*kept.borrow(), "x\n");

        let (_, edit) =
            scripted(&["mentions wf-error: here\n<!-- wf-error: old -->\n# wf-error: old\n"]);
        edit_until_valid_with(edit, "1\n", ".md", |text| {
            *kept.borrow_mut() = text.to_string();
            Ok(())
        })
        .unwrap();
        assert_eq!(*kept.borrow(), "mentions wf-error: here\n");
    }
}

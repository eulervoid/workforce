//! `wf watch`: the events of the workforce as they arrive, and a prompt for what waits on you.

use std::collections::BTreeSet;
use std::io::{BufRead, Write};
use std::time::Duration;

use anyhow::bail;
use wf_core::event::{Comment, CommentKind, Event, EventBody};
use wf_core::id::{HashId, ItemId};
use wf_core::inbox::Waiting;
use wf_core::item::ItemView;
use wf_daemon::proto::HeldPermission;
use wf_harness::Permission;

use crate::cli::{ReviewArgs, WatchArgs};
use crate::commands::{emit_line, repository, review, target, work_commit};
use crate::interactive::Mode;
use crate::workspace::Workspace;
use crate::{render, view};

/// How often the daemon is asked for what is new.
const POLL_EVERY: Duration = Duration::from_millis(500);

/// How many of the latest events are shown when watching starts.
const BACKLOG: usize = 10;

pub fn watch(
    json: bool,
    mode: Mode,
    workspace: &mut Workspace,
    args: WatchArgs,
) -> anyhow::Result<()> {
    if !workspace.follows_daemon() {
        bail!("`wf watch` follows the daemon; start it with `wf daemon start`");
    }
    let only = args
        .item
        .as_deref()
        .map(|text| workspace.resolve_item(text))
        .transpose()?;
    let mut events = workspace.events()?;
    let mut backlog: Vec<Event> = events
        .iter()
        .rev()
        .filter(|event| only.is_none() || workspace.state().item_of(event) == only)
        .take(BACKLOG)
        .cloned()
        .collect();
    backlog.reverse();
    show(json, workspace, &events, &backlog)?;
    // A prompt can be skipped; it comes back the next time `wf watch` starts.
    let mut asked = BTreeSet::new();
    let prompting = mode == Mode::Interactive && !json;
    loop {
        if prompting && !ask_what_waits(workspace, only, &mut asked)? {
            return Ok(());
        }
        std::thread::sleep(POLL_EVERY);
        workspace.refresh()?;
        let new = workspace.events_since(events.len() as u64)?;
        if new.is_empty() {
            continue;
        }
        events.extend(new.iter().cloned());
        let shown: Vec<Event> = new
            .into_iter()
            .filter(|event| only.is_none() || workspace.state().item_of(event) == only)
            .collect();
        show(json, workspace, &events, &shown)?;
    }
}

fn show(json: bool, workspace: &Workspace, events: &[Event], new: &[Event]) -> anyhow::Result<()> {
    for line in view::watch_lines(workspace.state(), events, new) {
        emit_line(json, &line, render::watch_line)?;
    }
    Ok(())
}

/// One thing that waits on you, with what it takes to answer it.
enum Prompt {
    Permission(HeldPermission),
    Blocker {
        item: ItemId,
        comment: wf_core::id::CommentId,
    },
    PrincipalReview {
        item: ItemId,
    },
    /// Shown once; it is answered elsewhere.
    Notice {
        item: ItemId,
        what: String,
    },
}

impl Prompt {
    /// What tells this prompt apart from the others, so it is asked once.
    fn key(&self) -> String {
        match self {
            Prompt::Permission(held) => format!("permission {} {}", held.session, held.since_ms),
            Prompt::Blocker { comment, .. } => format!("blocker {comment}"),
            Prompt::PrincipalReview { item } => format!("review {item}"),
            Prompt::Notice { item, what } => format!("notice {item} {what}"),
        }
    }
}

fn prompts(workspace: &Workspace, only: Option<ItemId>) -> Vec<Prompt> {
    let state = workspace.state();
    let mut prompts: Vec<Prompt> = workspace
        .held_permissions()
        .into_iter()
        .filter(|held| {
            only.is_none() || state.session(held.session).map(|session| session.item()) == only
        })
        .map(Prompt::Permission)
        .collect();
    for entry in state.inbox() {
        if only.is_some_and(|item| item != entry.item) {
            continue;
        }
        prompts.push(match entry.waiting {
            Waiting::Blocker { comment } => Prompt::Blocker {
                item: entry.item,
                comment,
            },
            Waiting::PrincipalReview { .. } => Prompt::PrincipalReview { item: entry.item },
            Waiting::Proposal | Waiting::Merge { .. } | Waiting::Recovery { .. } => {
                Prompt::Notice {
                    item: entry.item,
                    what: view::what_waits(state, &entry).1,
                }
            }
        });
    }
    prompts
}

/// Asks about each thing that waits and was not asked about yet. `false` when stdin is closed.
fn ask_what_waits(
    workspace: &mut Workspace,
    only: Option<ItemId>,
    asked: &mut BTreeSet<String>,
) -> anyhow::Result<bool> {
    for prompt in prompts(workspace, only) {
        if !asked.insert(prompt.key()) {
            continue;
        }
        let outcome = match prompt {
            Prompt::Permission(held) => ask_permission(workspace, &held),
            Prompt::Blocker { item, comment } => ask_blocker(workspace, item, comment),
            Prompt::PrincipalReview { item } => ask_review(workspace, item),
            Prompt::Notice { item, what } => {
                println!("{} {}", title(workspace, item), what);
                continue;
            }
        };
        // A question stands apart from the events around it, before and after.
        if matches!(outcome, Ok(true)) {
            println!();
        }
        match outcome {
            Ok(true) => {}
            Ok(false) => return Ok(false),
            // What was asked about may be gone already; say so and go on watching.
            Err(error) => eprintln!("error: {error:#}"),
        }
    }
    Ok(true)
}

fn title(workspace: &Workspace, item: ItemId) -> String {
    match workspace.state().item(item) {
        Some(ItemView::Live(live)) => format!("#{} \"{}\"", item.short(), live.title()),
        Some(ItemView::Archived(_)) | None => format!("#{}", item.short()),
    }
}

/// A line from stdin, trimmed, or `None` when it is closed.
fn read_line(prompt: &str) -> anyhow::Result<Option<String>> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line)? == 0 {
        return Ok(None);
    }
    Ok(Some(line.trim().to_string()))
}

fn ask_permission(workspace: &mut Workspace, held: &HeldPermission) -> anyhow::Result<bool> {
    let state = workspace.state();
    let (agent, item) = match state.session(held.session) {
        Some(session) => (
            format!("@{}", session.agent().name),
            title(workspace, session.item()),
        ),
        None => ("an agent".to_string(), String::new()),
    };
    println!("\n? {agent} on {item} asks to: {}", held.what);
    for why in &held.passed {
        println!("  passed by {why}");
    }
    loop {
        let Some(answer) = read_line("  [a]llow, [d]eny or [s]kip: ")? else {
            return Ok(false);
        };
        let answer = match answer.as_str() {
            "a" | "allow" | "y" | "yes" => Permission::Allow,
            "d" | "deny" | "n" | "no" => Permission::Deny,
            "s" | "skip" | "" => {
                println!("  skipped");
                return Ok(true);
            }
            _ => continue,
        };
        workspace.answer_permission(held.session, answer)?;
        println!(
            "  {}",
            match answer {
                Permission::Allow => "allowed",
                Permission::Deny => "denied",
            }
        );
        return Ok(true);
    }
}

fn ask_blocker(
    workspace: &mut Workspace,
    item: ItemId,
    comment: wf_core::id::CommentId,
) -> anyhow::Result<bool> {
    let text = match workspace.event(&comment.into())? {
        Some(Event {
            content:
                wf_core::event::EventContent {
                    body: EventBody::Commented(blocker),
                    ..
                },
            ..
        }) => blocker.text,
        Some(_) | None => String::new(),
    };
    println!(
        "\n⧗ blocker {} on {} waits on you:",
        comment.short(),
        title(workspace, item)
    );
    for line in text.lines() {
        println!("  {line}");
    }
    let Some(answer) = read_line("  answer (empty skips): ")? else {
        return Ok(false);
    };
    if answer.is_empty() {
        println!("  skipped");
        return Ok(true);
    }
    workspace.author(EventBody::Commented(Comment {
        resolves: vec![comment],
        ..Comment::plain(item, answer)
    }))?;
    println!("  resolved {}", comment.short());
    Ok(true)
}

fn ask_review(workspace: &mut Workspace, item: ItemId) -> anyhow::Result<bool> {
    println!(
        "\n◎ the work on {} waits for your review",
        title(workspace, item)
    );
    let verdict = |approve: bool| ReviewArgs {
        item: Some(item.to_base32()),
        approve,
        request_changes: !approve,
        escalate: false,
        note: None,
    };
    loop {
        let Some(answer) = read_line("  [d]iff, [a]pprove, [r]equest changes or [s]kip: ")? else {
            return Ok(false);
        };
        match answer.as_str() {
            "d" | "diff" => {
                let git = repository()?;
                let commit = work_commit(workspace, &git, item)?;
                wf_vcs::VcsAdapter::show_diff(&git, &target(workspace), commit, &[])?;
            }
            "a" | "approve" => {
                review(false, Mode::Scripted, workspace, verdict(true))?;
                return Ok(true);
            }
            "r" | "request changes" => {
                println!("  one change per line, an empty line ends:");
                let mut changes = Vec::new();
                while let Some(change) = read_line("  △ ")? {
                    if change.is_empty() {
                        break;
                    }
                    changes.push(change);
                }
                if changes.is_empty() {
                    continue;
                }
                for change in changes {
                    workspace.author(EventBody::Commented(Comment {
                        kind: CommentKind::ChangeRequest,
                        ..Comment::plain(item, change)
                    }))?;
                }
                review(false, Mode::Scripted, workspace, verdict(false))?;
                return Ok(true);
            }
            "s" | "skip" | "" => {
                println!("  skipped");
                return Ok(true);
            }
            _ => {}
        }
    }
}

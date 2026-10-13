//! Carrying out what the keys of the dashboard ask of the workforce: the same events the commands
//! write, without their output, which would draw over the dashboard.

use std::io::Write;

use anyhow::anyhow;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event as TerminalEvent, KeyEventKind};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{EnterAlternateScreen, enable_raw_mode};
use wf_core::event::{Comment, CommentKind, EventBody, Review};
use wf_core::id::{CommentId, HashId, ItemId};
use wf_core::session::Verdict;
use wf_harness::Permission;

use super::app::Effect;
use crate::commands::{edit_in_editor, repository, target, work_commit};
use crate::workspace::Workspace;

/// What is written above this line in `$EDITOR` counts; what is below is there to read.
const SCISSORS: &str = "--- wf: everything below this line is left out ---";

/// Carries out `effect`, and says what it did. Quitting is up to the loop.
pub fn carry_out(
    effect: Effect,
    workspace: &mut Workspace,
    terminal: &mut DefaultTerminal,
) -> anyhow::Result<String> {
    match effect {
        Effect::Quit => Ok(String::new()),
        Effect::Permit { session, answer } => {
            let held = workspace.answer_permission(session, answer)?;
            let verb = match answer {
                Permission::Allow => "allowed",
                Permission::Deny => "denied",
            };
            Ok(format!("{verb}: {}", held.what))
        }
        Effect::Resolve {
            item,
            blocker,
            text,
        } => resolve(workspace, item, blocker, text),
        Effect::ResolveInEditor { item, blocker } => {
            let question = blocker_text(workspace, blocker);
            let template = format!(
                "\n{SCISSORS}\nYour answer to the blocker {} on #{}:\n\n{question}\n",
                blocker.short(),
                item.short()
            );
            let text = suspended(terminal, || edit_in_editor(&template, ".md"))??;
            let text = above_scissors(&text);
            if text.is_empty() {
                return Ok("nothing written, the blocker stays open".to_owned());
            }
            resolve(workspace, item, blocker, text)
        }
        Effect::Approve { item } => {
            verdict(workspace, item, Verdict::Approve)?;
            Ok(format!("approved #{}", item.short()))
        }
        Effect::RequestChanges { item } => {
            let template = format!(
                "\n{SCISSORS}\nThe changes the work of #{} needs, one per line.\n",
                item.short()
            );
            let text = suspended(terminal, || edit_in_editor(&template, ".md"))??;
            let text = above_scissors(&text);
            let changes: Vec<&str> = text
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .collect();
            if changes.is_empty() {
                return Ok("no changes written, the review still waits".to_owned());
            }
            for change in &changes {
                workspace.author(EventBody::Commented(Comment {
                    kind: CommentKind::ChangeRequest,
                    ..Comment::plain(item, *change)
                }))?;
            }
            verdict(workspace, item, Verdict::RequestChanges)?;
            Ok(format!(
                "requested {} change{} on #{}",
                changes.len(),
                if changes.len() == 1 { "" } else { "s" },
                item.short()
            ))
        }
        Effect::ShowDiff { item } => {
            let git = repository()?;
            let commit = work_commit(workspace, &git, item)?;
            let target = target(workspace);
            suspended(terminal, || -> anyhow::Result<()> {
                wf_vcs::VcsAdapter::show_diff(&git, &target, commit, &[])?;
                // A short diff is printed without a pager: give it time to be read.
                print!("\npress any key to go back to the dashboard");
                std::io::stdout().flush()?;
                enable_raw_mode()?;
                loop {
                    if let TerminalEvent::Key(key) = event::read()?
                        && key.kind == KeyEventKind::Press
                    {
                        return Ok(());
                    }
                }
            })??;
            Ok(String::new())
        }
    }
}

fn resolve(
    workspace: &mut Workspace,
    item: ItemId,
    blocker: CommentId,
    text: String,
) -> anyhow::Result<String> {
    workspace.author(EventBody::Commented(Comment {
        resolves: vec![blocker],
        ..Comment::plain(item, text)
    }))?;
    Ok(format!("resolved {}", blocker.short()))
}

fn verdict(workspace: &mut Workspace, item: ItemId, verdict: Verdict) -> anyhow::Result<()> {
    let (commit, _) = workspace
        .state()
        .submission(item)
        .ok_or_else(|| anyhow!("#{} has no submitted work to review", item.short()))?;
    workspace.author(EventBody::Reviewed(Review {
        item,
        commit,
        verdict,
        because: None,
    }))?;
    Ok(())
}

/// The text of a blocker, quoted, for reading it while answering.
fn blocker_text(workspace: &Workspace, blocker: CommentId) -> String {
    let text = match workspace.event(&blocker.into()) {
        Ok(Some(event)) => match event.content.body {
            EventBody::Commented(comment) => comment.text,
            _ => String::new(),
        },
        Ok(None) | Err(_) => String::new(),
    };
    text.lines()
        .map(|line| format!("> {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn above_scissors(text: &str) -> String {
    text.split(SCISSORS)
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned()
}

/// Runs `run` on the plain terminal, as the dashboard left it, and draws the dashboard again
/// afterwards.
fn suspended<T>(terminal: &mut DefaultTerminal, run: impl FnOnce() -> T) -> anyhow::Result<T> {
    ratatui::restore();
    let result = run();
    enable_raw_mode()?;
    execute!(std::io::stdout(), EnterAlternateScreen)?;
    terminal.clear()?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_what_is_above_the_scissors_counts() {
        let text = format!("Use humantime.\n\n{SCISSORS}\n> Which crate?\n");
        assert_eq!(above_scissors(&text), "Use humantime.");
        assert_eq!(above_scissors(&format!("\n{SCISSORS}\n> Which?")), "");
    }
}

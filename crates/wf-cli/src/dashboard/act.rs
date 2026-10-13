//! Carrying out what the keys of the dashboard ask of the workforce: the same events the commands
//! write, without their output, which would draw over the dashboard.

use std::io::Write;

use anyhow::{anyhow, bail};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event as TerminalEvent, KeyEventKind};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{EnterAlternateScreen, enable_raw_mode};
use wf_core::event::{Comment, CommentKind, EventBody, ItemEdited, ItemMoved, Review};
use wf_core::id::{CommentId, HashId, ItemId};
use wf_core::item::{ItemView, Standing};
use wf_core::name::{AgentName, StateName};
use wf_core::policy::StateCategory;
use wf_core::session::Verdict;
use wf_harness::Permission;

use super::app::{Choice, Effect};
use crate::commands::{edit_in_editor, repository, target, work_commit};
use crate::draft::Draft;
use crate::workspace::{self, Workspace};

/// What is written above this line in `$EDITOR` counts; what is below is there to read.
const SCISSORS: &str = "--- wf: everything below this line is left out ---";

/// What carrying out an effect came to.
pub enum Outcome {
    /// What it did, to say so.
    Did(String),
    /// Something to pick before it can be done.
    Choose(Choice),
}

/// Carries out `effect`. Quitting is up to the loop.
pub fn carry_out(
    effect: Effect,
    workspace: &mut Workspace,
    terminal: &mut DefaultTerminal,
) -> anyhow::Result<Outcome> {
    match effect {
        Effect::ChooseAgent { item } => Ok(Outcome::Choose(agents(workspace, item)?)),
        Effect::ChooseState { item } => Ok(Outcome::Choose(states(workspace, item)?)),
        effect => done(effect, workspace, terminal).map(Outcome::Did),
    }
}

/// The agents of the principal of this machine, the policy's default agent first.
fn agents(workspace: &Workspace, item: ItemId) -> anyhow::Result<Choice> {
    let state = workspace.state();
    let principal = state
        .principal_of(&workspace.device_key())
        .ok_or_else(|| anyhow!("this device is not bound to a member"))?;
    let default = state
        .policy()
        .and_then(|stored| stored.policy.default_agent.clone());
    let mut agents: Vec<AgentName> = state
        .agents()
        .filter(|(id, _)| id.principal == principal)
        .map(|(id, _)| id.name.clone())
        .collect();
    agents.sort_by_key(|agent| Some(agent) != default.as_ref());
    if agents.is_empty() {
        bail!("you have no agents; add one with `wf agent add`");
    }
    Ok(Choice::Agent {
        item,
        agents,
        index: 0,
    })
}

/// The states of the item's type, its current state selected.
fn states(workspace: &Workspace, item: ItemId) -> anyhow::Result<Choice> {
    let state = workspace.state();
    let policy = &state.policy().expect("a workforce has a policy").policy;
    let ItemView::Live(live) = state
        .item(item)
        .ok_or_else(|| anyhow!("#{} is gone", item.short()))?
    else {
        bail!("#{} is archived", item.short());
    };
    let config = policy
        .type_config(live.item_type())
        .ok_or_else(|| anyhow!("no item type `{}` in the policy", live.item_type()))?;
    let states: Vec<(StateName, StateCategory)> = config
        .states
        .iter()
        .map(|state| (state.name.clone(), state.state_category))
        .collect();
    let current = match live.standing(policy) {
        Standing::Known { state, .. } => Some(state),
        Standing::Unknown { .. } => None,
    };
    let index = states
        .iter()
        .position(|(name, _)| Some(name) == current)
        .unwrap_or_default();
    Ok(Choice::State {
        item,
        states,
        index,
    })
}

/// Carries out an effect that needs nothing more, and says what it did.
fn done(
    effect: Effect,
    workspace: &mut Workspace,
    terminal: &mut DefaultTerminal,
) -> anyhow::Result<String> {
    match effect {
        Effect::Quit | Effect::ChooseAgent { .. } | Effect::ChooseState { .. } => Ok(String::new()),
        Effect::StartSession { item, agent } => {
            let (file, _) = workspace::find_workforce(&std::env::current_dir()?)?;
            let repository = file
                .parent()
                .ok_or_else(|| anyhow!("the workforce file has no directory"))?
                .to_path_buf();
            let session = workspace.start_session(agent.clone(), item, &repository)?;
            Ok(format!(
                "started {agent} on #{} ({})",
                item.short(),
                session.short()
            ))
        }
        Effect::StopSession { session } => {
            workspace.stop_session(session)?;
            Ok(format!("stopped {}", session.short()))
        }
        Effect::Move { item, state } => {
            workspace.author(EventBody::ItemMoved(ItemMoved {
                item,
                to: state.clone(),
                by_session: None,
                because: None,
            }))?;
            Ok(format!("moved #{} to {state}", item.short()))
        }
        Effect::Comment { item, text } => {
            workspace.author(EventBody::Commented(Comment::plain(item, text)))?;
            Ok(format!("commented on #{}", item.short()))
        }
        Effect::Edit { item } => {
            let draft = {
                let ItemView::Live(live) = workspace
                    .state()
                    .item(item)
                    .ok_or_else(|| anyhow!("#{} is gone", item.short()))?
                else {
                    bail!("#{} is archived; unarchive it to edit", item.short());
                };
                Draft::of(live)
            };
            let markdown = draft.to_markdown()?;
            let edited = suspended(terminal, || edit_in_editor(&markdown, ".md"))??;
            let draft = Draft::from_markdown(&edited)?;
            let changes = match workspace.state().item(item) {
                Some(ItemView::Live(live)) => draft.changes(live),
                Some(ItemView::Archived(_)) | None => bail!("#{} is gone", item.short()),
            };
            if changes.is_empty() {
                return Ok("nothing changed".to_owned());
            }
            workspace.author(EventBody::ItemEdited(ItemEdited { item, changes }))?;
            Ok(format!("edited #{}", item.short()))
        }
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

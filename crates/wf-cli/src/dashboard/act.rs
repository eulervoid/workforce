//! Carrying out what the keys of the dashboard ask of the workforce: the same events the commands
//! write, without their output, which would draw over the dashboard.

use anyhow::{anyhow, bail};
use ratatui::DefaultTerminal;
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{EnterAlternateScreen, enable_raw_mode};
use wf_core::event::{Comment, CommentKind, EventBody, ItemEdited, ItemMoved, Review};
use wf_core::id::{CommentId, HashId, ItemId};
use wf_core::item::{ItemState, ItemView};
use wf_core::name::AgentName;
use wf_core::session::{CommitId, Verdict};
use wf_harness::Permission;

use super::app::{Choice, Effect, Starter, Written};
use super::diff::LineComment;
use crate::commands::{edit_in_editor, edit_until_valid, item_work, repository};
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
    /// A comment on lines written in `$EDITOR`, a draft of the review of `item` in place of the
    /// draft `replacing`.
    Drafted {
        item: ItemId,
        comment: LineComment,
        replacing: Option<usize>,
    },
    /// The first `posted` drafts of the review of `item` were posted, then it failed with `error`.
    ReviewFailed {
        item: ItemId,
        posted: usize,
        error: anyhow::Error,
    },
    /// The review of `item` was sent with the comments on lines drafted for it, as `what` says.
    Reviewed { item: ItemId, what: String },
}

/// Carries out `effect`. Quitting is up to the loop.
pub fn carry_out(
    effect: Effect,
    workspace: &mut Workspace,
    terminal: &mut DefaultTerminal,
) -> anyhow::Result<Outcome> {
    match effect {
        Effect::ChooseStart { item } => Ok(Outcome::Choose(starters(workspace, item)?)),
        Effect::ChooseState { item } => Ok(Outcome::Choose(states(workspace, item)?)),
        Effect::Approve {
            item,
            reviewed,
            comments,
        } => {
            let commit = submitted(workspace, item, reviewed)?;
            let sent = send_review(workspace, item, commit, &comments, &[], Verdict::Approve);
            reviewed_or_failed(sent, item, || {
                format!("approved #{}{}", item.short(), with(&comments))
            })
        }
        Effect::RequestChanges {
            item,
            reviewed,
            comments,
        } => request_changes(workspace, terminal, item, reviewed, &comments),
        Effect::LineCommentInEditor {
            item,
            mut comment,
            then,
        } => {
            let quote: String = comment
                .lines
                .quote
                .lines()
                .map(|line| format!("    {line}\n"))
                .collect();
            let template = format!(
                "{}\n{SCISSORS}\nYour {} on {} of #{}:\n\n{quote}",
                comment.text,
                comment.kind.name().replace('_', " "),
                comment.lines.location(),
                item.short()
            );
            let text = suspended(terminal, || edit_in_editor(&template, ".md"))??;
            comment.text = above_scissors(&text);
            if comment.text.is_empty() {
                return Ok(Outcome::Did("nothing written".to_owned()));
            }
            match then {
                Written::Post => post_line_comment(workspace, item, comment).map(Outcome::Did),
                Written::Draft { replacing } => Ok(Outcome::Drafted {
                    item,
                    comment,
                    replacing,
                }),
            }
        }
        Effect::PostLineComment { item, comment } => {
            post_line_comment(workspace, item, comment).map(Outcome::Did)
        }
        effect => done(effect, workspace, terminal).map(Outcome::Did),
    }
}

/// The agents of the principal of this machine, the policy's default agent first. A draft can be
/// released for the dispatcher too, which comes first. An item that a live session holds is
/// refused right away, before a starter is picked for nothing.
fn starters(workspace: &Workspace, item: ItemId) -> anyhow::Result<Choice> {
    let state = workspace.state();
    wf_daemon::ensure_unclaimed(state, item)?;
    let policy = &state.policy().expect("a workforce has a policy").policy;
    let draft = matches!(
        state.item(item),
        Some(ItemView::Live(live)) if live.state(policy) == ItemState::Draft
    );
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
    if agents.is_empty() && !draft {
        bail!("you have no agents; add one with `wf agent add`");
    }
    let starters = draft
        .then_some(Starter::Dispatcher)
        .into_iter()
        .chain(agents.into_iter().map(Starter::Agent))
        .collect();
    Ok(Choice::Start {
        item,
        starters,
        index: 0,
    })
}

/// The states a principal moves an item to, that its type uses, less the one it is in. `active`
/// and `review` are the runner's.
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
    let current = live.state(policy);
    let states: Vec<ItemState> = [
        ItemState::Draft,
        ItemState::Open,
        ItemState::Done,
        ItemState::Cancelled,
    ]
    .into_iter()
    .filter(|state| config.uses(*state) && *state != current)
    .collect();
    if states.is_empty() {
        bail!("a {} that is {current} cannot be moved", live.item_type());
    }
    Ok(Choice::State {
        item,
        states,
        index: 0,
    })
}

/// Carries out an effect that needs nothing more, and says what it did.
fn done(
    effect: Effect,
    workspace: &mut Workspace,
    terminal: &mut DefaultTerminal,
) -> anyhow::Result<String> {
    match effect {
        // Carried out by `carry_out`.
        Effect::Quit
        | Effect::ChooseStart { .. }
        | Effect::ChooseState { .. }
        | Effect::Approve { .. }
        | Effect::RequestChanges { .. }
        | Effect::LineCommentInEditor { .. }
        | Effect::PostLineComment { .. } => Ok(String::new()),
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
                to: state.into(),
                by_session: None,
                because: None,
            }))?;
            Ok(format!("#{} is {state}", item.short()))
        }
        Effect::Comment { item, text } => comment(workspace, item, text),
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
            let draft = suspended(terminal, || {
                edit_until_valid(&markdown, ".md", Draft::from_markdown)
            })??;
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
        Effect::Permit { request, answer } => {
            let held = workspace.answer_permission(request.session, request.since_ms, answer)?;
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
        Effect::CommentInEditor { item, text } => {
            let title = workspace
                .state()
                .item(item)
                .map(|view| format!(" \"{}\"", view.title()))
                .unwrap_or_default();
            let template = format!(
                "{text}\n{SCISSORS}\nYour comment on #{}{title}\n",
                item.short()
            );
            let text = suspended(terminal, || edit_in_editor(&template, ".md"))??;
            let text = above_scissors(&text);
            if text.is_empty() {
                return Ok("nothing written, no comment posted".to_owned());
            }
            comment(workspace, item, text)
        }
        Effect::ResolveInEditor {
            item,
            blocker,
            text,
        } => {
            let question = blocker_text(workspace, blocker);
            let template = format!(
                "{text}\n{SCISSORS}\nYour answer to the blocker {} on #{}:\n\n{question}\n",
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

        Effect::PageDiff { item } => {
            let git = repository()?;
            let work = item_work(workspace, &git, item)?;
            // The pager stays open on a short diff too, so quitting it goes back to the dashboard.
            suspended(terminal, || {
                wf_vcs::VcsAdapter::show_diff(&git, &work, &[], wf_vcs::Pager::StaysOpen)
            })??;
            Ok(String::new())
        }
    }
}

/// Requests changes on the work of `item`, with the comments on lines drafted for its review.
/// Without a change request among them, the change requests are written in `$EDITOR` first, one
/// per line.
fn request_changes(
    workspace: &mut Workspace,
    terminal: &mut DefaultTerminal,
    item: ItemId,
    reviewed: Option<CommitId>,
    comments: &[LineComment],
) -> anyhow::Result<Outcome> {
    let commit = submitted(workspace, item, reviewed)?;
    let on_lines = comments
        .iter()
        .filter(|comment| comment.kind == CommentKind::ChangeRequest)
        .count();
    let mut changes = Vec::new();
    if on_lines == 0 {
        let template = format!(
            "\n{SCISSORS}\nThe changes the work of #{} needs, one per line.\n",
            item.short()
        );
        let text = suspended(terminal, || edit_in_editor(&template, ".md"))??;
        changes = above_scissors(&text)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect();
        if changes.is_empty() {
            return Ok(Outcome::Did(
                "no changes written, the review still waits".to_owned(),
            ));
        }
    }
    let sent = send_review(
        workspace,
        item,
        commit,
        comments,
        &changes,
        Verdict::RequestChanges,
    );
    let count = changes.len() + on_lines;
    let plain = comments.len() - on_lines;
    reviewed_or_failed(sent, item, || {
        let also = if plain > 0 {
            format!(" and {plain} comment{}", plural(plain))
        } else {
            String::new()
        };
        format!(
            "requested {count} change{} on #{}{also}",
            plural(count),
            item.short()
        )
    })
}

/// A review that failed after `posted` of its drafts were posted.
struct Unsent {
    posted: usize,
    error: anyhow::Error,
}

fn reviewed_or_failed(
    sent: Result<(), Unsent>,
    item: ItemId,
    what: impl FnOnce() -> String,
) -> anyhow::Result<Outcome> {
    match sent {
        Ok(()) => Ok(Outcome::Reviewed { item, what: what() }),
        Err(Unsent { posted: 0, error }) => Err(error),
        Err(Unsent { posted, error }) => Ok(Outcome::ReviewFailed {
            item,
            posted,
            error,
        }),
    }
}

/// Posts the drafted comments on lines and the `changes` as change requests, then gives the
/// verdict on `commit`.
fn send_review(
    workspace: &mut Workspace,
    item: ItemId,
    commit: Option<CommitId>,
    comments: &[LineComment],
    changes: &[String],
    verdict: Verdict,
) -> Result<(), Unsent> {
    let mut posted = 0;
    let mut send = || -> anyhow::Result<()> {
        for comment in comments {
            post_line_comment(workspace, item, comment.clone())?;
            posted += 1;
        }
        for change in changes {
            workspace.author(EventBody::Commented(Comment {
                kind: CommentKind::ChangeRequest,
                ..Comment::plain(item, change.as_str())
            }))?;
        }
        workspace.author(EventBody::Reviewed(Review {
            item,
            commit,
            verdict,
            because: None,
        }))?;
        Ok(())
    };
    send().map_err(|error| Unsent { posted, error })
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// ` with 2 comments on lines`, or nothing for none.
fn with(comments: &[LineComment]) -> String {
    match comments.len() {
        0 => String::new(),
        count => format!(" with {count} comment{} on lines", plural(count)),
    }
}

fn post_line_comment(
    workspace: &mut Workspace,
    item: ItemId,
    comment: LineComment,
) -> anyhow::Result<String> {
    let location = comment.lines.location();
    let kind = comment.kind;
    workspace.author(EventBody::Commented(Comment {
        kind,
        lines: Some(comment.lines),
        ..Comment::plain(item, comment.text)
    }))?;
    Ok(format!(
        "wrote a {} on {location} of #{}",
        kind.name().replace('_', " "),
        item.short()
    ))
}

fn comment(workspace: &mut Workspace, item: ItemId, text: String) -> anyhow::Result<String> {
    workspace.author(EventBody::Commented(Comment::plain(item, text)))?;
    Ok(format!("commented on #{}", item.short()))
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

/// The commit the work of `item` was submitted as, if it is what the principal `reviewed`.
fn submitted(
    workspace: &Workspace,
    item: ItemId,
    reviewed: Option<CommitId>,
) -> anyhow::Result<Option<CommitId>> {
    let (commit, _) = workspace
        .state()
        .submission(item)
        .ok_or_else(|| anyhow!("#{} has no submitted work to review", item.short()))?;
    if commit != reviewed {
        bail!(
            "#{} was submitted again or its branch moved since you looked; look at the diff again",
            item.short()
        );
    }
    Ok(commit)
}

/// The text of a blocker, quoted, for reading it while answering.
fn blocker_text(workspace: &Workspace, blocker: CommentId) -> String {
    let text = match workspace.event(&blocker.into()) {
        Ok(Some(event)) => {
            // A blocker is a comment; anything else has no text to quote.
            if let EventBody::Commented(comment) = event.content.body {
                comment.text
            } else {
                String::new()
            }
        }
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
    super::stop_reports()?;
    ratatui::restore();
    let result = run();
    enable_raw_mode()?;
    execute!(std::io::stdout(), EnterAlternateScreen)?;
    super::start_reports()?;
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

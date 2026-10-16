//! `wf diff`, `wf checkout` and `wf path`: the work of an item in git.

use anyhow::{anyhow, bail};
use serde::Serialize;
use wf_core::id::{HashId, ItemId};
use wf_core::review::ReviewStatus;

use super::emit;
use super::item::{Scope, choose_item};
use crate::interactive::Mode;
use crate::workspace::{self, Workspace};

#[derive(Serialize)]
struct DiffView {
    item: String,
    commit: String,
    /// The merge that brought the work into the target, whose diff it is once there is one.
    merge: Option<String>,
    /// What the work changes; `None` while the branch has no commits ahead of the target, empty
    /// when its commits change nothing.
    diff: Option<String>,
}

const NO_COMMITS: &str = "the agent has not made any commits";

/// The repository of the workforce here: the directory of its `.workforce.toml`.
pub(crate) fn repository() -> anyhow::Result<wf_vcs::GitCli> {
    let (file, _) = workspace::find_workforce(&std::env::current_dir()?)?;
    let repository = file
        .parent()
        .ok_or_else(|| anyhow!("the workforce file has no directory"))?;
    Ok(wf_vcs::GitCli::open(repository)?)
}

/// The work on an item: once merged, the merge that brought it into the target; before, the head
/// of its branch, or after the branch is gone, what was submitted.
pub(crate) fn item_work(
    workspace: &Workspace,
    git: &wf_vcs::GitCli,
    id: ItemId,
) -> anyhow::Result<wf_vcs::Work> {
    let submission = workspace.state().submission(id);
    if let (Some(ReviewStatus::Merged { commit: merge }), Some((Some(commit), _))) =
        (workspace.state().review_status(id), submission)
    {
        return Ok(wf_vcs::Work::Merged { merge, commit });
    }
    let unmerged = |commit| wf_vcs::Work::Unmerged {
        target: target(workspace),
        commit,
    };
    match wf_vcs::VcsAdapter::branch_head(git, &wf_vcs::item_branch(id)) {
        Ok(commit) => return Ok(unmerged(commit)),
        Err(wf_vcs::VcsError::NoBranch(_)) => {}
        Err(error) => return Err(error.into()),
    }
    match submission {
        Some((Some(commit), _)) => Ok(unmerged(commit)),
        Some((None, _)) => bail!(
            "#{} was submitted without commits, there is no work to show",
            id.short()
        ),
        None => bail!("#{} has no work yet", id.short()),
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

pub(super) fn diff(
    json: bool,
    mode: Mode,
    workspace: &Workspace,
    id: ItemId,
    git_args: &[String],
) -> anyhow::Result<()> {
    let git = repository()?;
    let work = item_work(workspace, &git, id)?;
    let has_commits = wf_vcs::VcsAdapter::has_commits(&git, &work)?;
    match mode {
        Mode::Interactive if has_commits => Ok(wf_vcs::VcsAdapter::show_diff(
            &git,
            &work,
            git_args,
            wf_vcs::Pager::AsConfigured,
        )?),
        Mode::Interactive | Mode::Scripted => {
            let view = DiffView {
                item: id.to_base32(),
                commit: work.commit().to_string(),
                merge: match &work {
                    wf_vcs::Work::Unmerged { .. } => None,
                    wf_vcs::Work::Merged { merge, .. } => Some(merge.to_string()),
                },
                diff: if has_commits {
                    Some(wf_vcs::VcsAdapter::diff(&git, &work, git_args)?)
                } else {
                    None
                },
            };
            emit(json, &view, |view| match &view.diff {
                Some(diff) => diff.clone(),
                None => format!("{NO_COMMITS}\n"),
            })
        }
    }
}

#[derive(Serialize)]
struct CheckoutView {
    /// The item checked out, or `None` after going back.
    item: Option<String>,
    commit: Option<String>,
}

pub(super) fn checkout(
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
    let commit = item_work(workspace, &git, id)?.commit();
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

pub(super) fn path(
    json: bool,
    mode: Mode,
    workspace: &Workspace,
    item: Option<String>,
) -> anyhow::Result<()> {
    let id = choose_item(workspace, mode, item, Scope::Live)?;
    let path = wf_vcs::item_worktree(&workspace.data.workforce(&workspace.workforce()), id);
    if !path.is_dir() {
        let merged = match workspace.state().review_status(id) {
            Some(ReviewStatus::Merged { commit }) => {
                format!("; its work was merged as {}", &commit.to_string()[..8])
            }
            Some(
                ReviewStatus::Awaiting { .. }
                | ReviewStatus::ChangesRequested
                | ReviewStatus::Approved { .. },
            )
            | None => String::new(),
        };
        bail!("#{} has no worktree on this machine{merged}", id.short());
    }
    let view = PathView {
        item: id.to_base32(),
        path: path.display().to_string(),
    };
    emit(json, &view, |view| format!("{}\n", view.path))
}

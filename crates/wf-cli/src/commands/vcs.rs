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
    diff: String,
}

/// The repository of the workforce here: the directory of its `.workforce.toml`.
pub(crate) fn repository() -> anyhow::Result<wf_vcs::GitCli> {
    let (file, _) = workspace::find_workforce(&std::env::current_dir()?)?;
    let repository = file
        .parent()
        .ok_or_else(|| anyhow!("the workforce file has no directory"))?;
    Ok(wf_vcs::GitCli::open(repository)?)
}

/// The latest commit of the work on an item: the head of its branch, or after the branch is
/// gone, what was submitted. A merged item says where its work went.
pub(crate) fn work_commit(
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

pub(crate) fn target(workspace: &Workspace) -> String {
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

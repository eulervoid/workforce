//! `wf diff`, `wf checkout` and `wf path`: the work of an item in git.

use anyhow::bail;
use serde::Serialize;
use wf_core::id::{HashId, ItemId};
use wf_core::identity::DeviceKey;
use wf_core::review::ReviewStatus;
use wf_core::session::CommitId;
use wf_daemon::proto::LiveDiff;
use wf_net::DiffAnswer;

use super::emit;
use super::item::{Scope, choose_item};
use crate::interactive::Mode;
use crate::workspace::Workspace;

#[derive(Serialize)]
struct DiffView {
    item: String,
    /// The device the diff was read from, when the session of the item runs on another one.
    device: Option<String>,
    commit: String,
    /// The merge that brought the work into the target, whose diff it is once there is one.
    merge: Option<String>,
    /// What the work changes; `None` while the branch has no commits ahead of the target, empty
    /// when its commits change nothing.
    diff: Option<String>,
}

const NO_COMMITS: &str = "the agent has not made any commits";

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
    let target = target(workspace);
    if let Some(work) = git.branch_work(id, &target)? {
        return Ok(work);
    }
    match submission {
        Some((Some(commit), _)) => Ok(wf_vcs::Work::Unmerged { target, commit }),
        Some((None, _)) => bail!(
            "#{} was submitted without commits, there is no work to show",
            id.short()
        ),
        None => bail!("#{} has no work yet", id.short()),
    }
}

/// What the work on an item changes, and where it was read.
pub(crate) enum ItemDiff {
    /// Read from the branch in this repository.
    Local(wf_vcs::Work),
    /// Read from the device of the live session of the item.
    Device { device: DeviceKey, diff: DeviceDiff },
}

/// What the device of the session of an item came to when it was asked.
pub(crate) enum DeviceDiff {
    Work(wf_net::Work),
    /// The head is still the one the caller last had.
    Unchanged,
    /// The device could not be asked.
    Unreachable(String),
}

/// The changes of `id`: those of its live session when it runs on another device, asked of that
/// device with `since`, the head of the answer the caller last had; otherwise [`item_work`].
pub(crate) fn item_diff(
    workspace: &Workspace,
    git: &wf_vcs::GitCli,
    id: ItemId,
    since: Option<CommitId>,
) -> anyhow::Result<ItemDiff> {
    let session_device = workspace
        .state()
        .claim(id)
        .map(|session| session.device())
        .filter(|device| *device != workspace.device_key());
    let merged = matches!(
        workspace.state().review_status(id),
        Some(ReviewStatus::Merged { .. })
    );
    if let (Some(device), false) = (session_device, merged)
        && let Some(live) = workspace.live_diff(id, since)?
    {
        match live {
            LiveDiff::Answer(DiffAnswer::Work(work)) => {
                return Ok(ItemDiff::Device {
                    device,
                    diff: DeviceDiff::Work(work),
                });
            }
            LiveDiff::Answer(DiffAnswer::Unchanged) => {
                return Ok(ItemDiff::Device {
                    device,
                    diff: DeviceDiff::Unchanged,
                });
            }
            LiveDiff::Unreachable { device, reason } => {
                return Ok(ItemDiff::Device {
                    device,
                    diff: DeviceDiff::Unreachable(reason),
                });
            }
            LiveDiff::Answer(DiffAnswer::NotHere) => bail!(
                "the device of the session of #{}, {}, has no branch or worktree of it",
                id.short(),
                device.fmt_short()
            ),
            LiveDiff::Answer(DiffAnswer::TooLarge) => bail!(
                "the work of #{} on {} is too large to show from there",
                id.short(),
                device.fmt_short()
            ),
            // The session ended since the state was read.
            LiveDiff::NoSession => {}
        }
    }
    Ok(ItemDiff::Local(item_work(workspace, git, id)?))
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
    let git = workspace.repository()?;
    let work = match item_diff(workspace, &git, id, None)? {
        ItemDiff::Local(work) => work,
        ItemDiff::Device { device, diff } => {
            return device_diff(json, workspace, id, device, diff);
        }
    };
    let has_commits = git.has_commits(&work)?;
    match mode {
        Mode::Interactive if has_commits => {
            Ok(git.show_diff(&work, git_args, wf_vcs::Pager::AsConfigured)?)
        }
        Mode::Interactive | Mode::Scripted => {
            let view = DiffView {
                item: id.to_base32(),
                device: None,
                commit: work.commit().to_string(),
                merge: match &work {
                    wf_vcs::Work::Unmerged { .. } => None,
                    wf_vcs::Work::Merged { merge, .. } => Some(merge.to_string()),
                },
                diff: if has_commits {
                    Some(git.diff(&work, git_args)?)
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

/// Prints the diff read from the device of the session, saying which device it came from. Git's
/// own options and pager do not apply to a diff that was read elsewhere.
fn device_diff(
    json: bool,
    workspace: &Workspace,
    id: ItemId,
    device: DeviceKey,
    diff: DeviceDiff,
) -> anyhow::Result<()> {
    let label = crate::view::device_label(workspace.state(), device);
    match diff {
        DeviceDiff::Work(work) => {
            let view = DiffView {
                item: id.to_base32(),
                device: Some(device.to_string()),
                commit: work.head.to_string(),
                merge: None,
                diff: Some(work.committed),
            };
            emit(json, &view, |view| {
                let diff = view.diff.as_deref().unwrap_or_default();
                format!("committed on {label}, against {}\n{diff}", work.target)
            })
        }
        DeviceDiff::Unreachable(reason) => {
            bail!(
                "the session of #{} runs on {label}, which cannot be reached: {reason}",
                id.short()
            )
        }
        DeviceDiff::Unchanged => bail!("{label} answered that nothing changed, which was not asked"),
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
    let git = workspace.repository()?;
    if item.as_deref() == Some("-") {
        git.checkout_previous()?;
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
    git.checkout_detached(commit)?;
    let view = CheckoutView {
        item: Some(id.to_base32()),
        commit: Some(commit.to_string()),
    };
    emit(json, &view, |view| {
        format!(
            "checked out the work of #{} at {}, detached; `wf item checkout -` goes back\n",
            id.short(),
            wf_core::session::short_commit(view.commit.as_deref().unwrap_or_default())
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
                format!("; its work was merged as {}", commit.short())
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

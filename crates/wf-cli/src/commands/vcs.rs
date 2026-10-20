//! `wf diff`, `wf checkout` and `wf path`: the work of an item in git.

use anyhow::bail;
use serde::Serialize;
use wf_core::id::{HashId, ItemId};
use wf_core::identity::DeviceKey;
use wf_core::review::ReviewStatus;
use wf_core::session::{CommitId, SessionRole};
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
    /// What the commits of the work change; `None` while the branch has no commits ahead of the
    /// target, empty when its commits change nothing.
    committed: Option<String>,
}

const NO_COMMITS: &str = "the agent has not made any commits";

/// The text of a diff: what the commits change, or that there are none.
fn diff_text(committed: Option<&str>) -> String {
    match committed {
        Some(diff) => diff.to_owned(),
        None => format!("{NO_COMMITS}\n"),
    }
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
        .holder(id, &SessionRole::Work)
        .map(|session| session.device())
        .filter(|device| *device != workspace.device_key());
    let merged = matches!(
        workspace.state().review_status(id),
        Some(ReviewStatus::Merged { .. })
    );
    if let (Some(device), false) = (session_device, merged)
        && let Some(live) = workspace.live_diff(id, since)?
        && let Some(diff) = device_answer(id, device, live)?
    {
        return Ok(diff);
    }
    Ok(ItemDiff::Local(item_work(workspace, git, id)?))
}

/// What the daemon's answer for the session of `id` on `device` shows; `None` when the session
/// is over, so the work is read here.
fn device_answer(
    id: ItemId,
    device: DeviceKey,
    live: LiveDiff,
) -> anyhow::Result<Option<ItemDiff>> {
    let shown = |device, diff| Ok(Some(ItemDiff::Device { device, diff }));
    match live {
        LiveDiff::Answer(DiffAnswer::Work(work)) => shown(device, DeviceDiff::Work(work)),
        LiveDiff::Answer(DiffAnswer::Unchanged) => shown(device, DeviceDiff::Unchanged),
        LiveDiff::Unreachable { device, reason } => shown(device, DeviceDiff::Unreachable(reason)),
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
        LiveDiff::NoSession => Ok(None),
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
            git.show_diff(&work, git_args, wf_vcs::Pager::AsConfigured)?;
            Ok(())
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
                committed: if has_commits {
                    Some(git.diff(&work, git_args)?)
                } else {
                    None
                },
            };
            emit(json, &view, |view| diff_text(view.committed.as_deref()))
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
    let (view, text) = device_view(id, device, &label, diff)?;
    emit(json, &view, |_| text)
}

/// What `wf item diff` shows of the diff read from `device`, called `label`: the data for
/// `--json` and the text.
fn device_view(
    id: ItemId,
    device: DeviceKey,
    label: &str,
    diff: DeviceDiff,
) -> anyhow::Result<(DiffView, String)> {
    match diff {
        DeviceDiff::Work(work) => {
            let text = format!(
                "committed on {label}, against {}\n{}",
                work.target, work.committed
            );
            let view = DiffView {
                item: id.to_base32(),
                device: Some(device.to_string()),
                commit: work.head.to_string(),
                merge: None,
                committed: Some(work.committed),
            };
            Ok((view, text))
        }
        DeviceDiff::Unreachable(reason) => {
            bail!(
                "the session of #{} runs on {label}, which cannot be reached: {reason}",
                id.short()
            )
        }
        DeviceDiff::Unchanged => {
            bail!("{label} answered that nothing changed, which was not asked")
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
    let checkout = workspace.checkout()?;
    if item.as_deref() == Some("-") {
        checkout.checkout_previous()?;
        let view = CheckoutView {
            item: None,
            commit: None,
        };
        return emit(json, &view, |_| {
            "back where the checkout was before\n".to_string()
        });
    }
    let id = choose_item(workspace, mode, item, Scope::Live)?;
    let commit = item_work(workspace, &workspace.repository()?, id)?.commit();
    checkout.checkout_detached(commit)?;
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
    let path = wf_vcs::item_repository(&workspace.data.workforce(&workspace.workforce()), id);
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
        bail!("#{} has no repository on this machine{merged}", id.short());
    }
    let view = PathView {
        item: id.to_base32(),
        path: path.display().to_string(),
    };
    emit(json, &view, |view| format!("{}\n", view.path))
}

#[cfg(test)]
mod tests {
    use wf_core::identity::DeviceSigner;
    use wf_daemon::proto::LiveDiff;
    use wf_net::{DiffAnswer, Work};

    use super::*;

    fn device() -> DeviceKey {
        DeviceSigner::generate([1; 32]).key()
    }

    fn commit() -> CommitId {
        CommitId::Git("a8d4948400000000000000000000000000000000".parse().unwrap())
    }

    #[test]
    fn the_diff_of_a_device_is_shown_with_the_device_and_its_commit() {
        let id = ItemId::from_bytes([2; 32]);
        let work = Work {
            target: "main".to_owned(),
            head: commit(),
            committed: "diff --git a/f b/f\n".to_owned(),
        };
        let (view, text) = device_view(id, device(), "laptop", DeviceDiff::Work(work)).unwrap();
        assert_eq!(view.device, Some(device().to_string()));
        assert_eq!(view.commit, commit().to_string());
        assert_eq!(view.merge, None);
        assert_eq!(view.committed.as_deref(), Some("diff --git a/f b/f\n"));
        assert_eq!(
            text,
            "committed on laptop, against main\ndiff --git a/f b/f\n"
        );
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["device"], device().to_string());
        assert_eq!(json["committed"], "diff --git a/f b/f\n");
    }

    #[test]
    fn an_unreachable_device_is_an_error_naming_it_not_a_missing_branch() {
        let id = ItemId::from_bytes([2; 32]);
        let error = device_view(
            id,
            device(),
            "laptop",
            DeviceDiff::Unreachable("timed out".to_owned()),
        )
        .map(|_| ())
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("runs on laptop, which cannot be reached: timed out"),
            "{error}"
        );
        assert!(!error.contains("missing"), "{error}");
    }

    #[test]
    fn the_answer_of_the_device_of_the_session_is_the_diff_shown() {
        let id = ItemId::from_bytes([2; 32]);
        let work = Work {
            target: "main".to_owned(),
            head: commit(),
            committed: "diff --git a/f b/f\n".to_owned(),
        };
        let shown = device_answer(
            id,
            device(),
            LiveDiff::Answer(DiffAnswer::Work(work.clone())),
        )
        .unwrap();
        let Some(ItemDiff::Device {
            device: from,
            diff: DeviceDiff::Work(read),
        }) = shown
        else {
            panic!("expected the work of the device");
        };
        assert_eq!(from, device());
        assert_eq!(read, work);
    }

    #[test]
    fn an_unreachable_device_is_not_a_missing_branch() {
        let id = ItemId::from_bytes([2; 32]);
        let live = LiveDiff::Unreachable {
            device: device(),
            reason: "timed out".to_owned(),
        };
        let Some(ItemDiff::Device {
            diff: DeviceDiff::Unreachable(reason),
            ..
        }) = device_answer(id, device(), live).unwrap()
        else {
            panic!("expected an unreachable device");
        };
        assert_eq!(reason, "timed out");
    }

    #[test]
    fn a_device_without_the_branch_or_without_a_session_is_not_an_unreachable_one() {
        let id = ItemId::from_bytes([2; 32]);
        let error = device_answer(id, device(), LiveDiff::Answer(DiffAnswer::NotHere)).err();
        assert!(
            error
                .unwrap()
                .to_string()
                .contains("has no branch or worktree")
        );
        assert!(
            device_answer(id, device(), LiveDiff::NoSession)
                .unwrap()
                .is_none()
        );
    }
}

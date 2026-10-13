//! The version control the work happens in: one branch and worktree per item, and the merge of
//! reviewed work into the target branch. Git is the only system, called as the `git` CLI.

mod git;

use std::path::{Path, PathBuf};

use wf_core::id::{HashId, ItemId};
use wf_core::session::CommitId;

pub use git::GitCli;

/// How much of an item's id names its branch and worktree.
const NAME_LEN: usize = 12;

/// The name of the branch that holds the work of an item. Sessions continue it.
pub fn item_branch(item: ItemId) -> String {
    format!("wf/{}", item_name(item))
}

/// The name of the directory of the worktree of an item.
pub fn item_name(item: ItemId) -> String {
    item.to_base32()[..NAME_LEN].to_string()
}

/// The directory of a workforce that the sessions on this machine run in: the worktree of each
/// item, and the checkouts and empty directories of review, supervise and diagnose sessions.
pub fn worktrees(workforce_directory: &Path) -> PathBuf {
    workforce_directory.join("worktrees")
}

/// The worktree of the work on an item.
pub fn item_worktree(workforce_directory: &Path, item: ItemId) -> PathBuf {
    worktrees(workforce_directory).join(item_name(item))
}

#[derive(Debug, thiserror::Error)]
pub enum VcsError {
    #[error("cannot run git")]
    Spawn(#[source] std::io::Error),
    #[error("`git {command}` failed: {stderr}")]
    Git { command: String, stderr: String },
    #[error("git {found} is too old, wf needs 2.38 or later for `merge-tree --write-tree`")]
    TooOld { found: String },
    #[error("unexpected output from `git {command}`: {output}")]
    UnexpectedOutput { command: String, output: String },
    #[error("cannot set up or remove a checkout")]
    Io(#[from] std::io::Error),
    #[error("there is no branch `{0}`")]
    NoBranch(String),
    #[error("{} is not a git repository", .0.display())]
    NotARepository(PathBuf),
    #[error("{} has uncommitted changes; commit or stash them first", .0.display())]
    DirtyCheckout(PathBuf),
}

/// What happened when reviewed work was merged into the target branch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeOutcome {
    /// The target branch now points at the merge commit.
    Merged { commit: CommitId },
    /// The work does not merge cleanly. The paths are the files in conflict.
    Conflict { paths: Vec<String> },
    /// The target branch is checked out in a worktree with changes, which the merge never touches.
    DirtyCheckout { worktree: PathBuf },
    /// The target branch moved while the merge was being made. Nothing changed; try again.
    TargetMoved,
}

pub trait VcsAdapter {
    /// Makes `branch` (created at the tip of `target` when it does not exist yet) and checks it
    /// out in a worktree at `path`. Does nothing if that worktree is already there.
    fn prepare_worktree(&self, branch: &str, path: &Path, target: &str) -> Result<(), VcsError>;

    /// Checks `commit` out, detached, in a new worktree at `path`, for a reviewer.
    fn detached_worktree(&self, path: &Path, commit: CommitId) -> Result<(), VcsError>;

    /// Removes the worktree at `path` together with whatever is uncommitted in it.
    fn remove_worktree(&self, path: &Path) -> Result<(), VcsError>;

    fn delete_branch(&self, branch: &str) -> Result<(), VcsError>;

    /// The commit a branch points at.
    fn branch_head(&self, branch: &str) -> Result<CommitId, VcsError>;

    /// How many commits `commit` has that `target` does not.
    fn commits_ahead(&self, target: &str, commit: CommitId) -> Result<usize, VcsError>;

    /// What `commit` changes relative to where it left `target`: `git diff <target>...<commit>`,
    /// with `args` added, such as `--stat` or `-- src/`.
    fn diff(&self, target: &str, commit: CommitId, args: &[String]) -> Result<String, VcsError>;

    /// The same diff, written to the terminal by git itself, so its pager and diff settings
    /// apply as for `git diff`.
    fn show_diff(&self, target: &str, commit: CommitId, args: &[String]) -> Result<(), VcsError>;

    /// Checks `commit` out, detached, in the main checkout, which has to be clean.
    fn checkout_detached(&self, commit: CommitId) -> Result<(), VcsError>;

    /// Goes back to what the main checkout had before the last checkout: `git checkout -`.
    fn checkout_previous(&self) -> Result<(), VcsError>;

    /// Merges `commit` into `target` with a merge commit that has both as parents.
    fn merge(
        &self,
        target: &str,
        commit: CommitId,
        message: &str,
    ) -> Result<MergeOutcome, VcsError>;
}

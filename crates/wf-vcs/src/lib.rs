//! The version control the work happens in: one branch and worktree per item, and the merge of
//! reviewed work into the target branch. Git is the only system, called as the `git` CLI.

mod git;
pub mod signing;

use std::path::{Path, PathBuf};

use wf_core::id::{HashId, ItemId};
use wf_core::session::{CommitId, GitOid};

pub use git::GitCli;

/// Whether and how commits are signed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommitSigning {
    /// As the git config says (`commit.gpgsign`), as for the principal's own commits.
    AsConfigured,
    /// Never, whatever the git config says.
    Unsigned,
    /// With the SSH private key in this file, which must not need a passphrase, through
    /// `ssh-keygen`: never through an agent that might ask the principal first.
    Key(PathBuf),
}

impl CommitSigning {
    /// The git config that signs this way, on top of what the repository's says.
    fn config(&self) -> Vec<(&'static str, String)> {
        match self {
            CommitSigning::AsConfigured => Vec::new(),
            CommitSigning::Unsigned => vec![("commit.gpgsign", "false".into())],
            CommitSigning::Key(key) => vec![
                ("commit.gpgsign", "true".into()),
                ("gpg.format", "ssh".into()),
                ("user.signingkey", key.to_string_lossy().into_owned()),
                ("gpg.ssh.program", "ssh-keygen".into()),
            ],
        }
    }

    /// The environment that makes git in another process, such as an agent's, sign this way.
    pub fn environment(&self) -> Vec<(String, String)> {
        let config = self.config();
        if config.is_empty() {
            return Vec::new();
        }
        let mut environment = vec![("GIT_CONFIG_COUNT".to_string(), config.len().to_string())];
        for (index, (key, value)) in config.into_iter().enumerate() {
            environment.push((format!("GIT_CONFIG_KEY_{index}"), key.to_string()));
            environment.push((format!("GIT_CONFIG_VALUE_{index}"), value));
        }
        environment
    }
}

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
    DirtyCheckout(DirtyCheckout),
    /// The target branch moved while the merge was being made. Nothing changed; try again.
    TargetMoved,
}

/// What happened when the target branch was merged into an item's branch, in its worktree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TargetMerge {
    /// The branch had everything of the target already.
    UpToDate,
    /// The branch now has the target merged in.
    Merged,
    /// The merge conflicts in these files, relative to the worktree. The merge is left in the
    /// worktree, for whoever works there to resolve the conflicts and commit it.
    Conflict { paths: Vec<String> },
    /// A merge from earlier is still in the worktree with every conflict resolved; it only needs
    /// committing.
    Uncommitted,
}

/// A worktree with uncommitted changes to tracked files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirtyCheckout {
    pub worktree: PathBuf,
    /// The changed files, relative to the worktree.
    pub paths: Vec<String>,
}

impl DirtyCheckout {
    /// How many changed files [`DirtyCheckout::files`] names before it only counts the rest.
    const NAMED: usize = 10;

    /// The changed files to name to the principal, such as `a.txt, b.txt and 3 more`.
    pub fn files(&self) -> String {
        let named = self.paths[..self.paths.len().min(Self::NAMED)].join(", ");
        match self.paths.len().saturating_sub(Self::NAMED) {
            0 => named,
            more => format!("{named} and {more} more"),
        }
    }
}

pub trait VcsAdapter {
    /// Makes `branch` (created at the tip of `target` when it does not exist yet) and checks it
    /// out in a worktree at `path`. Does nothing if that worktree is already there.
    fn prepare_worktree(&self, branch: &str, path: &Path, target: &str) -> Result<(), VcsError>;

    /// Checks `commit` out, detached, in a new worktree at `path`, for a reviewer.
    fn detached_worktree(&self, path: &Path, commit: CommitId) -> Result<(), VcsError>;

    /// Makes the worktree at `path` hold the files of `tree`, a tree or a commit, and nothing
    /// else: tracked files are reset and untracked ones removed. Ignored files, such as build
    /// output, stay. A commit is checked out with HEAD detached at it. The worktree is made,
    /// detached, when it is not there yet.
    fn check_out_tree(&self, path: &Path, tree: GitOid) -> Result<(), VcsError>;

    /// Removes the worktree at `path` together with whatever is uncommitted in it.
    fn remove_worktree(&self, path: &Path) -> Result<(), VcsError>;

    /// Forgets the worktrees whose directories are gone.
    fn prune_worktrees(&self) -> Result<(), VcsError>;

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

    /// Merges `commit` into `target` with a merge commit that has both as parents, signed as
    /// `signing` says.
    fn merge(
        &self,
        target: &str,
        commit: CommitId,
        message: &str,
        signing: &CommitSigning,
    ) -> Result<MergeOutcome, VcsError>;

    /// Merges `target` into the branch checked out in `worktree`, with a commit signed as
    /// `signing` says. A merge left unfinished there is not made again: its conflicts are named.
    fn merge_target(
        &self,
        worktree: &Path,
        target: &str,
        message: &str,
        signing: &CommitSigning,
    ) -> Result<TargetMerge, VcsError>;
}

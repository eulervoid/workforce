//! The version control the work happens in: one branch and worktree per item, and the merge of
//! reviewed work into the target branch. Git is the only system, called as the `git` CLI.

pub mod diff;
mod git;
pub mod signing;

use std::path::{Path, PathBuf};

use wf_core::id::{HashId, ItemId, SessionId};
use wf_core::session::{CommitId, GitOid, SessionRole};

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

/// The worktree or directory `session` of `role` on `item` runs in. Work sessions share the
/// worktree of the item; every other session has a directory of its own, so that cleaning up
/// after one cannot remove what the next on the same item uses.
pub fn session_directory(
    workforce_directory: &Path,
    item: ItemId,
    session: SessionId,
    role: SessionRole,
) -> PathBuf {
    let purpose = match role {
        SessionRole::Work => return item_worktree(workforce_directory, item),
        SessionRole::Review { .. } => "review",
        SessionRole::Supervise => "supervise",
        SessionRole::Diagnose => "diagnose",
    };
    worktrees(workforce_directory).join(format!(
        "{}-{purpose}-{}",
        item_name(item),
        session.short()
    ))
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

/// The work on an item, as far as git can show it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Work {
    /// Not merged yet: `commit` changes the target from where it left it, `<target>...<commit>`.
    Unmerged { target: String, commit: CommitId },
    /// Merged as `merge`, whose first parent is the target: what it brought in is
    /// `<merge>^1 <merge>`, however far the target moved since. `commit` is the merged work.
    Merged { merge: CommitId, commit: CommitId },
}

impl Work {
    /// The latest commit of the work itself, the one to check out.
    pub fn commit(&self) -> CommitId {
        match self {
            Work::Unmerged { commit, .. } | Work::Merged { commit, .. } => *commit,
        }
    }

    /// The commit whose files the diff of the work shows on its new side.
    pub fn shown(&self) -> CommitId {
        match self {
            Work::Unmerged { commit, .. } => *commit,
            Work::Merged { merge, .. } => *merge,
        }
    }
}

/// How the pager of a diff git writes to the terminal behaves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pager {
    /// As the principal's git and `LESS` config say, which may quit at once on a short diff.
    AsConfigured,
    /// `less` stays open however short the diff is, so it is left only by quitting it.
    StaysOpen,
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

/// What merging work into the target branch makes, before anything is committed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeTree {
    /// The work is in the target already, brought in by `commit`.
    Landed { commit: CommitId },
    /// The work merges cleanly with the target at `base` into `tree`.
    Clean { base: CommitId, tree: GitOid },
    /// The work does not merge cleanly. The paths are the files in conflict.
    Conflict { paths: Vec<String> },
}

/// What happened when an item's branch was rebased onto the target branch, in its worktree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rebase {
    /// The branch had everything of the target already, and nothing was rewritten.
    UpToDate,
    /// The branch's own commits are now on top of the target.
    Rebased,
    /// The rebase stopped at a commit that conflicts in these files, relative to the worktree.
    /// It is left in the worktree for whoever works there to resolve the files; the next rebase
    /// stages them and goes on.
    Conflict { paths: Vec<String> },
    /// The worktree has uncommitted changes, which a rebase would have to carry along. Nothing
    /// was rewritten.
    DirtyCheckout(DirtyCheckout),
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

    /// Whether `work` has commits of its own: merged work has; unmerged work once its commit is
    /// ahead of the target, which a branch just started is not.
    fn has_commits(&self, work: &Work) -> Result<bool, VcsError> {
        match work {
            Work::Unmerged { target, commit } => Ok(self.commits_ahead(target, *commit)? > 0),
            Work::Merged { .. } => Ok(true),
        }
    }

    /// What `work` changes, as [`Work`] says, with `args` added to `git diff`, such as `--stat`
    /// or `-- src/`.
    fn diff(&self, work: &Work, args: &[String]) -> Result<String, VcsError>;

    /// The same diff as data: the files it changes, their hunks and their lines, whatever the
    /// git config says about how a diff looks.
    fn file_diffs(&self, work: &Work) -> Result<Vec<diff::FileDiff>, VcsError> {
        let args = diff::ARGS.map(String::from);
        Ok(diff::parse(&self.diff(work, &args)?))
    }

    /// The same diff, written to the terminal by git itself, so its pager and diff settings
    /// apply as for `git diff`, except where `pager` says otherwise.
    fn show_diff(&self, work: &Work, args: &[String], pager: Pager) -> Result<(), VcsError>;

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

    /// What merging `commit` into `target` makes, without committing it or moving `target`.
    fn merge_tree(&self, target: &str, commit: CommitId) -> Result<MergeTree, VcsError>;

    /// Commits `tree`, made by [`VcsAdapter::merge_tree`], as the merge of `commit` into `target`
    /// at `base`, signed as `signing` says, and moves `target` to it. Never a conflict: the
    /// outcome is the merge, a checkout of `target` with changes, or `target` no longer at `base`.
    fn commit_merge(
        &self,
        target: &str,
        base: CommitId,
        tree: GitOid,
        commit: CommitId,
        message: &str,
        signing: &CommitSigning,
    ) -> Result<MergeOutcome, VcsError>;

    /// The tree of `commit`.
    fn tree_of(&self, commit: CommitId) -> Result<GitOid, VcsError>;

    /// Rebases the branch checked out in `worktree` onto `target`, the rewritten commits signed as
    /// `signing` says and committed by `committer`. A rebase stopped there earlier is not begun
    /// again: the conflicting files are staged once they have no conflict markers left, and the
    /// rebase goes on.
    fn rebase(
        &self,
        worktree: &Path,
        target: &str,
        signing: &CommitSigning,
        committer: &str,
    ) -> Result<Rebase, VcsError>;

    /// The files in conflict when a rebase is stopped in `worktree`; `None` when none is.
    fn stopped_rebase(&self, worktree: &Path) -> Result<Option<Vec<String>>, VcsError>;

    /// Gives up the rebase stopped in `worktree`, if one is, which checks the branch out again
    /// as it was before the rebase.
    fn abort_rebase(&self, worktree: &Path) -> Result<(), VcsError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_other_than_work_have_directories_of_their_own() {
        let workforce = Path::new("/workforce");
        let item = ItemId::from_bytes([1; 32]);
        let (first, second) = (
            SessionId::from_bytes([2; 32]),
            SessionId::from_bytes([3; 32]),
        );
        let commit = Some(CommitId::Git(GitOid::from_bytes([4; 20])));
        for role in [
            SessionRole::Review { commit },
            SessionRole::Review { commit: None },
            SessionRole::Supervise,
            SessionRole::Diagnose,
        ] {
            let directory = session_directory(workforce, item, first, role);
            assert_ne!(directory, session_directory(workforce, item, second, role));
            assert_ne!(directory, item_worktree(workforce, item));
        }
        assert_eq!(
            session_directory(workforce, item, first, SessionRole::Work),
            session_directory(workforce, item, second, SessionRole::Work),
        );
    }
}

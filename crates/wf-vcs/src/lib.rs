//! The version control the work happens in: one branch per item, a repository of its own for the
//! agent that works on it, and the merge of reviewed work into the target branch. Git is the only
//! system, called as the `git` CLI.

mod agent_repository;
mod bare_repository;
pub mod config;
pub mod diff;
mod git;
mod item_refs;
pub mod signing;
mod target_standing;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
mod transfer;

use std::path::{Path, PathBuf};

use wf_core::id::{HashId, ItemId, SessionId};
use wf_core::session::{CommitId, GitOid, SessionRole};

pub use agent_repository::{agent_branch_head, registered_worktrees, remove_agent_repository};
pub use bare_repository::REMOTE;
pub use git::{BRIDGE_DESCRIPTOR, GitCli};
pub use item_refs::diverged_ref;
pub use target_standing::{TargetStanding, target_standing};
pub use transfer::Bridge;

/// The variables that give git config through the environment: how many, then each key and value.
const CONFIG_COUNT: &str = "GIT_CONFIG_COUNT";
const CONFIG_KEY: &str = "GIT_CONFIG_KEY_";
const CONFIG_VALUE: &str = "GIT_CONFIG_VALUE_";

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

    /// The `git` arguments that set that config: `-c key=value` for each.
    fn arguments(&self) -> Vec<String> {
        self.config()
            .into_iter()
            .flat_map(|(key, value)| ["-c".to_string(), format!("{key}={value}")])
            .collect()
    }

    /// The environment that makes git in another process, such as an agent's, sign this way.
    pub fn environment(&self) -> Vec<(String, String)> {
        let config = self.config();
        if config.is_empty() {
            return Vec::new();
        }
        let mut environment = vec![(CONFIG_COUNT.to_string(), config.len().to_string())];
        for (index, (key, value)) in config.into_iter().enumerate() {
            environment.push((format!("{CONFIG_KEY}{index}"), key.to_string()));
            environment.push((format!("{CONFIG_VALUE}{index}"), value));
        }
        environment
    }
}

/// How much of an item's id names its branch and repository.
const NAME_LEN: usize = 12;

/// What the names of the branches of items begin with.
const ITEM_BRANCH_PREFIX: &str = "wf/";

/// The name of the branch that holds the work of an item. Sessions continue it.
pub fn item_branch(item: ItemId) -> String {
    format!("{ITEM_BRANCH_PREFIX}{}", item_name(item))
}

/// The bare repository of a workforce on this machine, `git` in its directory: the target and the
/// branches of the items. Agent repositories take their objects from it, and the principal's
/// checkout has it as its remote.
pub fn bare_repository(workforce_directory: &Path) -> PathBuf {
    workforce_directory.join("git")
}

/// The name of the directory of the worktree of an item.
pub fn item_name(item: ItemId) -> String {
    item.to_base32()[..NAME_LEN].to_string()
}

/// The directory of a workforce that the sessions on this machine run in: the repository of each
/// item, and the repositories and empty directories of review, supervise and diagnose sessions.
pub fn repositories(workforce_directory: &Path) -> PathBuf {
    workforce_directory.join("repos")
}

/// The repository of the agent that works on an item. It is the agent's own, with the principal's
/// objects as alternates, and the daemon copies the commits it submits into the principal's.
pub fn item_repository(workforce_directory: &Path, item: ItemId) -> PathBuf {
    repositories(workforce_directory).join(item_name(item))
}

/// The repository or directory `session` of `role` on `item` runs in. Work sessions share the
/// repository of the item; every other session has a directory of its own, so that cleaning up
/// after one cannot remove what the next on the same item uses.
pub fn session_directory(
    workforce_directory: &Path,
    item: ItemId,
    session: SessionId,
    role: &SessionRole,
) -> PathBuf {
    let purpose = match role {
        SessionRole::Work => return item_repository(workforce_directory, item),
        SessionRole::Review { .. } => "review",
        SessionRole::Supervise => "supervise",
        SessionRole::Diagnose => "diagnose",
    };
    repositories(workforce_directory).join(format!(
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
    #[error(
        "{} has no remote `{}` to fetch the work from; add wf's repository as that remote",
        .0.display(),
        REMOTE
    )]
    NoRemote(PathBuf),
    #[error("the device does not have commit {}", .0.short())]
    Unserved(CommitId),
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

/// What the commits of an agent's repository have to be before they are copied into the
/// principal's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportMode {
    /// Whatever the branch holds: the work as it stands, for the principal to look at.
    InProgress,
    /// Commits that contain the target's tip, as a submission has them after its rebase.
    Submission,
}

/// What came of copying the commits of an agent's repository into the principal's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Import {
    /// The branch of the item in the principal's repository now points at `commit`.
    Imported { commit: CommitId },
    /// The agent's repository has no branch of the item with a commit on it.
    NoCommits,
    /// The branch lacks the tip of the target, which moved since the agent rebased. Nothing was
    /// copied.
    BehindTarget,
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

/// Whether a rebase is stopped in a worktree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoppedRebase {
    NotStopped,
    /// Stopped at a commit that conflicts in these files, relative to the worktree.
    Conflict {
        paths: Vec<String>,
    },
    /// Stopped, but not at a conflict.
    Stopped,
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
            SessionRole::Review { commit, step: None },
            SessionRole::Review {
                commit: None,
                step: None,
            },
            SessionRole::Supervise,
            SessionRole::Diagnose,
        ] {
            let directory = session_directory(workforce, item, first, &role);
            assert_ne!(directory, session_directory(workforce, item, second, &role));
            assert_ne!(directory, item_repository(workforce, item));
        }
        assert_eq!(
            session_directory(workforce, item, first, &SessionRole::Work),
            session_directory(workforce, item, second, &SessionRole::Work),
        );
    }
}

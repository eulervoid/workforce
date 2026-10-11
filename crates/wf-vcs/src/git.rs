use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use wf_core::session::{CommitId, GitOid};

use crate::{MergeOutcome, VcsAdapter, VcsError};

const MINIMUM_VERSION: (u32, u32) = (2, 38);

/// A git repository, driven through the `git` command.
#[derive(Clone, Debug)]
pub struct GitCli {
    repository: PathBuf,
}

/// What `git` said, for commands whose failure is an answer and not an error.
struct Finished {
    success: bool,
    stdout: String,
    stderr: String,
}

impl GitCli {
    /// Opens the repository that contains `directory`, after checking the git version.
    pub fn open(directory: &Path) -> Result<Self, VcsError> {
        let version = Self::run_in(directory, ["--version"])?;
        let text = version.stdout.trim();
        let found = text
            .strip_prefix("git version ")
            .and_then(|rest| rest.split_whitespace().next())
            .unwrap_or(text);
        let mut numbers = found.split('.').map(|part| part.parse::<u32>());
        let (Some(Ok(major)), Some(Ok(minor))) = (numbers.next(), numbers.next()) else {
            return Err(VcsError::UnexpectedOutput {
                command: "--version".into(),
                output: text.into(),
            });
        };
        if (major, minor) < MINIMUM_VERSION {
            return Err(VcsError::TooOld {
                found: found.into(),
            });
        }
        let top = Self::run_in(directory, ["rev-parse", "--show-toplevel"])?;
        if !top.success {
            return Err(VcsError::NotARepository(directory.to_path_buf()));
        }
        Ok(Self {
            repository: PathBuf::from(top.stdout.trim()),
        })
    }

    pub fn repository(&self) -> &Path {
        &self.repository
    }

    fn run_in<I, S>(directory: &Path, args: I) -> Result<Finished, VcsError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output: Output = Command::new("git")
            .args(args)
            .current_dir(directory)
            .env("LC_ALL", "C")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .map_err(VcsError::Spawn)?;
        Ok(Finished {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    /// Runs git in `directory` and returns its output, or an error if it fails.
    fn succeed_in<const N: usize>(
        &self,
        directory: &Path,
        args: [&OsStr; N],
    ) -> Result<String, VcsError> {
        let finished = Self::run_in(directory, args)?;
        if finished.success {
            Ok(finished.stdout)
        } else {
            Err(VcsError::Git {
                command: describe(&args),
                stderr: finished.stderr.trim().to_string(),
            })
        }
    }

    fn succeed<const N: usize>(&self, args: [&str; N]) -> Result<String, VcsError> {
        self.succeed_in(&self.repository, args.map(OsStr::new))
    }

    fn attempt<const N: usize>(&self, args: [&str; N]) -> Result<Finished, VcsError> {
        Self::run_in(&self.repository, args)
    }

    fn oid(&self, command: &str, text: &str) -> Result<CommitId, VcsError> {
        text.trim()
            .parse::<GitOid>()
            .map(CommitId::Git)
            .map_err(|_| VcsError::UnexpectedOutput {
                command: command.into(),
                output: text.trim().into(),
            })
    }

    fn branch_exists(&self, branch: &str) -> Result<bool, VcsError> {
        let reference = format!("refs/heads/{branch}");
        Ok(self
            .attempt(["rev-parse", "--verify", "--quiet", &reference])?
            .success)
    }

    /// The worktrees of the repository: where each is and which branch it has checked out.
    fn worktrees(&self) -> Result<Vec<Worktree>, VcsError> {
        let listing = self.succeed(["worktree", "list", "--porcelain"])?;
        let mut worktrees = Vec::new();
        for block in listing.split("\n\n") {
            let mut path = None;
            let mut branch = None;
            for line in block.lines() {
                if let Some(found) = line.strip_prefix("worktree ") {
                    path = Some(PathBuf::from(found));
                } else if let Some(found) = line.strip_prefix("branch ") {
                    branch = Some(found.to_string());
                }
            }
            if let Some(path) = path {
                worktrees.push(Worktree { path, branch });
            }
        }
        Ok(worktrees)
    }

    fn is_clean(&self, worktree: &Path) -> Result<bool, VcsError> {
        let status =
            self.succeed_in(worktree, [OsStr::new("status"), OsStr::new("--porcelain")])?;
        Ok(status.trim().is_empty())
    }

    /// Moves `target` from `old` to `new`, if nothing moved it in the meantime. Where the target
    /// is checked out it must be clean and is fast-forwarded there, so the files follow.
    fn advance(&self, target: &str, old: &CommitId, new: &CommitId) -> Result<Advance, VcsError> {
        let reference = format!("refs/heads/{target}");
        let checkout = self
            .worktrees()?
            .into_iter()
            .find(|worktree| worktree.branch.as_deref() == Some(reference.as_str()));
        let moved =
            |this: &Self| -> Result<bool, VcsError> { Ok(this.rev_parse(&reference)? != *old) };
        match checkout {
            Some(worktree) => {
                if !self.is_clean(&worktree.path)? {
                    return Ok(Advance::Dirty(worktree.path));
                }
                let finished = Self::run_in(
                    &worktree.path,
                    ["merge", "--ff-only", "--quiet", &new.to_string()],
                )?;
                if finished.success {
                    Ok(Advance::Done)
                } else if moved(self)? {
                    Ok(Advance::Moved)
                } else {
                    Err(VcsError::Git {
                        command: format!("merge --ff-only {new}"),
                        stderr: finished.stderr.trim().to_string(),
                    })
                }
            }
            None => {
                let finished =
                    self.attempt(["update-ref", &reference, &new.to_string(), &old.to_string()])?;
                if finished.success {
                    Ok(Advance::Done)
                } else if moved(self)? {
                    Ok(Advance::Moved)
                } else {
                    Err(VcsError::Git {
                        command: format!("update-ref {reference}"),
                        stderr: finished.stderr.trim().to_string(),
                    })
                }
            }
        }
    }

    fn rev_parse(&self, reference: &str) -> Result<CommitId, VcsError> {
        let revision = format!("{reference}^{{commit}}");
        let output = self.succeed(["rev-parse", "--verify", &revision])?;
        self.oid("rev-parse", &output)
    }
}

struct Worktree {
    path: PathBuf,
    branch: Option<String>,
}

enum Advance {
    Done,
    Dirty(PathBuf),
    Moved,
}

/// Git lists worktrees by their real path, so a path through a symbolic link has to be resolved.
fn same_path(left: &Path, right: &Path) -> bool {
    let resolve = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    resolve(left) == resolve(right)
}

fn describe(args: &[&OsStr]) -> String {
    args.iter()
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

impl VcsAdapter for GitCli {
    fn prepare_worktree(&self, branch: &str, path: &Path, target: &str) -> Result<(), VcsError> {
        self.succeed(["worktree", "prune"])?;
        let reference = format!("refs/heads/{branch}");
        let existing = self.worktrees()?;
        if let Some(worktree) = existing
            .iter()
            .find(|worktree| same_path(&worktree.path, path))
        {
            return if worktree.branch.as_deref() == Some(reference.as_str()) {
                Ok(())
            } else {
                Err(VcsError::Git {
                    command: format!("worktree add {}", path.display()),
                    stderr: format!("the worktree there does not have `{branch}` checked out"),
                })
            };
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(VcsError::Spawn)?;
        }
        if self.branch_exists(branch)? {
            self.succeed_in(
                &self.repository,
                [
                    OsStr::new("worktree"),
                    OsStr::new("add"),
                    path.as_os_str(),
                    OsStr::new(branch),
                ],
            )?;
        } else {
            if !self.branch_exists(target)? {
                return Err(VcsError::NoBranch(target.into()));
            }
            self.succeed_in(
                &self.repository,
                [
                    OsStr::new("worktree"),
                    OsStr::new("add"),
                    OsStr::new("-b"),
                    OsStr::new(branch),
                    path.as_os_str(),
                    OsStr::new(target),
                ],
            )?;
        }
        Ok(())
    }

    fn detached_worktree(&self, path: &Path, commit: CommitId) -> Result<(), VcsError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(VcsError::Spawn)?;
        }
        self.succeed_in(
            &self.repository,
            [
                OsStr::new("worktree"),
                OsStr::new("add"),
                OsStr::new("--detach"),
                path.as_os_str(),
                OsStr::new(&commit.to_string()),
            ],
        )?;
        Ok(())
    }

    fn remove_worktree(&self, path: &Path) -> Result<(), VcsError> {
        self.succeed_in(
            &self.repository,
            [
                OsStr::new("worktree"),
                OsStr::new("remove"),
                OsStr::new("--force"),
                path.as_os_str(),
            ],
        )?;
        Ok(())
    }

    fn delete_branch(&self, branch: &str) -> Result<(), VcsError> {
        self.succeed(["branch", "-D", branch])?;
        Ok(())
    }

    fn branch_head(&self, branch: &str) -> Result<CommitId, VcsError> {
        if !self.branch_exists(branch)? {
            return Err(VcsError::NoBranch(branch.into()));
        }
        self.rev_parse(&format!("refs/heads/{branch}"))
    }

    fn commits_ahead(&self, target: &str, commit: CommitId) -> Result<usize, VcsError> {
        let range = format!("refs/heads/{target}..{commit}");
        let output = self.succeed(["rev-list", "--count", &range])?;
        output
            .trim()
            .parse()
            .map_err(|_| VcsError::UnexpectedOutput {
                command: "rev-list --count".into(),
                output: output.trim().into(),
            })
    }

    fn diff(&self, target: &str, commit: CommitId) -> Result<String, VcsError> {
        let range = format!("refs/heads/{target}...{commit}");
        self.succeed(["diff", &range])
    }

    fn merge(
        &self,
        target: &str,
        commit: CommitId,
        message: &str,
    ) -> Result<MergeOutcome, VcsError> {
        let old = self.branch_head(target)?;
        let merged = self.attempt([
            "merge-tree",
            "--write-tree",
            "--name-only",
            &old.to_string(),
            &commit.to_string(),
        ])?;
        let mut lines = merged.stdout.lines();
        if !merged.success {
            if merged.stdout.trim().is_empty() {
                return Err(VcsError::Git {
                    command: "merge-tree --write-tree".into(),
                    stderr: merged.stderr.trim().to_string(),
                });
            }
            lines.next();
            let paths = lines
                .take_while(|line| !line.is_empty())
                .map(str::to_string)
                .collect();
            return Ok(MergeOutcome::Conflict { paths });
        }
        let tree = lines.next().unwrap_or_default().trim().to_string();
        let new = self.succeed([
            "commit-tree",
            &tree,
            "-p",
            &old.to_string(),
            "-p",
            &commit.to_string(),
            "-m",
            message,
        ])?;
        let new = self.oid("commit-tree", &new)?;
        Ok(match self.advance(target, &old, &new)? {
            Advance::Done => MergeOutcome::Merged { commit: new },
            Advance::Dirty(worktree) => MergeOutcome::DirtyCheckout { worktree },
            Advance::Moved => MergeOutcome::TargetMoved,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestRepo {
        root: tempfile::TempDir,
        git: GitCli,
    }

    fn git_in(directory: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(directory)
            .env_remove("GIT_DIR")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    impl TestRepo {
        /// A repository on `main` with one commit that has `file.txt`.
        fn new() -> Self {
            let root = tempfile::TempDir::new().unwrap();
            let repository = root.path().join("repo");
            std::fs::create_dir(&repository).unwrap();
            git_in(&repository, &["init", "--quiet", "--initial-branch=main"]);
            git_in(&repository, &["config", "user.name", "Test"]);
            git_in(&repository, &["config", "user.email", "test@example.com"]);
            let repo = Self {
                git: GitCli::open(&repository).unwrap(),
                root,
            };
            repo.write(&repository, "file.txt", "one\ntwo\nthree\n");
            repo.commit(&repository, "first");
            repo
        }

        fn directory(&self) -> PathBuf {
            self.git.repository().to_path_buf()
        }

        fn write(&self, directory: &Path, name: &str, text: &str) {
            std::fs::write(directory.join(name), text).unwrap();
        }

        fn commit(&self, directory: &Path, message: &str) -> CommitId {
            git_in(directory, &["add", "--all"]);
            git_in(directory, &["commit", "--quiet", "-m", message]);
            self.git
                .oid("rev-parse", &git_in(directory, &["rev-parse", "HEAD"]))
                .unwrap()
        }

        fn worktree_path(&self, name: &str) -> PathBuf {
            self.root.path().join("worktrees").join(name)
        }

        /// Work on `branch` in its worktree and return the commit.
        fn work(&self, branch: &str, name: &str, text: &str) -> CommitId {
            let path = self.worktree_path(branch);
            self.git.prepare_worktree(branch, &path, "main").unwrap();
            self.write(&path, name, text);
            self.commit(&path, &format!("change {name}"))
        }

        fn main(&self) -> CommitId {
            self.git.branch_head("main").unwrap()
        }

        fn parents(&self, commit: CommitId) -> Vec<String> {
            let line = git_in(
                &self.directory(),
                &["rev-list", "--parents", "-n", "1", &commit.to_string()],
            );
            line.split_whitespace()
                .skip(1)
                .map(str::to_string)
                .collect()
        }
    }

    #[test]
    fn a_directory_that_is_not_a_repository_is_refused() {
        let directory = tempfile::TempDir::new().unwrap();
        assert!(GitCli::open(directory.path()).is_err());
    }

    #[test]
    fn opening_from_a_subdirectory_finds_the_repository() {
        let repo = TestRepo::new();
        let sub = repo.directory().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let opened = GitCli::open(&sub).unwrap();
        assert_eq!(
            opened.repository().canonicalize().unwrap(),
            repo.directory().canonicalize().unwrap()
        );
    }

    #[test]
    fn an_item_gets_a_branch_and_a_worktree_that_later_sessions_continue() {
        let repo = TestRepo::new();
        let path = repo.worktree_path("wf/k3f9");
        repo.git.prepare_worktree("wf/k3f9", &path, "main").unwrap();
        assert!(path.join("file.txt").is_file());
        assert_eq!(repo.git.branch_head("wf/k3f9").unwrap(), repo.main());
        repo.git.prepare_worktree("wf/k3f9", &path, "main").unwrap();

        let first = repo.work("wf/k3f9", "a.txt", "a\n");
        assert_eq!(repo.git.branch_head("wf/k3f9").unwrap(), first);
        assert_eq!(repo.git.commits_ahead("main", first).unwrap(), 1);

        repo.git.remove_worktree(&path).unwrap();
        assert!(!path.exists());
        repo.git.prepare_worktree("wf/k3f9", &path, "main").unwrap();
        assert!(
            path.join("a.txt").is_file(),
            "the branch continues where it was"
        );
        assert_eq!(repo.git.branch_head("wf/k3f9").unwrap(), first);
    }

    #[test]
    fn a_stale_worktree_entry_after_its_directory_is_gone_is_replaced() {
        let repo = TestRepo::new();
        let path = repo.worktree_path("k3f9");
        repo.git.prepare_worktree("wf/k3f9", &path, "main").unwrap();
        std::fs::remove_dir_all(&path).unwrap();
        repo.git.prepare_worktree("wf/k3f9", &path, "main").unwrap();
        assert!(path.join("file.txt").is_file());
    }

    #[test]
    fn a_worktree_with_another_branch_is_not_taken_for_the_item() {
        let repo = TestRepo::new();
        let path = repo.worktree_path("shared");
        repo.git.prepare_worktree("wf/one", &path, "main").unwrap();
        assert!(repo.git.prepare_worktree("wf/two", &path, "main").is_err());
    }

    #[test]
    fn a_missing_target_is_named() {
        let repo = TestRepo::new();
        let error = repo
            .git
            .prepare_worktree("wf/x", &repo.worktree_path("x"), "trunk")
            .unwrap_err();
        assert!(matches!(error, VcsError::NoBranch(name) if name == "trunk"));
    }

    #[test]
    fn the_diff_is_what_the_branch_changed_since_the_target() {
        let repo = TestRepo::new();
        let commit = repo.work("wf/k3f9", "file.txt", "one\ntwo\nthree\nfour\n");
        let main = repo.directory();
        repo.write(&main, "other.txt", "elsewhere\n");
        repo.commit(&main, "unrelated");
        let diff = repo.git.diff("main", commit).unwrap();
        assert!(diff.contains("+four"), "{diff}");
        assert!(!diff.contains("elsewhere"), "{diff}");
    }

    #[test]
    fn a_reviewer_gets_a_detached_checkout_of_the_commit() {
        let repo = TestRepo::new();
        let commit = repo.work("wf/k3f9", "a.txt", "a\n");
        let path = repo.worktree_path("review");
        repo.git.detached_worktree(&path, commit).unwrap();
        assert!(path.join("a.txt").is_file());
        assert_eq!(git_in(&path, &["rev-parse", "HEAD"]), commit.to_string());
        repo.git.remove_worktree(&path).unwrap();
    }

    #[test]
    fn a_branch_can_be_deleted_after_its_worktree_is_gone() {
        let repo = TestRepo::new();
        repo.work("wf/k3f9", "a.txt", "a\n");
        repo.git
            .remove_worktree(&repo.worktree_path("wf/k3f9"))
            .unwrap();
        repo.git.delete_branch("wf/k3f9").unwrap();
        assert!(matches!(
            repo.git.branch_head("wf/k3f9"),
            Err(VcsError::NoBranch(_))
        ));
    }

    #[test]
    fn a_clean_merge_updates_the_checked_out_target_and_its_files() {
        let repo = TestRepo::new();
        let work = repo.work("wf/k3f9", "a.txt", "a\n");
        let before = repo.main();
        let outcome = repo.git.merge("main", work, "Merge #k3f9: add a").unwrap();
        let MergeOutcome::Merged { commit } = outcome else {
            panic!("expected a merge, got {outcome:?}");
        };
        assert_eq!(repo.main(), commit);
        assert_eq!(repo.parents(commit), [before.to_string(), work.to_string()]);
        assert_eq!(
            git_in(&repo.directory(), &["log", "-1", "--format=%s", "main"]),
            "Merge #k3f9: add a"
        );
        assert!(repo.directory().join("a.txt").is_file());
        assert_eq!(git_in(&repo.directory(), &["status", "--porcelain"]), "");
    }

    #[test]
    fn a_target_that_is_not_checked_out_moves_by_its_reference() {
        let repo = TestRepo::new();
        let work = repo.work("wf/k3f9", "a.txt", "a\n");
        git_in(&repo.directory(), &["checkout", "--quiet", "--detach"]);
        let outcome = repo.git.merge("main", work, "Merge #k3f9: add a").unwrap();
        let MergeOutcome::Merged { commit } = outcome else {
            panic!("expected a merge, got {outcome:?}");
        };
        assert_eq!(repo.main(), commit);
    }

    #[test]
    fn work_that_conflicts_is_reported_and_nothing_moves() {
        let repo = TestRepo::new();
        let work = repo.work("wf/k3f9", "file.txt", "one\nTWO\nthree\n");
        let main = repo.directory();
        repo.write(&main, "file.txt", "one\n2\nthree\n");
        let before = repo.commit(&main, "conflicting change");
        let outcome = repo.git.merge("main", work, "Merge #k3f9").unwrap();
        assert_eq!(
            outcome,
            MergeOutcome::Conflict {
                paths: vec!["file.txt".into()]
            }
        );
        assert_eq!(repo.main(), before);
    }

    #[test]
    fn a_dirty_checkout_of_the_target_is_never_touched() {
        let repo = TestRepo::new();
        let work = repo.work("wf/k3f9", "a.txt", "a\n");
        let main = repo.directory();
        repo.write(&main, "file.txt", "edited by a person\n");
        let before = repo.main();
        let outcome = repo.git.merge("main", work, "Merge #k3f9").unwrap();
        assert_eq!(
            outcome,
            MergeOutcome::DirtyCheckout {
                worktree: main.clone()
            }
        );
        assert_eq!(repo.main(), before);
        assert_eq!(
            std::fs::read_to_string(main.join("file.txt")).unwrap(),
            "edited by a person\n"
        );
    }

    #[test]
    fn a_target_that_moved_during_the_merge_is_left_alone() {
        let repo = TestRepo::new();
        let work = repo.work("wf/k3f9", "a.txt", "a\n");
        let old = repo.main();
        let main = repo.directory();
        repo.write(&main, "b.txt", "b\n");
        let moved_to = repo.commit(&main, "someone else merged first");
        let outcome = repo.git.advance("main", &old, &work).unwrap();
        assert!(matches!(outcome, Advance::Moved));
        assert_eq!(repo.main(), moved_to);

        git_in(&main, &["checkout", "--quiet", "--detach"]);
        let outcome = repo.git.advance("main", &old, &work).unwrap();
        assert!(matches!(outcome, Advance::Moved));
        assert_eq!(repo.main(), moved_to);
    }
}

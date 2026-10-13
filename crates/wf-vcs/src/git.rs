use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use wf_core::session::{CommitId, GitOid};

use crate::{CommitSigning, DirtyCheckout, MergeOutcome, VcsAdapter, VcsError};

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
        let common = Self::run_in(
            directory,
            ["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        if !common.success {
            return Err(VcsError::NotARepository(directory.to_path_buf()));
        }
        // Whichever worktree `directory` is in, the repository is the main checkout, which stays.
        let common = PathBuf::from(common.stdout.trim());
        let repository = match common.file_name() {
            Some(name) if name == ".git" => common.parent().map(Path::to_path_buf),
            _ => None,
        };
        Ok(Self {
            repository: repository.unwrap_or(common),
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

    /// Whether the git config asks for commits to be signed (`commit.gpgsign`).
    fn signs_commits(&self) -> Result<bool, VcsError> {
        let configured = self.attempt(["config", "--type=bool", "--get", "commit.gpgsign"])?;
        Ok(configured.success && configured.stdout.trim() == "true")
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

    /// The tracked files edited in `worktree`. Untracked files do not count, such as the marker
    /// `wf init` leaves: git itself refuses to overwrite one, and nothing else is at risk.
    fn changed_paths(&self, worktree: &Path) -> Result<Vec<String>, VcsError> {
        let status = self.succeed_in(
            worktree,
            [
                OsStr::new("status"),
                OsStr::new("--porcelain"),
                OsStr::new("-z"),
                OsStr::new("--untracked-files=no"),
            ],
        )?;
        // Each entry is `XY path`; a rename or copy is followed by the path it came from.
        let mut entries = status.split('\0').filter(|entry| !entry.is_empty());
        let mut paths = Vec::new();
        while let Some(entry) = entries.next() {
            let (Some(code), Some(path)) = (entry.get(..2), entry.get(3..)) else {
                return Err(VcsError::UnexpectedOutput {
                    command: "status --porcelain -z".into(),
                    output: status.clone(),
                });
            };
            if code.contains(['R', 'C']) {
                entries.next();
            }
            paths.push(path.to_string());
        }
        Ok(paths)
    }

    fn is_clean(&self, worktree: &Path) -> Result<bool, VcsError> {
        Ok(self.changed_paths(worktree)?.is_empty())
    }

    /// Where `branch` is checked out with changes, if it is.
    pub fn dirty_checkout(&self, branch: &str) -> Result<Option<DirtyCheckout>, VcsError> {
        let reference = format!("refs/heads/{branch}");
        let Some(worktree) = self
            .worktrees()?
            .into_iter()
            .find(|worktree| worktree.branch.as_deref() == Some(reference.as_str()))
        else {
            return Ok(None);
        };
        let paths = self.changed_paths(&worktree.path)?;
        Ok((!paths.is_empty()).then_some(DirtyCheckout {
            worktree: worktree.path,
            paths,
        }))
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
                let paths = self.changed_paths(&worktree.path)?;
                if !paths.is_empty() {
                    return Ok(Advance::Dirty(DirtyCheckout {
                        worktree: worktree.path,
                        paths,
                    }));
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

    /// The commit that brought `commit` into `head`, when it is there already: the merge with
    /// `commit` as a parent, or `commit` itself when it came in without a merge.
    fn landed(&self, commit: CommitId, head: CommitId) -> Result<Option<CommitId>, VcsError> {
        let (commit, head) = (commit.to_string(), head.to_string());
        let ancestor = self.attempt(["merge-base", "--is-ancestor", &commit, &head])?;
        if !ancestor.success {
            return Ok(None);
        }
        let range = format!("{commit}..{head}");
        let listed = self.succeed(["rev-list", "--parents", "--ancestry-path", &range])?;
        // Newest first, so the last merge listed is the one that brought it in.
        let merge = listed.lines().rev().find_map(|line| {
            let mut ids = line.split_whitespace();
            let merge = ids.next()?;
            ids.any(|parent| parent == commit).then_some(merge)
        });
        self.oid("rev-list", merge.unwrap_or(&commit)).map(Some)
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
    Dirty(DirtyCheckout),
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
        self.succeed(["worktree", "prune"])?;
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

    fn diff(&self, target: &str, commit: CommitId, args: &[String]) -> Result<String, VcsError> {
        let range = format!("refs/heads/{target}...{commit}");
        let finished = Self::run_in(
            &self.repository,
            ["diff", range.as_str()]
                .into_iter()
                .chain(args.iter().map(String::as_str)),
        )?;
        if finished.success {
            Ok(finished.stdout)
        } else {
            Err(VcsError::Git {
                command: format!("diff {range} {}", args.join(" ")),
                stderr: finished.stderr.trim().to_string(),
            })
        }
    }

    fn show_diff(&self, target: &str, commit: CommitId, args: &[String]) -> Result<(), VcsError> {
        let range = format!("refs/heads/{target}...{commit}");
        // The user's environment stays as it is, so the pager, colors and locale are theirs.
        let status = Command::new("git")
            .arg("diff")
            .arg(&range)
            .args(args)
            .current_dir(&self.repository)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .status()
            .map_err(VcsError::Spawn)?;
        if status.success() {
            Ok(())
        } else {
            Err(VcsError::Git {
                command: format!("diff {range} {}", args.join(" ")),
                stderr: format!("exited with {status}"),
            })
        }
    }

    fn checkout_detached(&self, commit: CommitId) -> Result<(), VcsError> {
        if !self.is_clean(&self.repository)? {
            return Err(VcsError::DirtyCheckout(self.repository.clone()));
        }
        self.succeed(["checkout", "--quiet", "--detach", &commit.to_string()])?;
        Ok(())
    }

    fn checkout_previous(&self) -> Result<(), VcsError> {
        if !self.is_clean(&self.repository)? {
            return Err(VcsError::DirtyCheckout(self.repository.clone()));
        }
        self.succeed(["checkout", "--quiet", "-"])?;
        Ok(())
    }

    fn merge(
        &self,
        target: &str,
        commit: CommitId,
        message: &str,
        signing: &CommitSigning,
    ) -> Result<MergeOutcome, VcsError> {
        let old = self.branch_head(target)?;
        // Merged already, by a merge whose event was never written: merging again would only add
        // an empty merge commit.
        if let Some(landed) = self.landed(commit, old)? {
            return Ok(MergeOutcome::Merged { commit: landed });
        }
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
        // Unlike `git commit`, `commit-tree` signs only when told to.
        let sign = match signing {
            CommitSigning::AsConfigured if self.signs_commits()? => "-S",
            CommitSigning::Key(_) => "-S",
            CommitSigning::AsConfigured | CommitSigning::Unsigned => "--no-gpg-sign",
        };
        let mut args: Vec<String> = Vec::new();
        for (key, value) in signing.config() {
            args.extend(["-c".to_string(), format!("{key}={value}")]);
        }
        args.extend(
            [
                "commit-tree",
                sign,
                &tree,
                "-p",
                &old.to_string(),
                "-p",
                &commit.to_string(),
                "-m",
                message,
            ]
            .map(str::to_string),
        );
        let made = Self::run_in(&self.repository, &args)?;
        if !made.success {
            return Err(VcsError::Git {
                command: "commit-tree".into(),
                stderr: made.stderr.trim().to_string(),
            });
        }
        let new = made.stdout;
        let new = self.oid("commit-tree", &new)?;
        Ok(match self.advance(target, &old, &new)? {
            Advance::Done => MergeOutcome::Merged { commit: new },
            Advance::Dirty(dirty) => MergeOutcome::DirtyCheckout(dirty),
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
    fn opening_from_a_linked_worktree_finds_the_main_checkout() {
        let repo = TestRepo::new();
        let path = repo.worktree_path("wf/k3f9");
        repo.git.prepare_worktree("wf/k3f9", &path, "main").unwrap();
        let opened = GitCli::open(&path).unwrap();
        assert_eq!(
            opened.repository().canonicalize().unwrap(),
            repo.directory().canonicalize().unwrap()
        );
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
        let diff = repo.git.diff("main", commit, &[]).unwrap();
        assert!(diff.contains("+four"), "{diff}");
        assert!(!diff.contains("elsewhere"), "{diff}");
        let stat = repo.git.diff("main", commit, &["--stat".into()]).unwrap();
        assert!(stat.contains("file.txt | 1 +"), "{stat}");
    }

    #[test]
    fn the_main_checkout_goes_to_a_commit_and_back() {
        let repo = TestRepo::new();
        let commit = repo.work("wf/k3f9", "file.txt", "one\ntwo\nthree\nfour\n");
        let main = repo.directory();
        repo.git.checkout_detached(commit).unwrap();
        assert_eq!(
            git_in(&main, &["rev-parse", "HEAD"]).trim(),
            commit.to_string()
        );
        repo.git.checkout_previous().unwrap();
        assert_eq!(
            git_in(&main, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
            "main"
        );
        repo.write(&main, "file.txt", "changed\n");
        assert!(matches!(
            repo.git.checkout_detached(commit),
            Err(VcsError::DirtyCheckout(_))
        ));
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
        let outcome = repo
            .git
            .merge(
                "main",
                work,
                "Merge #k3f9: add a",
                &CommitSigning::AsConfigured,
            )
            .unwrap();
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
    fn merges_are_signed_as_configured_or_not_at_all() {
        let repo = TestRepo::new();
        // Signs with a stand-in for ssh-keygen, which writes a signature beside the data it gets.
        let signer = repo.root.path().join("fake-sign");
        std::fs::write(
            &signer,
            "#!/bin/sh\nfor last; do :; done\necho 'fake signature' > \"$last.sig\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&signer, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let main = repo.directory();
        for (key, value) in [
            ("commit.gpgsign", "true"),
            ("gpg.format", "ssh"),
            ("user.signingkey", "key::ssh-ed25519 AAAAfake"),
            ("gpg.ssh.program", signer.to_str().unwrap()),
        ] {
            git_in(&main, &["config", key, value]);
        }
        let signed = |commit: CommitId| {
            git_in(&main, &["cat-file", "commit", &commit.to_string()]).contains("gpgsig")
        };
        for (branch, signing, expected) in [
            ("wf/a", CommitSigning::Unsigned, false),
            ("wf/b", CommitSigning::AsConfigured, true),
        ] {
            let work = repo.work(branch, &format!("{}.txt", &branch[3..]), "x\n");
            let MergeOutcome::Merged { commit } =
                repo.git.merge("main", work, "Merge", &signing).unwrap()
            else {
                panic!("expected a merge");
            };
            assert_eq!(signed(commit), expected, "{signing:?}");
        }

        // With the device's key, the merge verifies against the devices of the workforce.
        let device = wf_core::identity::DeviceSigner::generate([7; 32]);
        let key = repo.root.path().join("device-key");
        std::fs::write(&key, crate::signing::device_private_key(&device).unwrap()).unwrap();
        std::fs::set_permissions(&key, std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .unwrap();
        let allowed = repo.root.path().join("allowed_signers");
        std::fs::write(
            &allowed,
            crate::signing::allowed_signer("joshua@laptop", device.key()).unwrap(),
        )
        .unwrap();
        let work = repo.work("wf/c", "c.txt", "c\n");
        let MergeOutcome::Merged { commit } = repo
            .git
            .merge("main", work, "Merge", &CommitSigning::Key(key))
            .unwrap()
        else {
            panic!("expected a merge");
        };
        let verified = Command::new("git")
            .args([
                "-c",
                &format!("gpg.ssh.allowedSignersFile={}", allowed.display()),
            ])
            .args(["-c", "gpg.ssh.program=ssh-keygen"])
            .args(["verify-commit", &commit.to_string()])
            .current_dir(&main)
            .output()
            .unwrap();
        assert!(
            verified.status.success(),
            "{}",
            String::from_utf8_lossy(&verified.stderr)
        );
        assert!(String::from_utf8_lossy(&verified.stderr).contains("joshua@laptop"));
    }

    #[test]
    fn merging_what_is_merged_already_names_the_earlier_merge() {
        let repo = TestRepo::new();
        let work = repo.work("wf/k3f9", "a.txt", "a\n");
        let MergeOutcome::Merged { commit: first } = repo
            .git
            .merge("main", work, "Merge #k3f9", &CommitSigning::AsConfigured)
            .unwrap()
        else {
            panic!("expected a merge");
        };
        let main = repo.directory();
        repo.write(&main, "b.txt", "b\n");
        let later = repo.commit(&main, "later work on main");
        let again = repo
            .git
            .merge("main", work, "Merge #k3f9", &CommitSigning::AsConfigured)
            .unwrap();
        assert_eq!(again, MergeOutcome::Merged { commit: first });
        assert_eq!(repo.main(), later, "nothing new on main");
    }

    #[test]
    fn a_target_that_is_not_checked_out_moves_by_its_reference() {
        let repo = TestRepo::new();
        let work = repo.work("wf/k3f9", "a.txt", "a\n");
        git_in(&repo.directory(), &["checkout", "--quiet", "--detach"]);
        let outcome = repo
            .git
            .merge(
                "main",
                work,
                "Merge #k3f9: add a",
                &CommitSigning::AsConfigured,
            )
            .unwrap();
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
        let outcome = repo
            .git
            .merge("main", work, "Merge #k3f9", &CommitSigning::AsConfigured)
            .unwrap();
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
        let outcome = repo
            .git
            .merge("main", work, "Merge #k3f9", &CommitSigning::AsConfigured)
            .unwrap();
        assert_eq!(
            outcome,
            MergeOutcome::DirtyCheckout(DirtyCheckout {
                worktree: main.clone(),
                paths: vec!["file.txt".into()],
            })
        );
        assert_eq!(repo.main(), before);
        assert_eq!(
            std::fs::read_to_string(main.join("file.txt")).unwrap(),
            "edited by a person\n"
        );
    }

    #[test]
    fn untracked_files_do_not_block_a_merge_into_the_checked_out_target() {
        let repo = TestRepo::new();
        let work = repo.work("wf/k3f9", "a.txt", "a\n");
        let main = repo.directory();
        repo.write(&main, ".workforce.toml", "id = \"x\"\n");
        let outcome = repo
            .git
            .merge("main", work, "Merge #k3f9", &CommitSigning::AsConfigured)
            .unwrap();
        assert!(
            matches!(outcome, MergeOutcome::Merged { .. }),
            "{outcome:?}"
        );
        assert!(main.join("a.txt").exists());
        assert!(main.join(".workforce.toml").exists());
    }

    #[test]
    fn a_dirty_checkout_names_its_changed_files_and_where_a_rename_went() {
        let repo = TestRepo::new();
        let main = repo.directory();
        repo.write(&main, "second.txt", "2\n");
        repo.commit(&main, "second");
        git_in(&main, &["mv", "file.txt", "file with space.txt"]);
        repo.write(&main, "second.txt", "two\n");
        repo.write(&main, "untracked.txt", "new\n");
        let dirty = repo.git.dirty_checkout("main").unwrap().unwrap();
        assert_eq!(dirty.worktree, main);
        assert_eq!(dirty.paths, ["file with space.txt", "second.txt"]);
        assert_eq!(repo.git.dirty_checkout("wf/k3f9").unwrap(), None);
    }

    #[test]
    fn a_long_list_of_changed_files_is_cut_short() {
        let dirty = DirtyCheckout {
            worktree: PathBuf::from("/repo"),
            paths: (1..=13).map(|number| format!("{number}.txt")).collect(),
        };
        assert_eq!(
            dirty.files(),
            "1.txt, 2.txt, 3.txt, 4.txt, 5.txt, 6.txt, 7.txt, 8.txt, 9.txt, 10.txt and 3 more"
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

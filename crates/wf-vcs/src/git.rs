use std::ffi::OsStr;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use wf_core::session::{CommitId, GitOid};

use crate::{
    CommitSigning, DirtyCheckout, MergeOutcome, MergeTree, Pager, Rebase, VcsAdapter, VcsError,
    Work,
};

const MINIMUM_VERSION: (u32, u32) = (2, 38);

/// `LESS` for a `less` that stays open on a diff shorter than the screen: the principal's `less`
/// without `F`, which quits at once then, or git's default `FRX` without it when it is not set.
fn less_staying_open(less: Option<&str>) -> String {
    match less {
        Some(less) => less.replace('F', ""),
        None => "RX".to_owned(),
    }
}

/// The revisions `git diff` compares to show `work`.
fn diff_revisions(work: &Work) -> Vec<String> {
    match work {
        Work::Unmerged { target, commit } => vec![format!("refs/heads/{target}...{commit}")],
        Work::Merged { merge, .. } => vec![format!("{merge}^1"), merge.to_string()],
    }
}

/// A git repository, driven through the `git` command.
#[derive(Clone, Debug)]
pub struct GitCli {
    repository: PathBuf,
}

/// `git`, kept from running code that an agent could have put in the repository's config: hooks
/// and an fsmonitor run outside the agent's sandbox, with the privileges of whoever runs git.
fn git() -> Command {
    let mut command = Command::new("git");
    command.args([
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "core.fsmonitor=false",
    ]);
    inherit_only_stdio(&mut command);
    command
}

/// Has `command` inherit no descriptor beyond stdin, stdout and stderr. On macOS std creates
/// pipes and sockets close-on-exec in two steps, as there is no `pipe2`; git spawned on one
/// thread in between inherits what another thread opened, and holds a pipe some other process
/// reads from open until it exits.
fn inherit_only_stdio(command: &mut Command) {
    // SAFETY: setting close-on-exec only calls functions that are async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            close_fds::set_fds_cloexec(3, &[]);
            Ok(())
        });
    }
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

    /// The top of the checkout that contains `directory`: the main checkout or a worktree.
    pub fn checkout_root(directory: &Path) -> Result<PathBuf, VcsError> {
        let top = Self::run_in(directory, ["rev-parse", "--show-toplevel"])?;
        if !top.success {
            return Err(VcsError::NotARepository(directory.to_path_buf()));
        }
        Ok(PathBuf::from(top.stdout.trim()))
    }

    fn run_in<I, S>(directory: &Path, args: I) -> Result<Finished, VcsError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Self::run_with(directory, args, &[])
    }

    /// Runs git in `directory` with `environment` set on top.
    fn run_with<I, S>(
        directory: &Path,
        args: I,
        environment: &[(&str, &str)],
    ) -> Result<Finished, VcsError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output: Output = git()
            .args(args)
            .envs(environment.iter().copied())
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
        let listed = self.succeed([
            "rev-list",
            "--first-parent",
            "--ancestry-path",
            "--parents",
            &range,
        ])?;
        // Oldest first-parent commit after `commit`: when its first parent is `commit`, the
        // target fast-forwarded over it; otherwise it is the merge that brought `commit` in.
        let merge = listed.lines().next_back().and_then(|line| {
            let mut ids = line.split_whitespace();
            let oldest = ids.next()?;
            (ids.next() != Some(commit.as_str())).then_some(oldest)
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
        self.prune_worktrees()?;
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
        self.prune_worktrees()?;
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

    fn check_out_tree(&self, path: &Path, tree: GitOid) -> Result<(), VcsError> {
        // Prune first, so a worktree whose directory was deleted is made again.
        self.succeed(["worktree", "prune"])?;
        let exists = self
            .worktrees()?
            .iter()
            .any(|worktree| same_path(&worktree.path, path));
        if !exists {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(VcsError::Spawn)?;
            }
            // The files come from the checkout below, which knows whether `tree` is a commit.
            self.succeed_in(
                &self.repository,
                [
                    OsStr::new("worktree"),
                    OsStr::new("add"),
                    OsStr::new("--detach"),
                    OsStr::new("--no-checkout"),
                    path.as_os_str(),
                ],
            )?;
        }
        let tree = tree.to_string();
        let kind = self.succeed(["cat-file", "-t", &tree])?;
        if kind.trim() == "commit" {
            // HEAD names the commit, so `git describe` and `git status` see it too.
            self.succeed_in(
                path,
                ["checkout", "--quiet", "--detach", "--force", &tree].map(OsStr::new),
            )?;
        } else {
            // A bare tree has no commit for HEAD to name; only the index and files change.
            self.succeed_in(path, ["read-tree", "-u", "--reset", &tree].map(OsStr::new))?;
        }
        self.succeed_in(path, ["clean", "-ffd", "--quiet"].map(OsStr::new))?;
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

    fn prune_worktrees(&self) -> Result<(), VcsError> {
        self.succeed(["worktree", "prune"])?;
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

    fn diff(&self, work: &Work, args: &[String]) -> Result<String, VcsError> {
        let revisions = diff_revisions(work);
        let finished = Self::run_in(
            &self.repository,
            ["diff"]
                .into_iter()
                .chain(revisions.iter().map(String::as_str))
                .chain(args.iter().map(String::as_str)),
        )?;
        if finished.success {
            Ok(finished.stdout)
        } else {
            Err(VcsError::Git {
                command: format!("diff {} {}", revisions.join(" "), args.join(" ")),
                stderr: finished.stderr.trim().to_string(),
            })
        }
    }

    fn show_diff(&self, work: &Work, args: &[String], pager: Pager) -> Result<(), VcsError> {
        let revisions = diff_revisions(work);
        // The principal's environment stays as it is, so the pager, colors and locale are theirs.
        let mut command = git();
        match pager {
            Pager::AsConfigured => {}
            Pager::StaysOpen => {
                let less = std::env::var("LESS").ok();
                command.env("LESS", less_staying_open(less.as_deref()));
            }
        }
        let status = command
            .arg("diff")
            .args(&revisions)
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
                command: format!("diff {} {}", revisions.join(" "), args.join(" ")),
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
        match self.merge_tree(target, commit)? {
            MergeTree::Landed { commit } => Ok(MergeOutcome::Merged { commit }),
            MergeTree::Conflict { paths } => Ok(MergeOutcome::Conflict { paths }),
            MergeTree::Clean { base, tree } => {
                self.commit_merge(target, base, tree, commit, message, signing)
            }
        }
    }

    fn merge_tree(&self, target: &str, commit: CommitId) -> Result<MergeTree, VcsError> {
        let base = self.branch_head(target)?;
        // Merged already, by a merge whose event was never written: merging again would only add
        // an empty merge commit.
        if let Some(landed) = self.landed(commit, base)? {
            return Ok(MergeTree::Landed { commit: landed });
        }
        let merged = self.attempt([
            "merge-tree",
            "--write-tree",
            "--name-only",
            "-z",
            &base.to_string(),
            &commit.to_string(),
        ])?;
        // With `-z` every field ends in NUL and paths come as they are, not C-quoted: the tree,
        // the conflicted paths, an empty field, then the informational messages.
        let mut fields = merged.stdout.split('\0');
        let tree = fields.next().unwrap_or_default().trim();
        if !merged.success {
            if tree.is_empty() {
                return Err(VcsError::Git {
                    command: "merge-tree --write-tree".into(),
                    stderr: merged.stderr.trim().to_string(),
                });
            }
            let paths = fields
                .take_while(|field| !field.is_empty())
                .map(str::to_string)
                .collect();
            return Ok(MergeTree::Conflict { paths });
        }
        let tree = tree.parse().map_err(|_| VcsError::UnexpectedOutput {
            command: "merge-tree --write-tree".into(),
            output: tree.into(),
        })?;
        Ok(MergeTree::Clean { base, tree })
    }

    fn commit_merge(
        &self,
        target: &str,
        base: CommitId,
        tree: GitOid,
        commit: CommitId,
        message: &str,
        signing: &CommitSigning,
    ) -> Result<MergeOutcome, VcsError> {
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
                &tree.to_string(),
                "-p",
                &base.to_string(),
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
        Ok(match self.advance(target, &base, &new)? {
            Advance::Done => MergeOutcome::Merged { commit: new },
            Advance::Dirty(dirty) => MergeOutcome::DirtyCheckout(dirty),
            Advance::Moved => MergeOutcome::TargetMoved,
        })
    }

    fn tree_of(&self, commit: CommitId) -> Result<GitOid, VcsError> {
        let revision = format!("{commit}^{{tree}}");
        let output = self.succeed(["rev-parse", "--verify", &revision])?;
        output
            .trim()
            .parse()
            .map_err(|_| VcsError::UnexpectedOutput {
                command: "rev-parse".into(),
                output: output.trim().into(),
            })
    }

    fn rebase(
        &self,
        worktree: &Path,
        target: &str,
        signing: &CommitSigning,
        committer: &str,
    ) -> Result<Rebase, VcsError> {
        let mut args: Vec<String> = Vec::new();
        for (key, value) in signing.config() {
            args.extend(["-c".to_string(), format!("{key}={value}")]);
        }
        match self.stopped_rebase(worktree)? {
            Some(paths) => {
                let unresolved: Vec<String> = paths
                    .into_iter()
                    .filter(|path| has_conflict_markers(&worktree.join(path)))
                    .collect();
                if !unresolved.is_empty() {
                    return Ok(Rebase::Conflict { paths: unresolved });
                }
                self.succeed_in(worktree, [OsStr::new("add"), OsStr::new("--update")])?;
                args.extend(["rebase", "--continue"].map(str::to_string));
            }
            None => {
                if !self.branch_exists(target)? {
                    return Err(VcsError::NoBranch(target.into()));
                }
                let reference = format!("refs/heads/{target}");
                let contained = Self::run_in(
                    worktree,
                    ["merge-base", "--is-ancestor", reference.as_str(), "HEAD"],
                )?;
                if contained.success {
                    return Ok(Rebase::UpToDate);
                }
                let paths = self.changed_paths(worktree)?;
                if !paths.is_empty() {
                    return Ok(Rebase::DirtyCheckout(DirtyCheckout {
                        worktree: worktree.to_path_buf(),
                        paths,
                    }));
                }
                args.extend(
                    [
                        "rebase",
                        "--no-autosquash",
                        "--no-autostash",
                        "--no-update-refs",
                        &reference,
                    ]
                    .map(str::to_string),
                );
            }
        }
        // The editor would open for the message of a commit whose conflicts were resolved.
        let rebased = Self::run_with(
            worktree,
            &args,
            &[("GIT_COMMITTER_NAME", committer), ("GIT_EDITOR", "true")],
        )?;
        if rebased.success {
            return Ok(Rebase::Rebased);
        }
        match self.stopped_rebase(worktree)? {
            Some(paths) if !paths.is_empty() => Ok(Rebase::Conflict { paths }),
            // Git refused before it began, or stopped for something other than a conflict.
            Some(_) | None => Err(VcsError::Git {
                command: "rebase".into(),
                stderr: rebased.stderr.trim().to_string(),
            }),
        }
    }

    fn stopped_rebase(&self, worktree: &Path) -> Result<Option<Vec<String>>, VcsError> {
        let state = self.succeed_in(
            worktree,
            [
                OsStr::new("rev-parse"),
                OsStr::new("--path-format=absolute"),
                OsStr::new("--git-path"),
                OsStr::new("rebase-merge"),
            ],
        )?;
        if !Path::new(state.trim()).is_dir() {
            return Ok(None);
        }
        self.unmerged_paths(worktree).map(Some)
    }

    fn abort_rebase(&self, worktree: &Path) -> Result<(), VcsError> {
        if self.stopped_rebase(worktree)?.is_some() {
            self.succeed_in(worktree, [OsStr::new("rebase"), OsStr::new("--abort")])?;
        }
        Ok(())
    }
}

/// Whether the file at `path` still has the lines git marks a conflict with. A file that is gone
/// has none: the conflict was resolved by deleting it.
fn has_conflict_markers(path: &Path) -> bool {
    let Ok(text) = std::fs::read(path) else {
        return false;
    };
    text.split(|byte| *byte == b'\n')
        .any(|line| line.starts_with(b"<<<<<<<") || line.starts_with(b">>>>>>>"))
}

impl GitCli {
    /// The files in conflict in `worktree`, relative to it.
    fn unmerged_paths(&self, worktree: &Path) -> Result<Vec<String>, VcsError> {
        let listing = self.succeed_in(
            worktree,
            [
                OsStr::new("diff"),
                OsStr::new("--name-only"),
                OsStr::new("--diff-filter=U"),
                OsStr::new("-z"),
            ],
        )?;
        Ok(listing
            .split('\0')
            .filter(|path| !path.is_empty())
            .map(str::to_string)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestRepo {
        root: tempfile::TempDir,
        git: GitCli,
    }

    #[test]
    fn less_staying_open_drops_quitting_on_one_screen() {
        assert_eq!(less_staying_open(None), "RX");
        assert_eq!(less_staying_open(Some("FRSX")), "RSX");
        assert_eq!(less_staying_open(Some("-R -i")), "-R -i");
    }

    /// While other threads run short commands with piped output, which on macOS create pipes
    /// that are not close-on-exec for a moment, a command git is run like holds only stdin,
    /// stdout and stderr.
    #[test]
    fn git_inherits_no_descriptor_while_other_threads_create_pipes() {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let commands: Vec<_> = (0..4)
            .map(|_| {
                let stop = std::sync::Arc::clone(&stop);
                std::thread::spawn(move || {
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        Command::new("true").output().unwrap();
                    }
                })
            })
            .collect();
        // The listing holds the directory open as the lowest free descriptor, 3.
        let probe = || {
            let mut command = Command::new("sh");
            command.args([
                "-c",
                r#"for path in /dev/fd/*; do echo "${path##*/}"; done"#,
            ]);
            inherit_only_stdio(&mut command);
            String::from_utf8(command.output().unwrap().stdout).unwrap()
        };
        let held: Vec<String> = std::thread::scope(|scope| {
            let probes: Vec<_> = (0..4)
                .map(|_| scope.spawn(|| (0..400).map(|_| probe()).collect::<Vec<_>>()))
                .collect();
            probes
                .into_iter()
                .flat_map(|probes| probes.join().unwrap())
                .collect()
        });
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for command in commands {
            command.join().unwrap();
        }
        for descriptors in held {
            assert_eq!(descriptors, "0\n1\n2\n3\n");
        }
    }

    /// The work on `commit` before it is merged into `main`.
    fn unmerged(commit: CommitId) -> Work {
        Work::Unmerged {
            target: "main".into(),
            commit,
        }
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

        /// Has the repository's config sign commits, with a stand-in for ssh-keygen that writes
        /// a signature beside the data it gets.
        fn sign_with_a_fake_signer(&self) {
            let signer = self.root.path().join("fake-sign");
            std::fs::write(
                &signer,
                "#!/bin/sh\nfor last; do :; done\necho 'fake signature' > \"$last.sig\"\n",
            )
            .unwrap();
            std::fs::set_permissions(&signer, std::os::unix::fs::PermissionsExt::from_mode(0o755))
                .unwrap();
            for (key, value) in [
                ("commit.gpgsign", "true"),
                ("gpg.format", "ssh"),
                ("user.signingkey", "key::ssh-ed25519 AAAAfake"),
                ("gpg.ssh.program", signer.to_str().unwrap()),
            ] {
                git_in(&self.directory(), &["config", key, value]);
            }
        }

        fn oid_of(&self, text: &str) -> CommitId {
            self.git.oid("rev-parse", text).unwrap()
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
    fn hooks_and_an_fsmonitor_in_the_config_never_run() {
        use std::os::unix::fs::PermissionsExt;

        let repo = TestRepo::new();
        let ran = repo.root.path().join("ran");
        let script = |name: &str| {
            let path = repo.root.path().join("hooks").join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let marker = ran.join(name);
            std::fs::write(
                &path,
                format!(
                    "#!/bin/sh\nmkdir -p '{}' && touch '{}'\n",
                    ran.display(),
                    marker.display()
                ),
            )
            .unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        };
        script("post-checkout");
        let hooks = repo.root.path().join("hooks");
        let fsmonitor = script("fsmonitor");
        git_in(
            &repo.directory(),
            &["config", "core.hooksPath", hooks.to_str().unwrap()],
        );
        git_in(
            &repo.directory(),
            &["config", "core.fsmonitor", fsmonitor.to_str().unwrap()],
        );

        let path = repo.worktree_path("wf/k3f9");
        repo.git.prepare_worktree("wf/k3f9", &path, "main").unwrap();
        assert!(repo.git.is_clean(&path).unwrap());
        assert!(!ran.exists(), "git ran {:?}", std::fs::read_dir(&ran).ok());
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
    fn pruning_forgets_the_worktrees_whose_directories_are_gone() {
        let repo = TestRepo::new();
        let (gone, kept) = (repo.worktree_path("gone"), repo.worktree_path("kept"));
        repo.git.prepare_worktree("wf/gone", &gone, "main").unwrap();
        repo.git.prepare_worktree("wf/kept", &kept, "main").unwrap();
        std::fs::remove_dir_all(&gone).unwrap();
        repo.git.prune_worktrees().unwrap();
        let listed: Vec<PathBuf> = repo
            .git
            .worktrees()
            .unwrap()
            .into_iter()
            .map(|worktree| worktree.path)
            .collect();
        assert_eq!(listed.len(), 2, "{listed:?}");
        assert!(listed.iter().any(|path| same_path(path, &kept)));
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
        let work = unmerged(commit);
        let diff = repo.git.diff(&work, &[]).unwrap();
        assert!(diff.contains("+four"), "{diff}");
        assert!(!diff.contains("elsewhere"), "{diff}");
        let stat = repo.git.diff(&work, &["--stat".into()]).unwrap();
        assert!(stat.contains("file.txt | 1 +"), "{stat}");
    }

    #[test]
    fn the_diff_of_merged_work_is_what_the_merge_brought_in_after_the_target_moved_on() {
        let repo = TestRepo::new();
        let commit = repo.work("wf/k3f9", "file.txt", "one\ntwo\nthree\nfour\n");
        let main = repo.directory();
        repo.write(&main, "before.txt", "already there\n");
        repo.commit(&main, "before the merge");
        let MergeOutcome::Merged { commit: merge } = repo
            .git
            .merge("main", commit, "Merge #k3f9", &CommitSigning::AsConfigured)
            .unwrap()
        else {
            panic!("expected a merge");
        };
        repo.write(&main, "after.txt", "later\n");
        repo.commit(&main, "after the merge");
        assert_eq!(repo.git.diff(&unmerged(commit), &[]).unwrap(), "");

        let work = Work::Merged { merge, commit };
        let diff = repo.git.diff(&work, &[]).unwrap();
        assert!(diff.contains("+four"), "{diff}");
        assert!(!diff.contains("already there"), "{diff}");
        assert!(!diff.contains("later"), "{diff}");
        let files = repo.git.file_diffs(&work).unwrap();
        assert_eq!(files.len(), 1, "{files:?}");
        assert_eq!(files[0].path, "file.txt");
    }

    #[test]
    fn the_diff_as_data_ignores_how_the_config_says_a_diff_looks() {
        let repo = TestRepo::new();
        let main = repo.directory();
        for (key, value) in [
            ("color.diff", "always"),
            ("diff.noprefix", "true"),
            ("diff.mnemonicPrefix", "true"),
        ] {
            git_in(&main, &["config", key, value]);
        }
        let commit = repo.work("wf/k3f9", "file.txt", "one\n2\nthree\nfour\n");
        let files = repo.git.file_diffs(&unmerged(commit)).unwrap();
        assert_eq!(files.len(), 1, "{files:?}");
        assert_eq!(files[0].path, "file.txt");
        assert_eq!(files[0].counts(), (2, 1));
        let crate::diff::Content::Text(hunks) = &files[0].content else {
            panic!("a text diff: {files:?}");
        };
        let added: Vec<_> = hunks[0]
            .lines
            .iter()
            .filter(|line| line.marker() == '+')
            .map(|line| (line.new_number(), line.text()))
            .collect();
        assert_eq!(added, [(Some(2), "2"), (Some(4), "four")]);
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
        repo.sign_with_a_fake_signer();
        let main = repo.directory();
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
            crate::signing::allowed_signer("ada@laptop", device.key()).unwrap(),
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
        assert!(String::from_utf8_lossy(&verified.stderr).contains("ada@laptop"));
    }

    #[test]
    fn a_merge_tree_is_committed_only_while_the_target_is_where_it_was_made() {
        let repo = TestRepo::new();
        let work = repo.work("wf/k3f9", "a.txt", "a\n");
        let MergeTree::Clean { base, tree } = repo.git.merge_tree("main", work).unwrap() else {
            panic!("expected a clean merge");
        };
        assert_eq!(base, repo.git.branch_head("main").unwrap());
        let main = repo.directory();
        assert_eq!(
            git_in(&main, &["ls-tree", "--name-only", &tree.to_string()]),
            "a.txt\nfile.txt"
        );
        assert_eq!(repo.git.tree_of(base).unwrap().to_string().len(), 40);

        repo.write(&main, "b.txt", "b\n");
        repo.commit(&main, "main moves on");
        let moved = repo
            .git
            .commit_merge("main", base, tree, work, "Merge", &CommitSigning::Unsigned)
            .unwrap();
        assert_eq!(moved, MergeOutcome::TargetMoved);

        let MergeTree::Clean { base, tree } = repo.git.merge_tree("main", work).unwrap() else {
            panic!("expected a clean merge");
        };
        let MergeOutcome::Merged { commit } = repo
            .git
            .commit_merge("main", base, tree, work, "Merge", &CommitSigning::Unsigned)
            .unwrap()
        else {
            panic!("expected a merge");
        };
        assert_eq!(repo.git.branch_head("main").unwrap(), commit);
        assert_eq!(repo.git.tree_of(commit).unwrap(), tree);
        assert_eq!(
            repo.git.merge_tree("main", work).unwrap(),
            MergeTree::Landed { commit }
        );
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
    fn work_fast_forwarded_into_the_target_is_itself_what_landed() {
        let repo = TestRepo::new();
        let work = repo.work("wf/k3f9", "a.txt", "a\n");
        let main = repo.directory();
        git_in(&main, &["merge", "--quiet", "--ff-only", "wf/k3f9"]);
        repo.write(&main, "b.txt", "b\n");
        let later = repo.commit(&main, "later work on main");
        assert_eq!(
            repo.git.merge_tree("main", work).unwrap(),
            MergeTree::Landed { commit: work }
        );
        assert_eq!(repo.main(), later);
    }

    #[test]
    fn a_dependent_branch_merged_later_does_not_replace_the_merge_that_landed() {
        let repo = TestRepo::new();
        let work = repo.work("wf/k3f9", "a.txt", "a\n");
        let MergeOutcome::Merged { commit: first } = repo
            .git
            .merge("main", work, "Merge #k3f9", &CommitSigning::AsConfigured)
            .unwrap()
        else {
            panic!("expected a merge");
        };
        let dependent_path = repo.worktree_path("wf/dependent");
        git_in(
            &repo.directory(),
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "wf/dependent",
                dependent_path.to_str().unwrap(),
                &work.to_string(),
            ],
        );
        repo.write(&dependent_path, "d.txt", "d\n");
        let dependent = repo.commit(&dependent_path, "dependent work");
        let MergeOutcome::Merged { commit: second } = repo
            .git
            .merge(
                "main",
                dependent,
                "Merge #dependent",
                &CommitSigning::AsConfigured,
            )
            .unwrap()
        else {
            panic!("expected a merge");
        };
        assert_ne!(first, second);
        assert_eq!(
            repo.git.merge_tree("main", work).unwrap(),
            MergeTree::Landed { commit: first }
        );
    }

    #[test]
    fn conflicting_paths_are_reported_as_they_are_named() {
        let repo = TestRepo::new();
        let name = "\u{e4} b.txt";
        let base = repo.directory();
        repo.write(&base, name, "one\n");
        repo.commit(&base, "add the file");
        let work = repo.work("wf/k3f9", name, "work\n");
        let main = repo.directory();
        repo.write(&main, name, "main\n");
        repo.commit(&main, "conflicting change");
        assert_eq!(
            repo.git.merge_tree("main", work).unwrap(),
            MergeTree::Conflict {
                paths: vec![name.into()]
            }
        );
    }

    /// Rebases `wf/k3f9` onto `main` as the agent `@ada/coder`, without signing.
    fn rebase(repo: &TestRepo) -> Rebase {
        repo.git
            .rebase(
                &repo.worktree_path("wf/k3f9"),
                "main",
                &CommitSigning::Unsigned,
                "@ada/coder",
            )
            .unwrap()
    }

    #[test]
    fn the_branch_is_rebased_onto_the_target_once() {
        let repo = TestRepo::new();
        repo.work("wf/k3f9", "a.txt", "a\n");
        repo.work("wf/k3f9", "c.txt", "c\n");
        let main = repo.directory();
        repo.write(&main, "b.txt", "b\n");
        let target = repo.commit(&main, "add b");
        let worktree = repo.worktree_path("wf/k3f9");

        assert_eq!(rebase(&repo), Rebase::Rebased);
        let head = repo.git.branch_head("wf/k3f9").unwrap();
        assert_eq!(repo.git.commits_ahead("main", head).unwrap(), 2);
        let first = git_in(&worktree, &["rev-parse", "HEAD~1"]);
        assert_eq!(repo.parents(repo.oid_of(&first)), [target.to_string()]);
        assert_eq!(
            git_in(&worktree, &["log", "-2", "--format=%s %cn"]),
            "change c.txt @ada/coder\nchange a.txt @ada/coder"
        );
        assert!(worktree.join("b.txt").is_file());

        assert_eq!(rebase(&repo), Rebase::UpToDate);
        assert_eq!(repo.git.branch_head("wf/k3f9").unwrap(), head);
    }

    #[test]
    fn a_rebase_stops_at_each_conflict_until_the_files_are_resolved() {
        let repo = TestRepo::new();
        let worktree = repo.worktree_path("wf/k3f9");
        repo.work("wf/k3f9", "file.txt", "one\nTWO\nthree\n");
        let work = repo.work("wf/k3f9", "file.txt", "one\nTWO\nTHREE\n");
        let main = repo.directory();
        repo.write(&main, "file.txt", "one\n2\n3\n");
        let target = repo.commit(&main, "conflicting change");
        let conflict = Rebase::Conflict {
            paths: vec!["file.txt".into()],
        };

        assert_eq!(rebase(&repo), conflict);
        assert_eq!(repo.git.branch_head("wf/k3f9").unwrap(), work);
        assert_eq!(
            repo.git.stopped_rebase(&worktree).unwrap(),
            Some(vec!["file.txt".to_string()])
        );
        // Nothing resolved yet: the same files are named again.
        assert_eq!(rebase(&repo), conflict);
        // Given up, the branch is checked out as it was; the next rebase stops at the same place.
        repo.git.abort_rebase(&worktree).unwrap();
        assert_eq!(repo.git.stopped_rebase(&worktree).unwrap(), None);
        assert_eq!(git_in(&worktree, &["rev-parse", "HEAD"]), work.to_string());
        repo.git.abort_rebase(&worktree).unwrap();
        assert_eq!(rebase(&repo), conflict);

        // The first commit is resolved; the second conflicts in turn.
        repo.write(&worktree, "file.txt", "one\nTWO\n3\n");
        assert_eq!(rebase(&repo), conflict);
        repo.write(&worktree, "file.txt", "one\nTWO\nTHREE\n");
        assert_eq!(rebase(&repo), Rebase::Rebased);
        assert_eq!(repo.git.stopped_rebase(&worktree).unwrap(), None);
        let head = repo.git.branch_head("wf/k3f9").unwrap();
        assert_eq!(repo.git.commits_ahead("main", head).unwrap(), 2);
        assert_eq!(
            git_in(&worktree, &["merge-base", "main", "HEAD"]),
            target.to_string()
        );
        assert_eq!(
            std::fs::read_to_string(worktree.join("file.txt")).unwrap(),
            "one\nTWO\nTHREE\n"
        );
    }

    #[test]
    fn a_branch_with_uncommitted_changes_is_not_rebased() {
        let repo = TestRepo::new();
        let work = repo.work("wf/k3f9", "a.txt", "a\n");
        let main = repo.directory();
        repo.write(&main, "b.txt", "b\n");
        repo.commit(&main, "add b");
        let worktree = repo.worktree_path("wf/k3f9");
        repo.write(&worktree, "file.txt", "uncommitted\n");
        assert_eq!(
            rebase(&repo),
            Rebase::DirtyCheckout(DirtyCheckout {
                worktree: worktree.clone(),
                paths: vec!["file.txt".into()],
            })
        );
        assert_eq!(repo.git.branch_head("wf/k3f9").unwrap(), work);
        assert_eq!(
            std::fs::read_to_string(worktree.join("file.txt")).unwrap(),
            "uncommitted\n"
        );
    }

    #[test]
    fn rebased_commits_are_signed_as_the_signing_says() {
        let repo = TestRepo::new();
        let main = repo.directory();
        repo.sign_with_a_fake_signer();
        let worktree = repo.worktree_path("wf/k3f9");
        repo.git
            .prepare_worktree("wf/k3f9", &worktree, "main")
            .unwrap();
        repo.write(&worktree, "a.txt", "a\n");
        git_in(&worktree, &["add", "a.txt"]);
        git_in(
            &worktree,
            &["-c", "commit.gpgsign=false", "commit", "--quiet", "-m", "a"],
        );
        let signed = || git_in(&worktree, &["cat-file", "commit", "HEAD"]).contains("gpgsig");
        for (signing, expected) in [
            (CommitSigning::Unsigned, false),
            (CommitSigning::AsConfigured, true),
        ] {
            repo.write(&main, "b.txt", &format!("{signing:?}\n"));
            repo.commit(&main, "move main");
            let rebased = repo
                .git
                .rebase(&worktree, "main", &signing, "@ada/coder")
                .unwrap();
            assert_eq!(rebased, Rebase::Rebased);
            assert_eq!(signed(), expected, "{signing:?}");
        }
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
        assert_eq!(
            repo.git.changed_paths(&main).unwrap(),
            ["file with space.txt", "second.txt"]
        );
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

    #[test]
    fn a_tree_is_checked_out_clean_and_build_output_stays() {
        let repo = TestRepo::new();
        let main = repo.directory();
        repo.write(&main, ".gitignore", "target/\n");
        let first = repo.commit(&main, "ignore build output");
        repo.write(&main, "file.txt", "changed\n");
        repo.write(&main, "added.txt", "added\n");
        repo.commit(&main, "second");
        let tree: GitOid = git_in(&main, &["rev-parse", "HEAD^{tree}"])
            .parse()
            .unwrap();
        let CommitId::Git(first) = first;

        let path = repo.worktree_path("check");
        repo.git.check_out_tree(&path, first).unwrap();
        assert_eq!(
            std::fs::read_to_string(path.join("file.txt")).unwrap(),
            "one\ntwo\nthree\n"
        );
        assert!(!path.join("added.txt").exists());
        assert_eq!(git_in(&path, &["rev-parse", "HEAD"]), first.to_string());
        assert_eq!(git_in(&path, &["status", "--porcelain"]), "");

        repo.write(&path, "file.txt", "edited\n");
        repo.write(&path, "stray.txt", "left behind\n");
        std::fs::create_dir(path.join("target")).unwrap();
        repo.write(&path, "target/build.o", "cached\n");
        repo.git.check_out_tree(&path, tree).unwrap();

        assert_eq!(
            std::fs::read_to_string(path.join("file.txt")).unwrap(),
            "changed\n"
        );
        assert_eq!(
            std::fs::read_to_string(path.join("added.txt")).unwrap(),
            "added\n"
        );
        assert!(!path.join("stray.txt").exists());
        assert!(path.join("target/build.o").exists());

        repo.git.check_out_tree(&path, first).unwrap();
        assert!(!path.join("added.txt").exists());
        assert_eq!(git_in(&path, &["rev-parse", "HEAD"]), first.to_string());
        assert_eq!(git_in(&path, &["status", "--porcelain"]), "");

        std::fs::remove_dir_all(&path).unwrap();
        repo.git.check_out_tree(&path, tree).unwrap();
        assert_eq!(
            std::fs::read_to_string(path.join("added.txt")).unwrap(),
            "added\n"
        );
        assert_eq!(git_in(&path, &["status", "--porcelain"]), "");
    }
}

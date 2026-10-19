use std::ffi::OsStr;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use wf_core::session::{CommitId, GitOid};

use crate::diff;
use crate::{
    CommitSigning, DirtyCheckout, MergeOutcome, MergeTree, Pager, Rebase, StoppedRebase, VcsError,
    Work,
};

/// How long a `packed-refs.lock` may stay before it counts as left behind. Age alone decides,
/// without looking for a git process that holds it: git keeps the lock only while it rewrites
/// `packed-refs` (seconds even for a very large repository), so a limit this far above that
/// never removes a live lock, and it needs no platform specific process lookup.
const STALE_LOCK_AGE: Duration = Duration::from_secs(120);

const MINIMUM_VERSION: (u32, u32) = (2, 38);

/// `LESS` for a `less` that stays open on a diff shorter than the screen: the principal's `less`
/// config, or git's default `FRX` when it is not set, followed by `-+F`, which undoes `F`
/// wherever it was set.
fn less_staying_open(less: Option<&str>) -> String {
    format!("{} -+F", less.unwrap_or("FRX"))
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
    /// The main checkout.
    repository: PathBuf,
    /// The git directory the checkouts share, where the references are.
    git_directory: PathBuf,
}

/// `git`, kept from running code that an agent could have put in the repository's config: hooks
/// and an fsmonitor run outside the agent's sandbox, with the privileges of whoever runs git.
///
/// Nor does git pack the references behind a session's back: `gc --auto` after a porcelain
/// command such as the rebase at submit would remove the loose file of the branch that a
/// sandboxed session can only rewrite, not create. The daemon never runs maintenance itself; the
/// references stay as they are until the principal packs them, and every turn of a work session
/// starts with `make_branch_loose`.
fn git() -> Command {
    let mut command = Command::new("git");
    command.args([
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "core.fsmonitor=false",
        "-c",
        "gc.auto=0",
        "-c",
        "maintenance.auto=false",
    ]);
    // A repository the caller's environment points at would win over the one asked for.
    command
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE");
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
    /// The arguments git ran with, to say which command failed.
    command: String,
    success: bool,
    stdout: String,
    stderr: String,
}

impl Finished {
    /// The output of git, or the error of the command if it failed.
    fn ok(self) -> Result<String, VcsError> {
        if self.success {
            Ok(self.stdout)
        } else {
            Err(self.error())
        }
    }

    /// The error of the command, whether or not it failed.
    fn error(self) -> VcsError {
        VcsError::Git {
            command: self.command,
            stderr: self.stderr.trim().to_string(),
        }
    }
}

/// Runs git in `directory` with `environment` set on top. Everything that runs git comes
/// through here, so that all of it runs the same way.
fn spawn<I, S>(
    directory: &Path,
    environment: &[(&str, &str)],
    args: I,
) -> Result<Finished, VcsError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<S> = args.into_iter().collect();
    let output = git()
        .args(&args)
        .envs(environment.iter().copied())
        .current_dir(directory)
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(VcsError::Spawn)?;
    Ok(Finished {
        command: describe(&args),
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// The arguments of a command as one line, without the `-c` config that only sets how it runs.
fn describe<S: AsRef<OsStr>>(args: &[S]) -> String {
    let mut words = args.iter().map(|arg| arg.as_ref().to_string_lossy());
    let mut described = Vec::new();
    while let Some(word) = words.next() {
        if word == "-c" && described.is_empty() {
            words.next();
        } else {
            described.push(word);
        }
    }
    described.join(" ")
}

/// Checks the git version, once per process.
fn check_version(directory: &Path) -> Result<(), VcsError> {
    static CHECKED: OnceLock<()> = OnceLock::new();
    if CHECKED.get().is_some() {
        return Ok(());
    }
    let version = spawn(directory, &[], ["--version"])?.ok()?;
    let text = version.trim();
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
    let _ = CHECKED.set(());
    Ok(())
}

/// What `command` printed as a `T`.
fn parse_output<T: FromStr>(command: &str, output: &str) -> Result<T, VcsError> {
    output
        .trim()
        .parse()
        .map_err(|_| VcsError::UnexpectedOutput {
            command: command.into(),
            output: output.trim().into(),
        })
}

impl GitCli {
    /// Opens the repository that contains `directory`, after checking the git version once.
    pub fn open(directory: &Path) -> Result<Self, VcsError> {
        check_version(directory)?;
        // Whichever worktree `directory` is in, the first one listed is the main checkout, which
        // stays, wherever its git directory is.
        let listing = spawn(directory, &[], ["worktree", "list", "--porcelain", "-z"])?;
        if !listing.success {
            return Err(VcsError::NotARepository(directory.to_path_buf()));
        }
        let main = parse_worktrees(&listing.stdout).into_iter().next();
        let Some(Worktree {
            path: repository, ..
        }) = main
        else {
            return Err(VcsError::UnexpectedOutput {
                command: listing.command,
                output: listing.stdout,
            });
        };
        let common = spawn(
            directory,
            &[],
            ["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?
        .ok()?;
        Ok(Self {
            repository,
            git_directory: PathBuf::from(common.trim()),
        })
    }

    pub fn repository(&self) -> &Path {
        &self.repository
    }

    /// The top of the checkout that contains `directory`: the main checkout or a worktree.
    pub fn checkout_root(directory: &Path) -> Result<PathBuf, VcsError> {
        let top = spawn(directory, &[], ["rev-parse", "--show-toplevel"])?;
        if !top.success {
            return Err(VcsError::NotARepository(directory.to_path_buf()));
        }
        Ok(PathBuf::from(top.stdout.trim()))
    }

    /// Runs git in `directory` with `environment` set on top, after clearing a lock left behind.
    fn run<I, S>(
        &self,
        directory: &Path,
        environment: &[(&str, &str)],
        args: I,
    ) -> Result<Finished, VcsError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.remove_stale_packed_refs_lock();
        spawn(directory, environment, args)
    }

    /// Removes a `packed-refs.lock` that has been there for longer than git holds it. A session
    /// may create that file (its sandbox allows it, so that git can fail quietly), and a
    /// lock left behind makes every git that deletes or packs a reference fail.
    fn remove_stale_packed_refs_lock(&self) {
        let lock = self.git_directory.join("packed-refs.lock");
        let age = std::fs::metadata(&lock)
            .and_then(|metadata| metadata.modified())
            // A modification time in the future cannot come from git, so it counts as stale.
            .map(|modified| SystemTime::now().duration_since(modified).ok());
        if age.is_ok_and(|age| age.is_none_or(|age| age > STALE_LOCK_AGE)) {
            let _ = std::fs::remove_file(&lock);
        }
    }

    /// Whether the git config asks for commits to be signed (`commit.gpgsign`).
    fn signs_commits(&self) -> Result<bool, VcsError> {
        let configured = self.run(
            &self.repository,
            &[],
            ["config", "--type=bool", "--get", "commit.gpgsign"],
        )?;
        Ok(configured.success && configured.stdout.trim() == "true")
    }

    fn oid(&self, command: &str, text: &str) -> Result<CommitId, VcsError> {
        parse_output(command, text).map(CommitId::Git)
    }

    /// Adds the worktree of `branch` at `path`, unless it is there.
    fn add_worktree_if_missing(
        &self,
        branch: &str,
        path: &Path,
        target: &str,
    ) -> Result<(), VcsError> {
        self.prune_worktrees()?;
        let existing = self.worktrees()?;
        if let Some(worktree) = existing
            .iter()
            .find(|worktree| same_path(&worktree.path, path))
        {
            return if worktree.branch.as_deref() == Some(branch) {
                Ok(())
            } else {
                Err(VcsError::Git {
                    command: format!("worktree add {}", path.display()),
                    stderr: format!("the worktree there does not have `{branch}` checked out"),
                })
            };
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if self.branch_exists(branch)? {
            let words = ["worktree", "add"].map(OsStr::new);
            let args = words
                .into_iter()
                .chain([path.as_os_str(), OsStr::new(branch)]);
            self.run(&self.repository, &[], args)?.ok()?;
        } else {
            if !self.branch_exists(target)? {
                return Err(VcsError::NoBranch(target.into()));
            }
            // The full name, or a tag of the same name would be where the branch starts.
            let start = format!("refs/heads/{target}");
            let words = ["worktree", "add", "-b", branch].map(OsStr::new);
            let args = words
                .into_iter()
                .chain([path.as_os_str(), OsStr::new(&start)]);
            self.run(&self.repository, &[], args)?.ok()?;
        }
        Ok(())
    }

    fn branch_exists(&self, branch: &str) -> Result<bool, VcsError> {
        let reference = format!("refs/heads/{branch}");
        let found = self.run(
            &self.repository,
            &[],
            ["rev-parse", "--verify", "--quiet", &reference],
        )?;
        Ok(found.success)
    }

    /// The worktrees of the repository: where each is and which branch it has checked out.
    fn worktrees(&self) -> Result<Vec<Worktree>, VcsError> {
        let listing = self
            .run(
                &self.repository,
                &[],
                ["worktree", "list", "--porcelain", "-z"],
            )?
            .ok()?;
        Ok(parse_worktrees(&listing))
    }

    /// The tracked files edited in `worktree`. Untracked files do not count, such as the marker
    /// `wf init` leaves: git itself refuses to overwrite one, and nothing else is at risk.
    fn changed_paths(&self, worktree: &Path) -> Result<Vec<String>, VcsError> {
        let status = self
            .run(
                worktree,
                &[],
                ["status", "--porcelain", "-z", "--untracked-files=no"],
            )?
            .ok()?;
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
        let checkout = self
            .worktrees()?
            .into_iter()
            .find(|worktree| worktree.branch.as_deref() == Some(target));
        let moved = |this: &Self| -> Result<bool, VcsError> {
            Ok(this.rev_parse(&format!("refs/heads/{target}"))? != *old)
        };
        let finished = match checkout {
            Some(worktree) => {
                let paths = self.changed_paths(&worktree.path)?;
                if !paths.is_empty() {
                    return Ok(Advance::Dirty(DirtyCheckout {
                        worktree: worktree.path,
                        paths,
                    }));
                }
                self.run(
                    &worktree.path,
                    &[],
                    ["merge", "--ff-only", "--quiet", &new.to_string()],
                )?
            }
            None => self.run(
                &self.repository,
                &[],
                [
                    "update-ref",
                    &format!("refs/heads/{target}"),
                    &new.to_string(),
                    &old.to_string(),
                ],
            )?,
        };
        match finished.ok() {
            Ok(_) => Ok(Advance::Done),
            Err(_) if moved(self)? => Ok(Advance::Moved),
            Err(error) => Err(error),
        }
    }

    /// The commit that brought `commit` into `head`, when it is there already: the merge with
    /// `commit` as a parent, or `commit` itself when it came in without a merge.
    fn landed(&self, commit: CommitId, head: CommitId) -> Result<Option<CommitId>, VcsError> {
        let (commit, head) = (commit.to_string(), head.to_string());
        let ancestor = self.run(
            &self.repository,
            &[],
            ["merge-base", "--is-ancestor", &commit, &head],
        )?;
        if !ancestor.success {
            return Ok(None);
        }
        let range = format!("{commit}..{head}");
        let listed = self
            .run(
                &self.repository,
                &[],
                [
                    "rev-list",
                    "--first-parent",
                    "--ancestry-path",
                    "--parents",
                    &range,
                ],
            )?
            .ok()?;
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
        let output = self
            .run(&self.repository, &[], ["rev-parse", "--verify", &revision])?
            .ok()?;
        self.oid("rev-parse", &output)
    }
}

struct Worktree {
    path: PathBuf,
    /// The short name of the branch checked out, if one is.
    branch: Option<String>,
}

/// The worktrees in the output of `git worktree list --porcelain -z`: NUL ends each attribute
/// and an empty one ends each worktree, and paths come as they are.
fn parse_worktrees(listing: &str) -> Vec<Worktree> {
    let mut worktrees = Vec::new();
    let mut path = None;
    let mut branch = None;
    for attribute in listing.split('\0') {
        if let Some(found) = attribute.strip_prefix("worktree ") {
            path = Some(PathBuf::from(found));
        } else if let Some(found) = attribute.strip_prefix("branch ") {
            branch = found.strip_prefix("refs/heads/").map(str::to_string);
        } else if attribute.is_empty()
            && let Some(path) = path.take()
        {
            worktrees.push(Worktree {
                path,
                branch: branch.take(),
            });
        }
    }
    worktrees
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

impl GitCli {
    /// Makes sure `branch` is a loose reference, a file under `refs/heads`. Packing the
    /// references (`git gc`) removes the file and its directories, and a session that may only
    /// write its own branch's file cannot create them again.
    pub fn make_branch_loose(&self, branch: &str) -> Result<(), VcsError> {
        let reference = self.git_directory.join("refs/heads").join(branch);
        if reference.is_file() {
            return Ok(());
        }
        let commit = self.branch_head(branch)?;
        if let Some(parent) = reference.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // `git update-ref` writes nothing when the value is the packed one. This follows git's
        // own protocol: the lock is created exclusively and renamed onto the reference.
        let mut lock_name = reference.clone().into_os_string();
        lock_name.push(".lock");
        let lock = PathBuf::from(lock_name);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)?;
        file.write_all(format!("{commit}\n").as_bytes())?;
        std::fs::rename(&lock, &reference).inspect_err(|_| {
            let _ = std::fs::remove_file(&lock);
        })?;
        Ok(())
    }

    /// Makes `branch` (created at the tip of `target` when it does not exist yet) and checks it
    /// out in a worktree at `path`. Does nothing if that worktree is already there.
    pub fn prepare_worktree(
        &self,
        branch: &str,
        path: &Path,
        target: &str,
    ) -> Result<(), VcsError> {
        self.add_worktree_if_missing(branch, path, target)?;
        self.make_branch_loose(branch)
    }

    /// Checks `commit` out, detached, in a new worktree at `path`, for a reviewer.
    pub fn detached_worktree(&self, path: &Path, commit: CommitId) -> Result<(), VcsError> {
        self.prune_worktrees()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let words = ["worktree", "add", "--detach"].map(OsStr::new);
        let commit = commit.to_string();
        let args = words
            .into_iter()
            .chain([path.as_os_str(), OsStr::new(&commit)]);
        self.run(&self.repository, &[], args)?.ok()?;
        Ok(())
    }

    /// Makes the worktree at `path` hold the files of `tree`, a tree or a commit, and nothing
    /// else: tracked files are reset and untracked ones removed. Ignored files, such as build
    /// output, stay. A commit is checked out with HEAD detached at it. The worktree is made,
    /// detached, when it is not there yet.
    pub fn check_out_tree(&self, path: &Path, tree: GitOid) -> Result<(), VcsError> {
        // Prune first, so a worktree whose directory was deleted is made again.
        self.prune_worktrees()?;
        let exists = self
            .worktrees()?
            .iter()
            .any(|worktree| same_path(&worktree.path, path));
        if !exists {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // The files come from the checkout below, which knows whether `tree` is a commit.
            let words = ["worktree", "add", "--detach", "--no-checkout"].map(OsStr::new);
            let args = words.into_iter().chain([path.as_os_str()]);
            self.run(&self.repository, &[], args)?.ok()?;
        }
        let tree = tree.to_string();
        let kind = self
            .run(&self.repository, &[], ["cat-file", "-t", &tree])?
            .ok()?;
        if kind.trim() == "commit" {
            // HEAD names the commit, so `git describe` and `git status` see it too.
            let args = ["checkout", "--quiet", "--detach", "--force", &tree];
            self.run(path, &[], args)?.ok()?;
        } else {
            // A bare tree has no commit for HEAD to name; only the index and files change.
            self.run(path, &[], ["read-tree", "-u", "--reset", &tree])?
                .ok()?;
        }
        self.run(path, &[], ["clean", "-ffd", "--quiet"])?.ok()?;
        Ok(())
    }

    /// Removes the worktree at `path` together with whatever is uncommitted in it.
    pub fn remove_worktree(&self, path: &Path) -> Result<(), VcsError> {
        let words = ["worktree", "remove", "--force"].map(OsStr::new);
        let args = words.into_iter().chain([path.as_os_str()]);
        self.run(&self.repository, &[], args)?.ok()?;
        Ok(())
    }

    /// Forgets the worktrees whose directories are gone.
    pub fn prune_worktrees(&self) -> Result<(), VcsError> {
        self.run(&self.repository, &[], ["worktree", "prune"])?
            .ok()?;
        Ok(())
    }

    pub fn delete_branch(&self, branch: &str) -> Result<(), VcsError> {
        self.run(&self.repository, &[], ["branch", "-D", branch])?
            .ok()?;
        Ok(())
    }

    /// The commit a branch points at.
    pub fn branch_head(&self, branch: &str) -> Result<CommitId, VcsError> {
        let found = self.run(
            &self.repository,
            &[],
            [
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}^{{commit}}"),
            ],
        )?;
        if !found.success {
            return Err(VcsError::NoBranch(branch.into()));
        }
        self.oid("rev-parse", &found.stdout)
    }

    /// How many commits `commit` has that `target` does not.
    pub fn commits_ahead(&self, target: &str, commit: CommitId) -> Result<usize, VcsError> {
        let range = format!("refs/heads/{target}..{commit}");
        let output = self
            .run(&self.repository, &[], ["rev-list", "--count", &range])?
            .ok()?;
        parse_output("rev-list --count", &output)
    }

    /// What `work` changes, as [`Work`] says, with `args` added to `git diff`, such as `--stat`
    /// or `-- src/`.
    pub fn diff(&self, work: &Work, args: &[String]) -> Result<String, VcsError> {
        let revisions = diff_revisions(work);
        self.run(
            &self.repository,
            &[],
            ["diff"]
                .into_iter()
                .chain(revisions.iter().map(String::as_str))
                .chain(args.iter().map(String::as_str)),
        )?
        .ok()
    }

    /// The same diff, written to the terminal by git itself, so its pager and diff settings
    /// apply as for `git diff`, except where `pager` says otherwise.
    pub fn show_diff(&self, work: &Work, args: &[String], pager: Pager) -> Result<(), VcsError> {
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

    /// Checks `commit` out, detached, in the main checkout, which has to be clean.
    pub fn checkout_detached(&self, commit: CommitId) -> Result<(), VcsError> {
        if !self.is_clean(&self.repository)? {
            return Err(VcsError::DirtyCheckout(self.repository.clone()));
        }
        let args = ["checkout", "--quiet", "--detach", &commit.to_string()];
        self.run(&self.repository, &[], args)?.ok()?;
        Ok(())
    }

    /// Goes back to what the main checkout had before the last checkout: `git checkout -`.
    pub fn checkout_previous(&self) -> Result<(), VcsError> {
        if !self.is_clean(&self.repository)? {
            return Err(VcsError::DirtyCheckout(self.repository.clone()));
        }
        self.run(&self.repository, &[], ["checkout", "--quiet", "-"])?
            .ok()?;
        Ok(())
    }

    /// What merging `commit` into `target` makes, without committing it or moving `target`.
    pub fn merge_tree(&self, target: &str, commit: CommitId) -> Result<MergeTree, VcsError> {
        let base = self.branch_head(target)?;
        // Merged already, by a merge whose event was never written: merging again would only add
        // an empty merge commit.
        if let Some(landed) = self.landed(commit, base)? {
            return Ok(MergeTree::Landed { commit: landed });
        }
        let merged = self.run(
            &self.repository,
            &[],
            [
                "merge-tree",
                "--write-tree",
                "--name-only",
                "-z",
                &base.to_string(),
                &commit.to_string(),
            ],
        )?;
        // With `-z` every field ends in NUL and paths come as they are, not C-quoted: the tree,
        // the conflicted paths, an empty field, then the informational messages.
        let mut fields = merged.stdout.split('\0');
        let tree = fields.next().unwrap_or_default();
        if merged.success {
            let tree = parse_output("merge-tree --write-tree", tree)?;
            return Ok(MergeTree::Clean { base, tree });
        }
        // A failure that names no tree is git failing, not a conflict.
        if tree.trim().is_empty() {
            return Err(merged.error());
        }
        let paths = fields
            .take_while(|field| !field.is_empty())
            .map(str::to_string)
            .collect();
        Ok(MergeTree::Conflict { paths })
    }

    /// Commits `tree`, made by [`GitCli::merge_tree`], as the merge of `commit` into `target`
    /// at `base`, signed as `signing` says, and moves `target` to it. Never a conflict: the
    /// outcome is the merge, a checkout of `target` with changes, or `target` no longer at `base`.
    pub fn commit_merge(
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
        let mut args = signing.arguments();
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
        let made = self.run(&self.repository, &[], &args)?.ok()?;
        let new = self.oid("commit-tree", &made)?;
        Ok(match self.advance(target, &base, &new)? {
            Advance::Done => MergeOutcome::Merged { commit: new },
            Advance::Dirty(dirty) => MergeOutcome::DirtyCheckout(dirty),
            Advance::Moved => MergeOutcome::TargetMoved,
        })
    }

    /// The tree of `commit`.
    pub fn tree_of(&self, commit: CommitId) -> Result<GitOid, VcsError> {
        let revision = format!("{commit}^{{tree}}");
        let output = self
            .run(&self.repository, &[], ["rev-parse", "--verify", &revision])?
            .ok()?;
        parse_output("rev-parse", &output)
    }

    /// Rebases the branch checked out in `worktree` onto `target`, the rewritten commits signed as
    /// `signing` says and committed by `committer`. A rebase stopped there earlier is not begun
    /// again: the conflicting files are staged once they are resolved, and the rebase goes on. A
    /// text file with content from both sides is resolved when no conflict markers are left in
    /// it; any other conflict (binary, or a file one side deleted) only when the index says so,
    /// that is, when whoever works there has staged the outcome with `git add` or `git rm`.
    pub fn rebase(
        &self,
        worktree: &Path,
        target: &str,
        signing: &CommitSigning,
        committer: &str,
    ) -> Result<Rebase, VcsError> {
        let mut args = signing.arguments();
        match self.stopped_rebase(worktree)? {
            StoppedRebase::Conflict { .. } | StoppedRebase::Stopped => {
                let unresolved: Vec<String> = self
                    .conflicts(worktree)?
                    .into_iter()
                    .filter(|conflict| !conflict.is_resolved_in(worktree))
                    .map(|conflict| conflict.path)
                    .collect();
                if !unresolved.is_empty() {
                    return Ok(Rebase::Conflict { paths: unresolved });
                }
                self.run(worktree, &[], ["add", "--update"])?.ok()?;
                args.extend(["rebase", "--continue"].map(str::to_string));
            }
            StoppedRebase::NotStopped => {
                if !self.branch_exists(target)? {
                    return Err(VcsError::NoBranch(target.into()));
                }
                let reference = format!("refs/heads/{target}");
                let contained = self.run(
                    worktree,
                    &[],
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
                        // The apply backend, which a principal's config may choose, leaves
                        // `rebase-apply` and conflicts that read differently.
                        "--merge",
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
        let rebased = self.run(
            worktree,
            &[("GIT_COMMITTER_NAME", committer), ("GIT_EDITOR", "true")],
            &args,
        )?;
        if rebased.success {
            return Ok(Rebase::Rebased);
        }
        match self.stopped_rebase(worktree)? {
            StoppedRebase::Conflict { paths } => Ok(Rebase::Conflict { paths }),
            // Git refused before it began, or stopped for something other than a conflict.
            StoppedRebase::Stopped | StoppedRebase::NotStopped => Err(rebased.error()),
        }
    }

    /// Whether a rebase is stopped in `worktree`, and at which files.
    pub fn stopped_rebase(&self, worktree: &Path) -> Result<StoppedRebase, VcsError> {
        let mut stopped = false;
        for state in ["rebase-merge", "rebase-apply"] {
            let args = ["rev-parse", "--path-format=absolute", "--git-path", state];
            let directory = self.run(worktree, &[], args)?.ok()?;
            stopped |= Path::new(directory.trim()).is_dir();
        }
        if !stopped {
            return Ok(StoppedRebase::NotStopped);
        }
        let paths: Vec<String> = self
            .conflicts(worktree)?
            .into_iter()
            .map(|conflict| conflict.path)
            .collect();
        Ok(if paths.is_empty() {
            StoppedRebase::Stopped
        } else {
            StoppedRebase::Conflict { paths }
        })
    }

    /// Gives up the rebase stopped in `worktree`, if one is, which checks the branch out again
    /// as it was before the rebase.
    pub fn abort_rebase(&self, worktree: &Path) -> Result<(), VcsError> {
        if self.stopped_rebase(worktree)? != StoppedRebase::NotStopped {
            self.run(worktree, &[], ["rebase", "--abort"])?.ok()?;
        }
        Ok(())
    }

    /// Whether `work` has commits of its own: merged work has; unmerged work once its commit is
    /// ahead of the target, which a branch just started is not.
    pub fn has_commits(&self, work: &Work) -> Result<bool, VcsError> {
        match work {
            Work::Unmerged { target, commit } => Ok(self.commits_ahead(target, *commit)? > 0),
            Work::Merged { .. } => Ok(true),
        }
    }

    /// The same diff as data: the files it changes, their hunks and their lines, whatever the
    /// git config says about how a diff looks.
    pub fn file_diffs(&self, work: &Work) -> Result<Vec<diff::FileDiff>, VcsError> {
        let args = diff::ARGS.map(String::from);
        Ok(diff::parse(&self.diff(work, &args)?))
    }
}

/// A file the index still has unmerged, with the sides the rebase could not combine.
struct Conflict {
    path: String,
    /// The target has a version of the file (index stage 2).
    in_target: bool,
    /// The rebased commit has a version of the file (index stage 3). When only one of the two
    /// has, the other deleted it.
    in_commit: bool,
}

impl Conflict {
    /// Whether the file in `worktree` is resolved though the index still has it unmerged. Only a
    /// text file with both sides is: git leaves markers in it, and once they are gone it has been
    /// edited. A binary file or a file one side deleted holds no markers either way, so its
    /// content says nothing; it is resolved when it is staged, and then it is not unmerged.
    fn is_resolved_in(&self, worktree: &Path) -> bool {
        if !(self.in_target && self.in_commit) {
            return false;
        }
        match std::fs::read(worktree.join(&self.path)) {
            Ok(text) => !text.contains(&0) && !has_conflict_markers(&text),
            Err(_) => false,
        }
    }
}

/// Whether `text` has the lines git marks a conflict with.
fn has_conflict_markers(text: &[u8]) -> bool {
    text.split(|byte| *byte == b'\n')
        .any(|line| line.starts_with(b"<<<<<<<") || line.starts_with(b">>>>>>>"))
}

impl GitCli {
    /// The files the index has unmerged in `worktree`, relative to it.
    fn conflicts(&self, worktree: &Path) -> Result<Vec<Conflict>, VcsError> {
        let listing = self
            .run(worktree, &[], ["ls-files", "--unmerged", "-z"])?
            .ok()?;
        // Each entry reads `<mode> <object> <stage>\t<path>`: stage 2 is the target's version,
        // stage 3 the rebased commit's.
        let mut conflicts: Vec<Conflict> = Vec::new();
        for entry in listing.split('\0').filter(|entry| !entry.is_empty()) {
            let Some((meta, path)) = entry.split_once('\t') else {
                return Err(VcsError::UnexpectedOutput {
                    command: "ls-files".into(),
                    output: entry.into(),
                });
            };
            let stage = meta.rsplit(' ').next().unwrap_or_default();
            if conflicts.last().is_none_or(|last| last.path != path) {
                conflicts.push(Conflict {
                    path: path.to_string(),
                    in_target: false,
                    in_commit: false,
                });
            }
            let last = conflicts.last_mut().expect("pushed above");
            last.in_target |= stage == "2";
            last.in_commit |= stage == "3";
        }
        Ok(conflicts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{TestRepository as TestRepo, git_in};

    /// Merges `commit` into `target` with a merge commit that has both as parents, signed as
    /// `signing` says.
    impl GitCli {
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
    }

    #[test]
    fn less_staying_open_drops_quitting_on_one_screen() {
        assert_eq!(less_staying_open(None), "FRX -+F");
        assert_eq!(less_staying_open(Some("FRSX")), "FRSX -+F");
        assert_eq!(less_staying_open(Some("-R -i")), "-R -i -+F");
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
    fn a_branch_starts_at_the_target_branch_not_at_a_tag_of_its_name() {
        let repo = TestRepo::new();
        let first = repo.main();
        repo.write(&repo.directory(), "file.txt", "changed\n");
        let second = repo.commit(&repo.directory(), "second");
        git_in(
            &repo.directory(),
            &["-c", "tag.gpgsign=false", "tag", "main", &first.to_string()],
        );
        let path = repo.worktree_path("wf/k3f9");
        repo.git.prepare_worktree("wf/k3f9", &path, "main").unwrap();
        assert_eq!(repo.git.branch_head("wf/k3f9").unwrap(), second);
    }

    #[test]
    fn a_separate_git_directory_is_found_from_a_worktree() {
        let root = tempfile::TempDir::new().unwrap();
        let repository = root.path().join("repo");
        std::fs::create_dir(&repository).unwrap();
        git_in(
            &repository,
            &[
                "init",
                "--quiet",
                "--initial-branch=main",
                "--separate-git-dir",
                root.path().join("elsewhere").to_str().unwrap(),
            ],
        );
        git_in(&repository, &["config", "user.name", "Test"]);
        git_in(&repository, &["config", "user.email", "test@example.com"]);
        std::fs::write(repository.join("file.txt"), "one\n").unwrap();
        git_in(&repository, &["add", "--all"]);
        git_in(&repository, &["commit", "--quiet", "-m", "first"]);
        let git = GitCli::open(&repository).unwrap();
        let path = root.path().join("worktrees/wf/k3f9");
        git.prepare_worktree("wf/k3f9", &path, "main").unwrap();
        // Git lists the git directory as the main worktree when it is not in `.git`.
        let opened = GitCli::open(&path).unwrap();
        assert_eq!(opened.repository(), git.repository());
        assert_eq!(opened.git_directory, git.git_directory);
        assert_eq!(
            opened.git_directory.canonicalize().unwrap(),
            root.path().join("elsewhere").canonicalize().unwrap()
        );
    }

    #[test]
    fn packed_refs_are_made_loose_again_so_a_session_can_commit() {
        let repo = TestRepo::new();
        let path = repo.worktree_path("wf/k3f9");
        repo.git.prepare_worktree("wf/k3f9", &path, "main").unwrap();
        git_in(&repo.directory(), &["pack-refs", "--all"]);
        let reference = repo.directory().join(".git/refs/heads/wf/k3f9");
        assert!(!reference.exists());
        repo.git.prepare_worktree("wf/k3f9", &path, "main").unwrap();
        assert!(reference.is_file());
        // What the sandbox allows: the file and its lock, and no new directory.
        std::fs::remove_dir_all(repo.directory().join(".git/logs")).ok();
        let commit = repo.work("wf/k3f9", "a.txt", "a");
        assert_eq!(repo.git.branch_head("wf/k3f9").unwrap(), commit);
    }

    #[test]
    fn a_packed_refs_lock_left_behind_does_not_block_deleting_a_branch() {
        let repo = TestRepo::new();
        repo.work("wf/gone", "a.txt", "a");
        git_in(&repo.directory(), &["pack-refs", "--all"]);
        let lock = repo.directory().join(".git/packed-refs.lock");
        let left_behind = std::fs::File::create(&lock).unwrap();
        left_behind
            .set_modified(SystemTime::now() - Duration::from_secs(600))
            .unwrap();
        repo.git
            .remove_worktree(&repo.worktree_path("wf/gone"))
            .unwrap();
        repo.git.delete_branch("wf/gone").unwrap();
        assert!(!lock.exists());
    }

    #[test]
    fn a_packed_refs_lock_dated_in_the_future_is_removed() {
        let repo = TestRepo::new();
        repo.work("wf/gone", "a.txt", "a");
        git_in(&repo.directory(), &["pack-refs", "--all"]);
        let lock = repo.directory().join(".git/packed-refs.lock");
        let left_behind = std::fs::File::create(&lock).unwrap();
        left_behind
            .set_modified(SystemTime::now() + Duration::from_secs(86_400))
            .unwrap();
        repo.git
            .remove_worktree(&repo.worktree_path("wf/gone"))
            .unwrap();
        repo.git.delete_branch("wf/gone").unwrap();
        assert!(!lock.exists());
    }

    #[test]
    fn a_fresh_packed_refs_lock_is_left_alone() {
        let repo = TestRepo::new();
        let lock = repo.directory().join(".git/packed-refs.lock");
        std::fs::File::create(&lock).unwrap();
        repo.git.remove_stale_packed_refs_lock();
        assert!(lock.exists());
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
        assert_eq!(repo.parents(repo.commit_id(&first, &repo.directory())), [target.to_string()]);
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
            StoppedRebase::Conflict {
                paths: vec!["file.txt".to_string()]
            }
        );
        // Nothing resolved yet: the same files are named again.
        assert_eq!(rebase(&repo), conflict);
        // Given up, the branch is checked out as it was; the next rebase stops at the same place.
        repo.git.abort_rebase(&worktree).unwrap();
        assert_eq!(
            repo.git.stopped_rebase(&worktree).unwrap(),
            StoppedRebase::NotStopped
        );
        assert_eq!(git_in(&worktree, &["rev-parse", "HEAD"]), work.to_string());
        repo.git.abort_rebase(&worktree).unwrap();
        assert_eq!(rebase(&repo), conflict);

        // The first commit is resolved; the second conflicts in turn.
        repo.write(&worktree, "file.txt", "one\nTWO\n3\n");
        assert_eq!(rebase(&repo), conflict);
        repo.write(&worktree, "file.txt", "one\nTWO\nTHREE\n");
        assert_eq!(rebase(&repo), Rebase::Rebased);
        assert_eq!(
            repo.git.stopped_rebase(&worktree).unwrap(),
            StoppedRebase::NotStopped
        );
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

    /// Leaves `wf/k3f9` rebased onto a `main` that conflicts in `name`, which is binary or deleted.
    fn conflict_in(repo: &TestRepo, name: &str, work: &[u8], main_side: Option<&[u8]>) {
        let base = repo.directory();
        std::fs::write(base.join(name), b"\0base").unwrap();
        repo.commit(&base, "add the file");
        let worktree = repo.worktree_path("wf/k3f9");
        repo.git
            .prepare_worktree("wf/k3f9", &worktree, "main")
            .unwrap();
        std::fs::write(worktree.join(name), work).unwrap();
        git_in(&worktree, &["add", name]);
        git_in(&worktree, &["commit", "--quiet", "-m", "work"]);
        match main_side {
            Some(bytes) => std::fs::write(base.join(name), bytes).unwrap(),
            None => std::fs::remove_file(base.join(name)).unwrap(),
        }
        git_in(&base, &["add", "-A", name]);
        git_in(&base, &["commit", "--quiet", "-m", "main side"]);
    }

    #[test]
    fn a_binary_conflict_stays_a_conflict_until_it_is_staged() {
        let repo = TestRepo::new();
        conflict_in(&repo, "img.bin", b"\0work", Some(b"\0main"));
        let worktree = repo.worktree_path("wf/k3f9");
        let conflict = Rebase::Conflict {
            paths: vec!["img.bin".into()],
        };
        assert_eq!(rebase(&repo), conflict);
        // The worktree holds the target's version without markers; that is not a resolution.
        assert_eq!(rebase(&repo), conflict);
        std::fs::write(worktree.join("img.bin"), b"\0work").unwrap();
        git_in(&worktree, &["add", "img.bin"]);
        assert_eq!(rebase(&repo), Rebase::Rebased);
        assert_eq!(std::fs::read(worktree.join("img.bin")).unwrap(), b"\0work");
    }

    #[test]
    fn a_modify_delete_conflict_stays_a_conflict_until_it_is_staged() {
        for deleted_by_main in [true, false] {
            let repo = TestRepo::new();
            if deleted_by_main {
                conflict_in(&repo, "gone.txt", b"work\n", None);
            } else {
                // The branch deletes the file the target modified.
                let base = repo.directory();
                std::fs::write(base.join("gone.txt"), "base\n").unwrap();
                repo.commit(&base, "add the file");
                let worktree = repo.worktree_path("wf/k3f9");
                repo.git
                    .prepare_worktree("wf/k3f9", &worktree, "main")
                    .unwrap();
                git_in(&worktree, &["rm", "--quiet", "gone.txt"]);
                git_in(&worktree, &["commit", "--quiet", "-m", "work"]);
                std::fs::write(base.join("gone.txt"), "main\n").unwrap();
                repo.commit(&base, "main side");
            }
            let worktree = repo.worktree_path("wf/k3f9");
            let conflict = Rebase::Conflict {
                paths: vec!["gone.txt".into()],
            };
            assert_eq!(rebase(&repo), conflict);
            assert_eq!(rebase(&repo), conflict);
            git_in(&worktree, &["rm", "--quiet", "--force", "gone.txt"]);
            assert_eq!(rebase(&repo), Rebase::Rebased);
        }
    }

    #[test]
    fn a_rebase_stopped_with_the_apply_backend_is_found_and_aborted() {
        let repo = TestRepo::new();
        let worktree = repo.worktree_path("wf/k3f9");
        let work = repo.work("wf/k3f9", "file.txt", "one\nTWO\n");
        let main = repo.directory();
        repo.write(&main, "file.txt", "one\n2\n");
        repo.commit(&main, "conflicting change");
        let output = Command::new("git")
            .current_dir(&worktree)
            .args(["-c", "rebase.backend=apply", "rebase", "main"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        let rebase_apply = git_in(
            &worktree,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                "rebase-apply",
            ],
        );
        assert!(Path::new(rebase_apply.trim()).is_dir());
        assert_eq!(
            repo.git.stopped_rebase(&worktree).unwrap(),
            StoppedRebase::Conflict {
                paths: vec!["file.txt".into()],
            }
        );
        repo.git.abort_rebase(&worktree).unwrap();
        assert_eq!(
            repo.git.stopped_rebase(&worktree).unwrap(),
            StoppedRebase::NotStopped
        );
        assert_eq!(repo.git.branch_head("wf/k3f9").unwrap(), work);
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

//! The repository an agent works in, and how its commits reach the principal's.
//!
//! The agent's repository is its own: its objects directory has the principal's as alternate, so
//! nothing is copied, and it has no remote. The agent can write all of it and none of the
//! principal's. What it commits is copied into the principal's repository by the daemon, which
//! never runs git with the config, hooks or attributes of the agent's repository to do so.

use std::path::Path;
use std::process::Stdio;

use wf_core::id::ItemId;
use wf_core::session::{CommitId, GitOid};

use crate::git::{GitCli, git};
use crate::{Import, ImportMode, VcsError, item_branch};

/// The variable that adds object directories to those git reads from.
const ALTERNATE_OBJECTS: &str = "GIT_ALTERNATE_OBJECT_DIRECTORIES";

/// Git options for reading the agent's objects: it controls the `commit-graph` and
/// `multi-pack-index` files in its objects directory, and git would believe a forged one about
/// parents and reachability.
const UNTRUSTED_OBJECTS: [&str; 4] = [
    "-c",
    "core.commitGraph=false",
    "-c",
    "core.multiPackIndex=false",
];

/// Git never fetches a missing object from a promisor remote while reading the agent's objects.
const NO_LAZY_FETCH: (&str, &str) = ("GIT_NO_LAZY_FETCH", "1");

/// The most `packed-refs` that is read from the agent's repository.
const PACKED_REFS_LIMIT: u64 = 16 * 1024 * 1024;

/// `args` after the options for reading untrusted objects.
fn untrusted<'a>(args: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    UNTRUSTED_OBJECTS
        .into_iter()
        .chain(args)
        .map(str::to_string)
        .collect()
}

impl GitCli {
    /// Makes the repository of an agent at `path`, with `branch` checked out at `commit`. The
    /// agent's objects directory has the principal's as alternate. Does nothing if the
    /// repository is there already, which later sessions on the item continue.
    ///
    /// This is the only time the daemon runs git in the repository, before the agent touches it.
    pub fn create_agent_repository(
        &self,
        path: &Path,
        branch: &str,
        commit: CommitId,
    ) -> Result<(), VcsError> {
        if path.join(".git").exists() {
            return Ok(());
        }
        self.init_with_alternates(path, ["-b", branch])?;
        let reference = format!("refs/heads/{branch}");
        self.run(path, &[], ["update-ref", &reference, &commit.to_string()])?
            .ok()?;
        self.run(path, &[], ["reset", "--hard", "--quiet"])?.ok()?;
        // The agent commits under its own name, from the environment, but git wants an address,
        // which the principal's repository may be the only place to give one.
        let email = self.run(&self.repository, &[], ["config", "--get", "user.email"])?;
        if email.success && !email.stdout.trim().is_empty() {
            let address = email.stdout.trim().to_string();
            self.run(path, &[], ["config", "user.email", &address])?
                .ok()?;
        }
        Ok(())
    }

    /// Makes a repository at `path` with `commit` checked out, detached, for a reviewer.
    pub fn create_review_repository(&self, path: &Path, commit: CommitId) -> Result<(), VcsError> {
        self.init_with_alternates(path, [])?;
        self.run(
            path,
            &[],
            ["update-ref", "--no-deref", "HEAD", &commit.to_string()],
        )?
        .ok()?;
        self.run(path, &[], ["reset", "--hard", "--quiet"])?.ok()?;
        Ok(())
    }

    /// The commit an agent starts the work on `item` at: where its branch is when the principal's
    /// repository has it already, otherwise the tip of `target`.
    pub fn starting_commit(&self, item: ItemId, target: &str) -> Result<CommitId, VcsError> {
        match self.branch_head(&item_branch(item)) {
            Err(VcsError::NoBranch(_)) => self.branch_head(target),
            found => found,
        }
    }

    /// Makes the branch of `item` at `commit`, unless the principal's repository has it already,
    /// so that the item has a branch from the start of its first session.
    pub fn ensure_item_branch(&self, item: ItemId, commit: CommitId) -> Result<(), VcsError> {
        let branch = item_branch(item);
        if !self.branch_exists(&branch)? {
            let reference = format!("refs/heads/{branch}");
            // The empty old value makes this fail, not overwrite, if the branch came meanwhile.
            self.run(
                &self.repository,
                &[],
                ["update-ref", &reference, &commit.to_string(), ""],
            )?
            .ok()?;
        }
        Ok(())
    }

    fn init_with_alternates<const N: usize>(
        &self,
        path: &Path,
        options: [&str; N],
    ) -> Result<(), VcsError> {
        std::fs::create_dir_all(path)?;
        // No templates: the hooks of the principal's `init.templateDir` are not the agent's.
        let args = ["init", "--quiet", "--template="]
            .into_iter()
            .chain(options)
            .collect::<Vec<_>>();
        self.run(path, &[], args)?.ok()?;
        let alternates = path.join(".git/objects/info");
        std::fs::create_dir_all(&alternates)?;
        std::fs::write(
            alternates.join("alternates"),
            format!("{}\n", self.git_directory.join("objects").display()),
        )?;
        Ok(())
    }

    /// Copies the commits on the branch of `item` in the agent's repository into the principal's
    /// and points the branch of the item there at the head. The head is read from the file of the
    /// reference and the objects are packed from the principal's repository, which reads the
    /// agent's objects through `GIT_ALTERNATE_OBJECT_DIRECTORIES`: no git process starts in the
    /// agent's repository, which would read its config, hooks and attributes.
    pub fn import_commits(
        &self,
        agent_repository: &Path,
        item: ItemId,
        target: &str,
        mode: ImportMode,
    ) -> Result<Import, VcsError> {
        let branch = item_branch(item);
        let Some(head) = agent_branch_head(agent_repository, &branch) else {
            return Ok(Import::NoCommits);
        };
        let objects = agent_repository.join(".git/objects");
        let Some(objects) = objects.to_str() else {
            return Err(VcsError::UnexpectedOutput {
                command: "import".into(),
                output: objects.display().to_string(),
            });
        };
        let environment = [(ALTERNATE_OBJECTS, objects), NO_LAZY_FETCH];
        let (head_text, target_reference) = (head.to_string(), format!("refs/heads/{target}"));
        let commit = format!("{head_text}^{{commit}}");
        self.run(
            &self.repository,
            &environment,
            untrusted(["rev-parse", "--verify", &commit]),
        )?
        .ok()?;
        match mode {
            ImportMode::InProgress => {}
            ImportMode::Submission => {
                let contains = self.run(
                    &self.repository,
                    &environment,
                    untrusted(["merge-base", "--is-ancestor", &target_reference, &head_text]),
                )?;
                if !contains.success {
                    return Ok(Import::BehindTarget);
                }
            }
        }
        // What the principal has already is left out: the target and the branch as it was.
        let own_branch = format!("refs/heads/{branch}");
        let mut reachable = untrusted([
            "rev-list",
            "--objects",
            &head_text,
            "--not",
            &target_reference,
        ]);
        if self.branch_exists(&branch)? {
            reachable.push(own_branch.clone());
        }
        let listing = self.run(&self.repository, &environment, &reachable)?.ok()?;
        if !listing.trim().is_empty() {
            self.pack(&environment, &listing)?;
        }
        // The principal's repository alone, without the agent's objects, must now have all of
        // the commits: a bad import leaves the branch where it was.
        let mut complete = untrusted([
            "rev-list",
            "--objects",
            &head_text,
            "--not",
            &target_reference,
        ]);
        if self.branch_exists(&branch)? {
            complete.push(own_branch.clone());
        }
        self.run(&self.repository, &[NO_LAZY_FETCH], &complete)?
            .ok()?;
        self.run(
            &self.repository,
            &[],
            ["update-ref", &own_branch, &head_text],
        )?
        .ok()?;
        Ok(Import::Imported { commit: head })
    }

    /// Writes a pack of the objects `listing` names, as `git rev-list --objects` prints them,
    /// into the principal's repository through `index-pack --strict`, which refuses malformed
    /// objects (a `.git` entry in a tree, say) and objects that do not connect.
    fn pack(&self, environment: &[(&str, &str)], listing: &str) -> Result<(), VcsError> {
        use std::io::Write;
        let mut packer = git();
        packer
            .args(UNTRUSTED_OBJECTS)
            .args(["pack-objects", "--quiet", "--stdout"])
            .envs(environment.iter().copied())
            .env("LC_ALL", "C")
            .current_dir(&self.repository)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut packer = packer.spawn().map_err(VcsError::Spawn)?;
        let mut input = packer.stdin.take().expect("stdin is piped");
        let pack = packer.stdout.take().expect("stdout is piped");
        let mut indexer = git();
        // Without the agent's objects: what the pack lacks must be in the principal's.
        indexer
            .args(["index-pack", "--stdin", "--strict", "--fix-thin"])
            .env(NO_LAZY_FETCH.0, NO_LAZY_FETCH.1)
            .env("LC_ALL", "C")
            .current_dir(&self.repository)
            .stdin(Stdio::from(pack))
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let indexer = indexer.spawn().map_err(VcsError::Spawn)?;
        // The listing is small beside the pack, and `pack-objects` reads all of it before it
        // writes anything.
        let written = input.write_all(listing.as_bytes());
        drop(input);
        let packed = packer.wait_with_output().map_err(VcsError::Spawn)?;
        let indexed = indexer.wait_with_output().map_err(VcsError::Spawn)?;
        for (command, output) in [("pack-objects", packed), ("index-pack", indexed)] {
            if !output.status.success() {
                return Err(VcsError::Git {
                    command: command.into(),
                    stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
                });
            }
        }
        written?;
        Ok(())
    }
}

/// The commit the branch `branch` of the repository at `repository` points at, read from the
/// reference's file or `packed-refs` without git. `None` when there is no such branch, or what
/// the file says is not a commit id: an agent can write anything there.
pub fn agent_branch_head(repository: &Path, branch: &str) -> Option<CommitId> {
    let git_directory = repository.join(".git");
    if !std::fs::symlink_metadata(&git_directory).ok()?.is_dir() {
        return None;
    }
    read_branch_head(&git_directory, branch)
}

/// The commit `branch` points at in `git_directory`, read from the reference's file or
/// `packed-refs` without git. `None` when there is no such branch, or the files do not name a
/// commit id: a symbolic reference, say, or a repository that keeps its references elsewhere.
pub(crate) fn read_branch_head(git_directory: &Path, branch: &str) -> Option<CommitId> {
    let reference = format!("refs/heads/{branch}");
    let file = git_directory.join(&reference);
    let text = match std::fs::symlink_metadata(&file) {
        Ok(metadata) if metadata.is_file() => read_regular_file(&file, 1024)?,
        Ok(_) => return None,
        Err(_) => read_regular_file(&git_directory.join("packed-refs"), PACKED_REFS_LIMIT)?
            .lines()
            .find_map(|line| line.strip_suffix(reference.as_str())?.strip_suffix(' '))?
            .to_string(),
    };
    text.trim().parse::<GitOid>().ok().map(CommitId::Git)
}

/// The text of the file at `path`, if it is a regular file (not a link, FIFO or device, which
/// could block the daemon or never end) of at most `limit` bytes.
fn read_regular_file(path: &Path, limit: u64) -> Option<String> {
    use std::io::Read;
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > limit {
        return None;
    }
    let mut text = String::new();
    // The file can grow after the check: the limit applies to what is read.
    std::fs::File::open(path)
        .ok()?
        .take(limit)
        .read_to_string(&mut text)
        .ok()?;
    Some(text)
}

/// Removes the repository at `path` with the work in it. Does nothing if it is not there.
pub fn remove_agent_repository(path: &Path) -> Result<(), VcsError> {
    match std::fs::remove_dir_all(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        removed => Ok(removed?),
    }
}

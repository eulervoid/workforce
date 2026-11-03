//! Snapshots of the work: the tree of the files in an item's agent repository at one moment,
//! committed or not (SPEC §10).
//!
//! Git runs in the item's snapshot repository, a bare repository of the daemon's own, with the
//! agent repository as its work tree and an index of the session's own. No git process reads the
//! agent repository's `.git`, so its config, hooks and filters play no part.

use std::path::{Path, PathBuf};

use wf_core::id::{HashId, ItemId, SessionId};
use wf_core::session::{CommitId, GitOid};

use crate::git::{show_diff_in, spawn, spawn_with_input};
use crate::{Pager, VcsError, bare_repository, item_name, item_repository};

/// The largest file a snapshot takes; a larger one is left out and named.
pub const LARGEST_FILE: u64 = 8 * 1024 * 1024;

/// The directory of the snapshot repositories of a workforce's items.
pub fn snapshots(workforce_directory: &Path) -> PathBuf {
    workforce_directory.join("snapshots")
}

/// The snapshot repository of `item`: bare, with the objects of the workforce's bare repository
/// as alternates, so that what is committed there is not stored twice. It is kept as long as the
/// transcripts of the item's sessions, and never packed or pruned.
pub fn snapshot_repository(workforce_directory: &Path, item: ItemId) -> PathBuf {
    snapshots(workforce_directory).join(item_name(item))
}

/// The files of the agent repository of an item at one moment, committed or not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    /// The tree of the files, in the item's snapshot repository.
    pub tree: GitOid,
    /// The files over [`LARGEST_FILE`], relative to the repository, which the tree lacks.
    pub left_out: Vec<String>,
    /// The tree of the session's snapshot before this one, if it took one.
    pub previous: Option<GitOid>,
}

/// Takes a snapshot of the agent repository of `item` for `session`, whose index in the snapshot
/// repository is kept between snapshots, so that only the files changed since are read. Its
/// `previous` is the session's latest recorded snapshot; the snapshot becomes the latest only by
/// [`record_snapshot`], once it is recorded. Makes the snapshot repository on first use.
/// `.gitignore` and `.gitattributes` among the files apply as in a commit.
pub fn take_snapshot(
    workforce_directory: &Path,
    item: ItemId,
    session: SessionId,
) -> Result<Snapshot, VcsError> {
    let repository = snapshot_repository(workforce_directory, item);
    create_snapshot_repository(&repository, &bare_repository(workforce_directory))?;
    let work_tree = item_repository(workforce_directory, item);
    let index = repository.join(format!("{}.index", session.to_base32()));
    let latest = latest_snapshot_ref(session);
    let (Some(repository_text), Some(work_tree_text), Some(index)) =
        (repository.to_str(), work_tree.to_str(), index.to_str())
    else {
        return Err(VcsError::UnexpectedOutput {
            command: "snapshot".into(),
            output: repository.display().to_string(),
        });
    };
    let environment = [("GIT_INDEX_FILE", index)];
    let arguments = |args: &[&str]| {
        ["--git-dir", repository_text, "--work-tree", work_tree_text]
            .iter()
            .chain(args)
            .map(|arg| arg.to_string())
            .collect::<Vec<_>>()
    };
    // The files `add -A` would read: those new or changed since the last snapshot.
    let changed = spawn(
        &repository,
        &environment,
        arguments(&[
            "ls-files",
            "-z",
            "--others",
            "--modified",
            "--exclude-standard",
        ]),
    )?
    .ok()?;
    let mut left_out: Vec<String> = changed
        .split('\0')
        .filter(|path| !path.is_empty())
        .filter(|path| {
            std::fs::symlink_metadata(work_tree.join(path))
                .is_ok_and(|metadata| metadata.is_file() && metadata.len() > LARGEST_FILE)
        })
        .map(str::to_string)
        .collect();
    left_out.sort();
    left_out.dedup();
    if !left_out.is_empty() {
        // Not in the tree, also when an earlier snapshot had it.
        let removals = left_out.iter().map(|path| format!("{path}\0")).collect();
        spawn_with_input(
            &repository,
            &environment,
            arguments(&["update-index", "--force-remove", "-z", "--stdin"]),
            removals,
        )?
        .ok()?;
    }
    let pathspecs = std::iter::once(".\0".to_string())
        .chain(
            left_out
                .iter()
                .map(|path| format!(":(exclude,literal){path}\0")),
        )
        .collect();
    spawn_with_input(
        &repository,
        &environment,
        arguments(&["add", "-A", "--pathspec-from-file=-", "--pathspec-file-nul"]),
        pathspecs,
    )?
    .ok()?;
    let tree = spawn(&repository, &environment, arguments(&["write-tree"]))?.ok()?;
    let tree = parse_tree("write-tree", &tree)?;
    let previous = spawn(
        &repository,
        &[],
        arguments(&["rev-parse", "--verify", "--quiet", &latest]),
    )?;
    let previous = match previous.success {
        true => Some(parse_tree("rev-parse", &previous.stdout)?),
        false => None,
    };
    Ok(Snapshot {
        tree,
        left_out,
        previous,
    })
}

/// Makes `tree` the latest recorded snapshot of `session` on `item`, the reference
/// `refs/snapshots/<session>` in the item's snapshot repository, which the next
/// [`take_snapshot`] compares with.
pub fn record_snapshot(
    workforce_directory: &Path,
    item: ItemId,
    session: SessionId,
    tree: GitOid,
) -> Result<(), VcsError> {
    let repository = snapshot_repository(workforce_directory, item);
    spawn(
        &repository,
        &[],
        [
            "update-ref",
            &latest_snapshot_ref(session),
            &tree.to_string(),
        ],
    )?
    .ok()?;
    Ok(())
}

/// The reference to the latest recorded snapshot of `session`.
fn latest_snapshot_ref(session: SessionId) -> String {
    format!("refs/snapshots/{}", session.to_base32())
}

/// The tree of the latest recorded snapshot of `session` on `item`; `None` when it recorded none.
pub fn latest_snapshot(
    workforce_directory: &Path,
    item: ItemId,
    session: SessionId,
) -> Result<Option<GitOid>, VcsError> {
    let repository = snapshot_repository(workforce_directory, item);
    if !repository.join("HEAD").is_file() {
        return Ok(None);
    }
    let found = spawn(
        &repository,
        &[],
        [
            "rev-parse",
            "--verify",
            "--quiet",
            &latest_snapshot_ref(session),
        ],
    )?;
    match found.success {
        true => Ok(Some(parse_tree("rev-parse", &found.stdout)?)),
        false => Ok(None),
    }
}

/// What the snapshot `tree` of `item` changes against `commit`, which the workforce's bare
/// repository has: the uncommitted part of the work at the snapshot, in the form
/// [`crate::diff::parse`] reads.
pub fn snapshot_diff(
    workforce_directory: &Path,
    item: ItemId,
    commit: CommitId,
    tree: GitOid,
) -> Result<String, VcsError> {
    let args = crate::diff::ARGS.map(String::from);
    snapshot_diff_with(workforce_directory, item, commit, tree, &args)
}

/// The same diff with `args` added to `git diff`, such as `--stat` or `-- src/`, as
/// [`crate::GitCli::diff`] takes them.
pub fn snapshot_diff_with(
    workforce_directory: &Path,
    item: ItemId,
    commit: CommitId,
    tree: GitOid,
    args: &[String],
) -> Result<String, VcsError> {
    let repository = snapshot_repository(workforce_directory, item);
    spawn(
        &repository,
        &[],
        ["diff".to_string()]
            .into_iter()
            .chain(snapshot_diff_arguments(commit, tree, args)),
    )?
    .ok()
}

/// The same diff with `args`, written to the terminal by git itself, as
/// [`crate::GitCli::show_diff`] does.
pub fn show_snapshot_diff(
    workforce_directory: &Path,
    item: ItemId,
    commit: CommitId,
    tree: GitOid,
    args: &[String],
    pager: Pager,
) -> Result<(), VcsError> {
    let repository = snapshot_repository(workforce_directory, item);
    show_diff_in(
        &repository,
        &snapshot_diff_arguments(commit, tree, args),
        pager,
    )
}

/// The revisions of the diff of `tree` against `commit`, then `args`, whose paths follow `--`.
fn snapshot_diff_arguments(commit: CommitId, tree: GitOid, args: &[String]) -> Vec<String> {
    let mut all = vec![commit.to_string(), tree.to_string()];
    all.extend(args.iter().cloned());
    all
}

/// The tree `command` printed.
fn parse_tree(command: &str, output: &str) -> Result<GitOid, VcsError> {
    output
        .trim()
        .parse()
        .map_err(|_| VcsError::UnexpectedOutput {
            command: command.into(),
            output: output.trim().into(),
        })
}

/// Points the snapshot repository of `item`, copied in from another workforce's data, at the
/// objects of this workforce's bare repository, which has those of the other.
pub fn adopt_snapshot_repository(workforce_directory: &Path, item: ItemId) -> Result<(), VcsError> {
    set_alternates(
        &snapshot_repository(workforce_directory, item),
        &bare_repository(workforce_directory),
    )
}

/// Makes the snapshot repository at `path` with the objects of `bare` as alternates, unless it is
/// there already.
fn create_snapshot_repository(path: &Path, bare: &Path) -> Result<(), VcsError> {
    if path.join("HEAD").is_file() {
        return Ok(());
    }
    std::fs::create_dir_all(path)?;
    // No templates: the principal's `init.templateDir` has nothing a snapshot needs.
    spawn(path, &[], ["init", "--bare", "--quiet", "--template="])?.ok()?;
    set_alternates(path, bare)
}

fn set_alternates(path: &Path, bare: &Path) -> Result<(), VcsError> {
    let alternates = path.join("objects/info");
    std::fs::create_dir_all(&alternates)?;
    std::fs::write(
        alternates.join("alternates"),
        format!("{}\n", bare.join("objects").display()),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{git_in, init_repository};

    struct Workforce {
        root: tempfile::TempDir,
        item: ItemId,
        session: SessionId,
    }

    impl Workforce {
        /// A workforce with its bare repository and the agent repository of one item, which has
        /// `file.txt` committed.
        fn new() -> Self {
            let root = tempfile::TempDir::new().unwrap();
            let bare = bare_repository(root.path());
            std::fs::create_dir_all(&bare).unwrap();
            git_in(&bare, &["init", "--bare", "--quiet"]);
            let workforce = Self {
                root,
                item: ItemId::from_bytes([1; 32]),
                session: SessionId::from_bytes([2; 32]),
            };
            std::fs::create_dir_all(workforce.agent()).unwrap();
            init_repository(&workforce.agent());
            workforce
        }

        fn agent(&self) -> PathBuf {
            item_repository(self.root.path(), self.item)
        }

        fn write(&self, name: &str, contents: &[u8]) {
            let path = self.agent().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }

        /// Takes a snapshot without recording it.
        fn take(&self) -> Snapshot {
            take_snapshot(self.root.path(), self.item, self.session).unwrap()
        }

        /// Takes a snapshot and records it, as the daemon does once it is in the transcript.
        fn snapshot(&self) -> Snapshot {
            let snapshot = self.take();
            record_snapshot(self.root.path(), self.item, self.session, snapshot.tree).unwrap();
            snapshot
        }

        /// The files of `tree` with their contents.
        fn files(&self, tree: GitOid) -> Vec<(String, String)> {
            let repository = snapshot_repository(self.root.path(), self.item);
            let listing = git_in(
                &repository,
                &["ls-tree", "-r", "--name-only", &tree.to_string()],
            );
            listing
                .lines()
                .map(|name| {
                    let blob = format!("{tree}:{name}");
                    let contents = git_in(&repository, &["cat-file", "blob", &blob]);
                    (name.to_string(), contents)
                })
                .collect()
        }
    }

    fn file(name: &str, contents: &str) -> (String, String) {
        (name.to_string(), contents.to_string())
    }

    #[test]
    fn a_snapshot_has_the_uncommitted_and_untracked_files_and_not_the_deleted_or_ignored() {
        let workforce = Workforce::new();
        workforce.write("kept.txt", b"kept");
        workforce.write(".gitignore", b"*.log\n");
        git_in(&workforce.agent(), &["add", "--all"]);
        git_in(&workforce.agent(), &["commit", "--quiet", "-m", "more"]);
        workforce.write("file.txt", b"changed");
        workforce.write("new/untracked.txt", b"new");
        workforce.write("build.log", b"ignored");
        std::fs::remove_file(workforce.agent().join("kept.txt")).unwrap();

        let snapshot = workforce.snapshot();

        assert_eq!(
            workforce.files(snapshot.tree),
            [
                file(".gitignore", "*.log"),
                file("file.txt", "changed"),
                file("new/untracked.txt", "new"),
            ]
        );
        assert!(snapshot.left_out.is_empty());
    }

    #[test]
    fn a_later_snapshot_follows_the_files_and_an_unchanged_one_has_the_same_tree() {
        let workforce = Workforce::new();
        let first = workforce.snapshot();
        assert_eq!(first.previous, None);
        let again = workforce.snapshot();
        assert_eq!((again.tree, again.previous), (first.tree, Some(first.tree)));

        workforce.write("added.txt", b"added");
        std::fs::remove_file(workforce.agent().join("file.txt")).unwrap();
        let second = workforce.snapshot();

        assert_ne!(second.tree, first.tree);
        assert_eq!(second.previous, Some(first.tree));
        assert_eq!(workforce.files(second.tree), [file("added.txt", "added")]);
    }

    #[test]
    fn a_snapshot_not_recorded_is_not_the_previous_one_of_the_next() {
        let workforce = Workforce::new();
        let first = workforce.snapshot();
        workforce.write("added.txt", b"added");
        let unrecorded = workforce.take();
        assert_eq!(unrecorded.previous, Some(first.tree));

        let next = workforce.take();

        assert_eq!(
            (next.tree, next.previous),
            (unrecorded.tree, Some(first.tree))
        );
    }

    #[test]
    fn a_snapshot_is_the_latest_once_recorded_and_diffs_against_a_commit_of_the_bare_repository() {
        let workforce = Workforce::new();
        let root = workforce.root.path();
        assert_eq!(
            latest_snapshot(root, workforce.item, workforce.session).unwrap(),
            None
        );
        let bare = bare_repository(root).display().to_string();
        git_in(
            &workforce.agent(),
            &["push", "--quiet", &bare, "HEAD:refs/heads/work"],
        );
        let head = git_in(Path::new(&bare), &["rev-parse", "work"]);
        let head = CommitId::Git(head.trim().parse().unwrap());
        workforce.write("file.txt", b"changed\n");
        workforce.write("new.txt", b"new\n");

        let unrecorded = workforce.take();
        assert_eq!(
            latest_snapshot(root, workforce.item, workforce.session).unwrap(),
            None
        );
        let snapshot = workforce.snapshot();
        assert_eq!(snapshot.tree, unrecorded.tree);
        assert_eq!(
            latest_snapshot(root, workforce.item, workforce.session).unwrap(),
            Some(snapshot.tree)
        );

        let diff = snapshot_diff(root, workforce.item, head, snapshot.tree).unwrap();
        let files: Vec<String> = crate::diff::parse(&diff)
            .into_iter()
            .map(|file| file.path)
            .collect();
        assert_eq!(files, ["file.txt", "new.txt"]);
        assert!(diff.contains("+changed"));
    }

    #[test]
    fn a_file_over_the_limit_is_left_out_and_named() {
        let workforce = Workforce::new();
        workforce.write("small.txt", b"small");
        let first = workforce.snapshot();
        assert!(first.left_out.is_empty());

        let large = vec![b'x'; LARGEST_FILE as usize + 1];
        workforce.write("large.bin", &large);
        workforce.write("small.txt", &large);
        workforce.write("limit.bin", &large[..LARGEST_FILE as usize]);
        let snapshot = workforce.snapshot();

        assert_eq!(snapshot.left_out, ["large.bin", "small.txt"]);
        let names: Vec<String> = workforce
            .files(snapshot.tree)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["file.txt", "limit.bin"]);
    }

    #[test]
    fn a_filter_hook_or_fsmonitor_of_the_agent_repository_never_runs() {
        use std::os::unix::fs::PermissionsExt;

        let workforce = Workforce::new();
        let ran = workforce.root.path().join("ran");
        let script = workforce.root.path().join("script");
        std::fs::write(
            &script,
            format!("#!/bin/sh\ntouch '{}'\ncat\n", ran.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let script = script.to_str().unwrap();
        for key in [
            "filter.agent.clean",
            "core.fsmonitor",
            "core.hooksPath",
            "core.pager",
        ] {
            git_in(&workforce.agent(), &["config", key, script]);
        }
        workforce.write(".gitattributes", b"* filter=agent\n");
        workforce.write("file.txt", b"changed");

        let snapshot = workforce.snapshot();

        assert!(!ran.exists());
        assert!(
            workforce
                .files(snapshot.tree)
                .contains(&file("file.txt", "changed"))
        );
    }
}

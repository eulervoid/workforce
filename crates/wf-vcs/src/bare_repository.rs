//! wf's own bare repository, `<workforce>/git`: the target and the branches of the items, and
//! nothing else. Agent repositories take their objects from it, imports, rebases, checks and
//! merges work in it, and the principal's checkout has it as its remote `wf`.
//!
//! The principal's other branches are never in it, which is what keeps them out of what another
//! device is served from it.

use std::path::Path;

use crate::git::GitCli;
use crate::{ITEM_BRANCH_PREFIX, VcsError};

/// The name of the remote the principal's checkout has for the bare repository.
pub const REMOTE: &str = "wf";

/// The config of the principal's checkout that the bare repository starts with: who the merge
/// commits are by, and how they are signed. The checkout's own config is not read afterwards, and
/// the git config of the principal's account applies to the bare repository as it does to any.
const COPIED_CONFIG: [&str; 8] = [
    "user.name",
    "user.email",
    "user.signingkey",
    "commit.gpgsign",
    "gpg.format",
    "gpg.program",
    "gpg.ssh.program",
    "gpg.ssh.allowedSignersFile",
];

impl GitCli {
    /// Makes the bare repository at `path` with `target` as its head, and opens it; opens it as it
    /// is when it exists already. With the principal's `checkout`, the repository starts with the
    /// target and the item branches the checkout has, and the checkout gets the repository as its
    /// remote `wf`, the upstream of the target.
    ///
    /// Pushes take only fast-forwards (`receive.denyNonFastForwards`): the principal's own
    /// commits go out as new tips of the target, never as a rewrite of what merged work built on.
    pub fn create_bare_repository(
        path: &Path,
        target: &str,
        checkout: Option<&Path>,
    ) -> Result<GitCli, VcsError> {
        if path.join("HEAD").is_file() {
            return GitCli::open(path);
        }
        // A checkout that is gone or no longer a repository leaves an empty one, which the first
        // push fills.
        let checkout = checkout.filter(|checkout| GitCli::open(checkout).is_ok());
        let made = Self::make_bare(path, target, checkout);
        if made.is_err() {
            // Nothing half made stays for the next start to take for a repository.
            let _ = std::fs::remove_dir_all(path);
        }
        made
    }

    /// Makes the bare repository of a fork at `path` from `parent`, the bare repository of the
    /// workforce it was forked from, and opens it; opens it as it is when it exists already. It
    /// takes every object of the parent, so the commits its history names are there whether a
    /// branch reaches them or not, its branches (the target and the item branches) and the config
    /// the parent started with. With the principal's `checkout`, the checkout's remote `wf` moves
    /// to it. Without a parent repository it is made as a new one, from the checkout.
    pub fn create_fork_repository(
        path: &Path,
        target: &str,
        parent: &Path,
        checkout: Option<&Path>,
    ) -> Result<GitCli, VcsError> {
        if path.join("HEAD").is_file() {
            return GitCli::open(path);
        }
        if !parent.join("HEAD").is_file() {
            return Self::create_bare_repository(path, target, checkout);
        }
        let checkout = checkout.filter(|checkout| GitCli::open(checkout).is_ok());
        let made = Self::clone_bare(path, target, parent, checkout);
        if made.is_err() {
            let _ = std::fs::remove_dir_all(path);
        }
        made
    }

    fn make_bare(path: &Path, target: &str, checkout: Option<&Path>) -> Result<GitCli, VcsError> {
        std::fs::create_dir_all(path)?;
        // No templates: no hooks, whatever the principal's `init.templateDir` has.
        let initial = format!("--initial-branch={target}");
        let args = ["init", "--quiet", "--bare", "--template=", &initial];
        crate::git::spawn(path, &[], args)?.ok()?;
        let bare = GitCli::open(path)?;
        bare.configure_receive()?;
        if let Some(checkout) = checkout {
            bare.copy_config(checkout)?;
            bare.fetch_branches(checkout, target)?;
            GitCli::open(checkout)?.add_remote(path, target)?;
        }
        Ok(bare)
    }

    fn clone_bare(
        path: &Path,
        target: &str,
        parent: &Path,
        checkout: Option<&Path>,
    ) -> Result<GitCli, VcsError> {
        let above = path.parent().unwrap_or(path);
        std::fs::create_dir_all(above)?;
        // A local clone links the whole object directory, unreachable objects included.
        let (parent_text, path_text) = (parent.to_string_lossy(), path.to_string_lossy());
        let args = [
            "clone",
            "--quiet",
            "--bare",
            "--no-tags",
            "--template=",
            &parent_text,
            &path_text,
        ];
        crate::git::spawn(above, &[], args)?.ok()?;
        let bare = GitCli::open(path)?;
        bare.run(path, &[], ["remote", "remove", "origin"])?.ok()?;
        bare.configure_receive()?;
        bare.copy_config(parent)?;
        if let Some(checkout) = checkout {
            GitCli::open(checkout)?.add_remote(path, target)?;
        }
        Ok(bare)
    }

    fn configure_receive(&self) -> Result<(), VcsError> {
        for (key, value) in [
            ("receive.denyNonFastForwards", "true"),
            // Nothing packs or prunes behind the daemon's back: the agents' repositories read its
            // objects.
            ("receive.autogc", "false"),
        ] {
            self.run(&self.repository, &[], ["config", key, value])?
                .ok()?;
        }
        Ok(())
    }

    /// Copies what `COPIED_CONFIG` names from the config of the repository at `from`, the keys it
    /// sets itself and not those it gets from the principal's account.
    fn copy_config(&self, from: &Path) -> Result<(), VcsError> {
        for key in COPIED_CONFIG {
            let found = self.run(from, &[], ["config", "--local", "--get", key])?;
            if found.success {
                let value = found.stdout.trim_end_matches('\n');
                self.run(&self.repository, &[], ["config", key, value])?
                    .ok()?;
            }
        }
        Ok(())
    }

    /// Copies the target and the item branches of the repository at `from` into this one.
    fn fetch_branches(&self, from: &Path, target: &str) -> Result<(), VcsError> {
        let target_reference = format!("refs/heads/{target}");
        let has_target = self
            .run(
                from,
                &[],
                ["rev-parse", "--verify", "--quiet", &target_reference],
            )?
            .success;
        let item_branches =
            format!("+refs/heads/{ITEM_BRANCH_PREFIX}*:refs/heads/{ITEM_BRANCH_PREFIX}*");
        let mut refspecs = vec![item_branches];
        if has_target {
            refspecs.push(format!("+{target_reference}:{target_reference}"));
        }
        let from = from.to_string_lossy();
        let args = [
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            &from,
        ]
        .into_iter()
        .chain(refspecs.iter().map(String::as_str));
        self.run(&self.repository, &[], args)?.ok()?;
        Ok(())
    }

    /// Makes the bare repository at `bare` the remote `wf` of this checkout and the upstream of
    /// `target`, and fetches it, so that `git pull` and `git push` on `target` go to it. A push
    /// to it without refspecs pushes the target alone: everything in the bare repository is
    /// shared with the workforce, so no other branch of the principal is to go there by accident.
    fn add_remote(&self, bare: &Path, target: &str) -> Result<(), VcsError> {
        let url = bare.to_string_lossy();
        let known = self.run(&self.repository, &[], ["remote", "get-url", REMOTE])?;
        let command = if known.success { "set-url" } else { "add" };
        self.run(&self.repository, &[], ["remote", command, REMOTE, &url])?
            .ok()?;
        // Not `--set-upstream-to`, which needs the branch and the remote's tracking branch to be
        // there already.
        let remote_key = format!("branch.{target}.remote");
        let merge_key = format!("branch.{target}.merge");
        let merge = format!("refs/heads/{target}");
        let push_key = format!("remote.{REMOTE}.push");
        let push = format!("{merge}:{merge}");
        for (key, value) in [
            (remote_key.as_str(), REMOTE),
            (merge_key.as_str(), merge.as_str()),
            (push_key.as_str(), push.as_str()),
        ] {
            self.run(&self.repository, &[], ["config", key, value])?
                .ok()?;
        }
        self.run(
            &self.repository,
            &[],
            ["fetch", "--quiet", "--no-tags", REMOTE],
        )?
        .ok()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{TestRepository, git_in};
    use crate::{Import, ImportMode, item_branch};
    use wf_core::id::ItemId;
    use wf_core::session::CommitId;

    fn branches(directory: &Path) -> Vec<String> {
        git_in(directory, &["branch", "--format=%(refname:short)"])
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// A checkout on `main` with an item branch and a branch of the principal's own.
    fn checkout_with_branches() -> (TestRepository, String) {
        let repo = TestRepository::new();
        let item_branch = "wf/k3f9k3f9k3f9";
        repo.work(item_branch, "a.txt", "a\n");
        repo.work("private", "b.txt", "b\n");
        (repo, item_branch.to_string())
    }

    #[test]
    fn the_bare_repository_starts_with_the_target_and_the_item_branches_only() {
        let (repo, item_branch) = checkout_with_branches();
        let path = repo.root.path().join("wf-git");
        let bare = GitCli::create_bare_repository(&path, "main", Some(&repo.directory())).unwrap();
        let mut found = branches(&path);
        found.sort();
        assert_eq!(found, ["main", item_branch.as_str()]);
        assert_eq!(bare.branch_head("main").unwrap(), repo.main());
        assert_eq!(
            git_in(&path, &["rev-parse", "--is-bare-repository"]),
            "true"
        );
        assert_eq!(git_in(&path, &["symbolic-ref", "HEAD"]), "refs/heads/main");
        assert_eq!(
            git_in(&path, &["config", "receive.denyNonFastForwards"]),
            "true"
        );
    }

    #[test]
    fn a_checkout_that_is_gone_or_not_a_repository_leaves_an_empty_repository() {
        let repo = TestRepository::new();
        let gone = repo.root.path().join("gone");
        let plain = tempfile::tempdir().unwrap();
        for (name, checkout) in [("gone", gone.as_path()), ("plain", plain.path())] {
            let path = repo.root.path().join(format!("wf-git-{name}"));
            let bare = GitCli::create_bare_repository(&path, "main", Some(checkout)).unwrap();
            assert!(matches!(
                bare.branch_head("main"),
                Err(VcsError::NoBranch(_))
            ));
        }
    }

    #[test]
    fn making_the_repository_again_leaves_it_as_it_is() {
        let (repo, _) = checkout_with_branches();
        let path = repo.root.path().join("wf-git");
        GitCli::create_bare_repository(&path, "main", Some(&repo.directory())).unwrap();
        git_in(&path, &["branch", "wf/later", "main"]);
        GitCli::create_bare_repository(&path, "main", Some(&repo.directory())).unwrap();
        assert!(branches(&path).contains(&"wf/later".to_string()));
    }

    #[test]
    fn a_repository_made_without_a_checkout_is_empty_and_takes_the_first_push() {
        let repo = TestRepository::new();
        let path = repo.root.path().join("wf-git");
        let bare = GitCli::create_bare_repository(&path, "main", None).unwrap();
        assert!(matches!(
            bare.branch_head("main"),
            Err(VcsError::NoBranch(_))
        ));
        GitCli::open(&repo.directory())
            .unwrap()
            .add_remote(&path, "main")
            .unwrap();
        git_in(&repo.directory(), &["push", REMOTE, "main"]);
        assert_eq!(bare.branch_head("main").unwrap(), repo.main());
    }

    #[test]
    fn a_fork_repository_takes_the_branches_and_commits_of_its_parent_and_the_checkout_moves_to_it()
    {
        let (repo, item_branch) = checkout_with_branches();
        let checkout = repo.directory();
        let parent = repo.root.path().join("parent-git");
        let parent_bare = GitCli::create_bare_repository(&parent, "main", Some(&checkout)).unwrap();
        let orphan = bare_commit(&parent, "wf/gone", "orphan.txt");
        parent_bare.delete_branch("wf/gone").unwrap();

        let path = repo.root.path().join("fork").join("git");
        let fork = GitCli::create_fork_repository(&path, "main", &parent, Some(&checkout)).unwrap();
        let mut found = branches(&path);
        found.sort();
        assert_eq!(found, ["main", item_branch.as_str()]);
        assert_eq!(fork.branch_head("main").unwrap(), repo.main());
        assert_eq!(
            git_in(&path, &["cat-file", "-t", &orphan.to_string()]),
            "commit",
            "a commit no branch reaches comes along"
        );
        assert_eq!(git_in(&path, &["remote"]), "");
        assert_eq!(
            git_in(&path, &["config", "receive.denyNonFastForwards"]),
            "true"
        );

        // The checkout pushes to the fork, not to the parent.
        assert_eq!(
            Path::new(&git_in(&checkout, &["remote", "get-url", REMOTE]))
                .canonicalize()
                .unwrap(),
            path.canonicalize().unwrap()
        );
        repo.write(&checkout, "own.txt", "own\n");
        let own = repo.commit(&checkout, "own");
        git_in(&checkout, &["push", "--quiet"]);
        assert_eq!(fork.branch_head("main").unwrap(), own);
        assert_ne!(parent_bare.branch_head("main").unwrap(), own);
    }

    #[test]
    fn a_fork_without_a_parent_repository_starts_from_the_checkout() {
        let (repo, item_branch) = checkout_with_branches();
        let path = repo.root.path().join("fork-git");
        let parent = repo.root.path().join("no-parent-git");
        GitCli::create_fork_repository(&path, "main", &parent, Some(&repo.directory())).unwrap();
        let mut found = branches(&path);
        found.sort();
        assert_eq!(found, ["main", item_branch.as_str()]);
    }

    #[test]
    fn a_push_without_refspecs_from_another_branch_pushes_only_the_target() {
        let (repo, _) = checkout_with_branches();
        let checkout = repo.directory();
        let path = repo.root.path().join("wf-git");
        let bare = GitCli::create_bare_repository(&path, "main", Some(&checkout)).unwrap();
        repo.write(&checkout, "main.txt", "shared\n");
        let shared = repo.commit(&checkout, "shared");
        // From the worktree of the private branch, which shares the checkout's config.
        let private = repo.worktree_path("private");
        repo.write(&private, "private.txt", "mine\n");
        repo.commit(&private, "private");
        git_in(&private, &["push", "--quiet", REMOTE]);
        assert_eq!(bare.branch_head("main").unwrap(), shared);
        assert!(!branches(&path).contains(&"private".to_string()));
    }

    #[test]
    fn the_checkout_pulls_merged_work_and_pushes_its_own_commits_as_fast_forwards() {
        let (repo, _) = checkout_with_branches();
        let checkout = repo.directory();
        let path = repo.root.path().join("wf-git");
        let bare = GitCli::create_bare_repository(&path, "main", Some(&checkout)).unwrap();
        assert_eq!(
            git_in(&checkout, &["rev-parse", "--abbrev-ref", "main@{upstream}"]),
            "wf/main"
        );

        // Work merged in the bare repository comes in with `git pull`.
        let feature = bare_commit(&path, "main", "merged.txt");
        git_in(&checkout, &["pull", "--ff-only", "--quiet"]);
        assert_eq!(repo.main(), feature);
        assert!(checkout.join("merged.txt").is_file());

        // The principal's own commit goes out with `git push`.
        repo.write(&checkout, "own.txt", "own\n");
        let own = repo.commit(&checkout, "own");
        git_in(&checkout, &["push", "--quiet"]);
        assert_eq!(bare.branch_head("main").unwrap(), own);

        // A rewrite of the target does not.
        git_in(
            &checkout,
            &["commit", "--amend", "--quiet", "-m", "rewritten"],
        );
        let refused = std::process::Command::new("git")
            .args(["push", "--force", "--quiet"])
            .current_dir(&checkout)
            .env_remove("GIT_CONFIG_COUNT")
            .output()
            .unwrap();
        assert!(!refused.status.success());
        assert_eq!(bare.branch_head("main").unwrap(), own);
    }

    #[test]
    fn a_merge_in_the_bare_repository_comes_into_the_checkout_with_a_pull_and_loses_to_a_push() {
        use crate::{CommitSigning, MergeOutcome, MergeTree};

        let (repo, item_branch) = checkout_with_branches();
        let checkout = repo.directory();
        let path = repo.root.path().join("wf-git");
        let bare = GitCli::create_bare_repository(&path, "main", Some(&checkout)).unwrap();
        let work = bare.branch_head(&item_branch).unwrap();
        let MergeTree::Clean { base, tree } = bare.merge_tree("main", work).unwrap() else {
            panic!("the work merges cleanly");
        };

        // The principal pushed first: the merge is not made over it.
        repo.write(&checkout, "own.txt", "own\n");
        let own = repo.commit(&checkout, "own");
        git_in(&checkout, &["push", "--quiet"]);
        let outcome = bare
            .commit_merge("main", base, tree, work, "Merge", &CommitSigning::Unsigned)
            .unwrap();
        assert_eq!(outcome, MergeOutcome::TargetMoved);
        assert_eq!(bare.branch_head("main").unwrap(), own);

        let MergeTree::Clean { base, tree } = bare.merge_tree("main", work).unwrap() else {
            panic!("the work merges cleanly");
        };
        let MergeOutcome::Merged { commit } = bare
            .commit_merge("main", base, tree, work, "Merge", &CommitSigning::Unsigned)
            .unwrap()
        else {
            panic!("the target did not move");
        };
        // The checkout is not touched until the principal pulls.
        assert!(!checkout.join("a.txt").exists());
        git_in(&checkout, &["pull", "--ff-only", "--quiet"]);
        assert_eq!(repo.main(), commit);
        assert!(checkout.join("a.txt").is_file());
    }

    #[test]
    fn the_checkout_fetches_work_from_the_bare_repository_when_it_checks_it_out() {
        let (repo, _) = checkout_with_branches();
        let checkout = repo.directory();
        let path = repo.root.path().join("wf-git");
        let bare = GitCli::create_bare_repository(&path, "main", Some(&checkout)).unwrap();
        let on_branch = bare_commit(&path, "wf/later", "later.txt");
        // The branch of the item is gone, as after a merge or a submission without commits.
        let orphan = bare_commit(&path, "wf/gone", "orphan.txt");
        bare.delete_branch("wf/gone").unwrap();

        let opened = GitCli::open(&checkout).unwrap();
        opened.checkout_detached(on_branch).unwrap();
        assert!(checkout.join("later.txt").is_file());
        opened.checkout_detached(orphan).unwrap();
        assert!(checkout.join("orphan.txt").is_file());
    }

    /// Commits a file on `branch` of the bare repository at `path` through a scratch clone.
    fn bare_commit(path: &Path, branch: &str, name: &str) -> CommitId {
        let scratch = tempfile::TempDir::new().unwrap();
        let clone = scratch.path().join("clone");
        git_in(
            scratch.path(),
            &[
                "clone",
                "--quiet",
                path.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        git_in(&clone, &["config", "user.name", "Test"]);
        git_in(&clone, &["config", "user.email", "test@example.com"]);
        git_in(&clone, &["config", "commit.gpgsign", "false"]);
        std::fs::write(clone.join(name), "x\n").unwrap();
        git_in(&clone, &["add", "--all"]);
        git_in(&clone, &["commit", "--quiet", "-m", name]);
        git_in(
            &clone,
            &["push", "--quiet", "origin", &format!("HEAD:{branch}")],
        );
        GitCli::open(path).unwrap().branch_head(branch).unwrap()
    }

    #[test]
    fn an_agent_repository_reads_the_bare_repository_and_its_commits_are_imported_into_it() {
        let (repo, _) = checkout_with_branches();
        let path = repo.root.path().join("wf-git");
        let bare = GitCli::create_bare_repository(&path, "main", Some(&repo.directory())).unwrap();
        let item = ItemId::from_bytes([7; 32]);
        let branch = item_branch(item);
        let agent = repo.root.path().join("repos").join("agent");
        let start = bare.starting_commit(item, "main").unwrap();
        bare.create_agent_repository(&agent, &branch, start)
            .unwrap();
        bare.ensure_item_branch(item, start).unwrap();
        let alternates =
            std::fs::read_to_string(agent.join(".git/objects/info/alternates")).unwrap();
        assert_eq!(
            Path::new(alternates.trim()).canonicalize().unwrap(),
            path.join("objects").canonicalize().unwrap()
        );

        std::fs::write(agent.join("work.txt"), "work\n").unwrap();
        git_in(&agent, &["add", "--all"]);
        git_in(
            &agent,
            &[
                "-c",
                "user.name=A",
                "-c",
                "user.email=a@example.com",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "work",
            ],
        );
        let Import::Imported { commit } = bare
            .import_commits(&agent, item, "main", ImportMode::Submission)
            .unwrap()
        else {
            panic!("the commit is imported");
        };
        assert_eq!(bare.branch_head(&branch).unwrap(), commit);
        // The checkout reads the branch from the bare repository, not the agent's.
        git_in(&repo.directory(), &["fetch", "--quiet", REMOTE, &branch]);
        assert_eq!(
            git_in(&repo.directory(), &["rev-parse", "FETCH_HEAD"]),
            commit.to_string()
        );
    }

    fn copy_tree(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                copy_tree(&entry.path(), &to.join(entry.file_name()));
            } else {
                std::fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
            }
        }
    }

    #[test]
    fn an_agent_repository_copied_from_another_workforce_works_on_the_renamed_branch() {
        let (repo, _) = checkout_with_branches();
        let old_path = repo.root.path().join("old-git");
        let old =
            GitCli::create_bare_repository(&old_path, "main", Some(&repo.directory())).unwrap();
        let (old_item, new_item) = (ItemId::from_bytes([7; 32]), ItemId::from_bytes([9; 32]));
        let old_agent = repo.root.path().join("old-repos").join("agent");
        old.create_agent_repository(&old_agent, &item_branch(old_item), repo.main())
            .unwrap();
        std::fs::write(old_agent.join("work.txt"), "work\n").unwrap();
        git_in(&old_agent, &["add", "--all"]);
        git_in(
            &old_agent,
            &[
                "-c",
                "user.name=A",
                "-c",
                "user.email=a@example.com",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "work",
            ],
        );
        let Import::Imported { commit } = old
            .import_commits(&old_agent, old_item, "main", ImportMode::InProgress)
            .unwrap()
        else {
            panic!("the commit is imported");
        };
        std::fs::write(old_agent.join("unfinished.txt"), "not yet\n").unwrap();

        let new_path = repo.root.path().join("new-git");
        let new = GitCli::create_fork_repository(&new_path, "main", &old_path, None).unwrap();
        new.rename_branch(&item_branch(old_item), &item_branch(new_item))
            .unwrap();
        let new_agent = repo.root.path().join("new-repos").join("agent");
        copy_tree(&old_agent, &new_agent);
        new.adopt_agent_repository(&new_agent, old_item, new_item)
            .unwrap();
        std::fs::remove_dir_all(&old_path).unwrap();
        std::fs::remove_dir_all(&old_agent).unwrap();

        assert_eq!(new.branch_head(&item_branch(new_item)).unwrap(), commit);
        assert_eq!(
            crate::agent_branch_head(&new_agent, &item_branch(new_item)),
            Some(commit)
        );
        assert_eq!(
            git_in(&new_agent, &["symbolic-ref", "HEAD"]),
            format!("refs/heads/{}", item_branch(new_item))
        );
        // The objects come from the new repository, and the work not committed is still there.
        git_in(&new_agent, &["fsck", "--no-dangling"]);
        assert_eq!(
            git_in(&new_agent, &["status", "--porcelain"]),
            "?? unfinished.txt"
        );
    }

    #[test]
    fn the_checkout_fetches_work_it_does_not_have_before_checking_it_out() {
        let (repo, _) = checkout_with_branches();
        let branch = "wf/later".to_string();
        let checkout = repo.directory();
        let path = repo.root.path().join("wf-git");
        GitCli::create_bare_repository(&path, "main", Some(&checkout)).unwrap();
        let checkout_git = GitCli::open(&checkout).unwrap();

        // Submitted work, on the branch of its item.
        let submitted = bare_commit(&path, &branch, "submitted.txt");
        checkout_git.checkout_detached(submitted).unwrap();
        assert_eq!(
            git_in(&checkout, &["rev-parse", "HEAD"]),
            submitted.to_string()
        );

        // Merged work, on the target.
        checkout_git.checkout_previous().unwrap();
        let merged = bare_commit(&path, "main", "merged.txt");
        checkout_git.checkout_detached(merged).unwrap();
        assert_eq!(
            git_in(&checkout, &["rev-parse", "HEAD"]),
            merged.to_string()
        );
    }

    #[test]
    fn work_whose_branch_is_gone_is_fetched_when_the_target_has_it() {
        let (repo, _) = checkout_with_branches();
        let branch = "wf/later".to_string();
        let checkout = repo.directory();
        let path = repo.root.path().join("wf-git");
        GitCli::create_bare_repository(&path, "main", Some(&checkout)).unwrap();
        let work = bare_commit(&path, &branch, "work.txt");
        // The merge: the target takes the commit, and the branch of the item is deleted.
        git_in(&path, &["update-ref", "refs/heads/main", &work.to_string()]);
        git_in(&path, &["branch", "--delete", "--force", &branch]);
        GitCli::open(&checkout)
            .unwrap()
            .checkout_detached(work)
            .unwrap();
        assert_eq!(git_in(&checkout, &["rev-parse", "HEAD"]), work.to_string());
    }

    #[test]
    fn work_no_branch_reaches_any_more_is_fetched_by_its_commit_id() {
        let (repo, _) = checkout_with_branches();
        let checkout = repo.directory();
        let path = repo.root.path().join("wf-git");
        GitCli::create_bare_repository(&path, "main", Some(&checkout)).unwrap();
        let orphan = bare_commit(&path, "wf/later", "orphan.txt");
        git_in(&path, &["branch", "--delete", "--force", "wf/later"]);
        // Git's protocol version 2, the default, serves an object by its id whatever reaches it;
        // the bare repository still has the commit until a gc prunes it.
        GitCli::open(&checkout)
            .unwrap()
            .checkout_detached(orphan)
            .unwrap();
        assert_eq!(
            git_in(&checkout, &["rev-parse", "HEAD"]),
            orphan.to_string()
        );
    }

    #[test]
    fn a_checkout_without_the_remote_says_so() {
        let repo = TestRepository::new();
        let checkout = GitCli::open(&repo.directory()).unwrap();
        let unknown = CommitId::Git(wf_core::session::GitOid::from_bytes([9; 20]));
        assert!(matches!(
            checkout.checkout_detached(unknown),
            Err(VcsError::NoRemote(_))
        ));
    }
}

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
        let made = Self::make_bare(path, target, checkout);
        if made.is_err() {
            // Nothing half made stays for the next start to take for a repository.
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
        for (key, value) in [
            ("receive.denyNonFastForwards", "true"),
            // Nothing packs or prunes behind the daemon's back: the agents' repositories read its
            // objects.
            ("receive.autogc", "false"),
        ] {
            bare.run(path, &[], ["config", key, value])?.ok()?;
        }
        if let Some(checkout) = checkout {
            bare.fetch_branches(checkout, target)?;
            GitCli::open(checkout)?.add_remote(path, target)?;
        }
        Ok(bare)
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
    /// `target`, and fetches it, so that `git pull` and `git push` on `target` go to it.
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
        for (key, value) in [
            (remote_key.as_str(), REMOTE),
            (merge_key.as_str(), merge.as_str()),
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
}

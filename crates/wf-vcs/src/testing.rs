//! A git repository for tests, shared by the crates that run against one (feature `testing`).

use std::path::{Path, PathBuf};
use std::process::Command;

use wf_core::session::{CommitId, GitOid};

use crate::GitCli;

/// Runs `git` in `directory`, which must succeed, and returns its trimmed standard output.
pub fn git_in(directory: &Path, args: &[&str]) -> String {
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

/// Makes `directory` a repository on `main` with one commit that has `file.txt` of three lines.
pub fn init_repository(directory: &Path) {
    git_in(directory, &["init", "--quiet", "--initial-branch=main"]);
    git_in(directory, &["config", "user.name", "Test"]);
    git_in(directory, &["config", "user.email", "test@example.com"]);
    std::fs::write(directory.join("file.txt"), "one\ntwo\nthree\n").unwrap();
    git_in(directory, &["add", "--all"]);
    git_in(directory, &["commit", "--quiet", "-m", "first"]);
}

/// A repository on `main` with one commit that has `file.txt`, in a temporary directory that
/// also has room for worktrees.
pub struct TestRepository {
    pub root: tempfile::TempDir,
    pub git: GitCli,
}

impl Default for TestRepository {
    fn default() -> Self {
        Self::new()
    }
}

impl TestRepository {
    pub fn new() -> Self {
        let root = tempfile::TempDir::new().unwrap();
        let repository = root.path().join("repo");
        std::fs::create_dir(&repository).unwrap();
        init_repository(&repository);
        Self {
            git: GitCli::open(&repository).unwrap(),
            root,
        }
    }

    /// The main checkout.
    pub fn directory(&self) -> PathBuf {
        self.git.repository().to_path_buf()
    }

    pub fn write(&self, directory: &Path, name: &str, text: &str) {
        std::fs::write(directory.join(name), text).unwrap();
    }

    /// Commits everything in `directory`.
    pub fn commit(&self, directory: &Path, message: &str) -> CommitId {
        git_in(directory, &["add", "--all"]);
        git_in(directory, &["commit", "--quiet", "-m", message]);
        self.commit_id("HEAD", directory)
    }

    /// Where the worktree called `name` goes, outside the repository.
    pub fn worktree_path(&self, name: &str) -> PathBuf {
        self.root.path().join("worktrees").join(name)
    }

    /// Work on `branch` in its worktree and return the commit.
    pub fn work(&self, branch: &str, name: &str, text: &str) -> CommitId {
        let path = self.worktree_path(branch);
        self.git.prepare_worktree(branch, &path, "main").unwrap();
        self.write(&path, name, text);
        self.commit(&path, &format!("change {name}"))
    }

    pub fn main(&self) -> CommitId {
        self.git.branch_head("main").unwrap()
    }

    /// The commit `revision` names in `directory`.
    pub fn commit_id(&self, revision: &str, directory: &Path) -> CommitId {
        let oid: GitOid = git_in(directory, &["rev-parse", revision]).parse().unwrap();
        CommitId::Git(oid)
    }

    /// Has the repository's config sign commits, with a stand-in for ssh-keygen that writes
    /// a signature beside the data it gets.
    pub fn sign_with_a_fake_signer(&self) {
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

    pub fn parents(&self, commit: CommitId) -> Vec<String> {
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

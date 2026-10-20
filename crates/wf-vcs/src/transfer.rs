//! Commits between devices: serving the bare repository with `git upload-pack`, and fetching a
//! commit through a bridge to another device's. Git negotiates, packs and checks; wf only starts
//! the processes and carries their bytes (in `wf-net`, over iroh).

use std::os::fd::OwnedFd;
use std::process::{Child, Stdio};

use wf_core::session::CommitId;

use crate::git::git;
use crate::{GitCli, VcsError};

/// Escapes `word` for the command of an `ext::` URL, where a space separates words and `%`
/// starts a sequence.
fn escape_ext(word: &str) -> String {
    word.replace('%', "%%").replace(' ', "% ")
}

/// Where the branches of the device a join goes through are fetched to before they become
/// branches here.
const JOINED_PREFIX: &str = "refs/wf/joined/";

/// What `git fetch` reaches another device's repository with: a program that pipes its input and
/// output to that device, which git runs as an `ext::` remote.
pub struct Bridge {
    /// The program and its arguments.
    pub command: Vec<String>,
    /// A descriptor the program finds as [`crate::BRIDGE_DESCRIPTOR`] and talks to this process
    /// over. No path leads to it, so no other process can take the place of the program on it.
    pub descriptor: Option<OwnedFd>,
}

impl GitCli {
    /// Starts `git upload-pack` on this repository, its input and output piped: the process to
    /// serve a fetch from another device with. Fetch only.
    ///
    /// It speaks protocol version 2, which serves every object of the repository by id, whether
    /// a ref reaches it or not: everything in wf's repository is shared with the workforce. The
    /// server sets the version, as a client behind an `ext::` bridge cannot ask for one; git
    /// fetching follows what the server speaks.
    pub fn spawn_upload_pack(&self) -> Result<Child, VcsError> {
        git()
            .args(["upload-pack", "--strict"])
            .arg(&self.repository)
            .env("LC_ALL", "C")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_PROTOCOL", "version=2")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(VcsError::Spawn)
    }

    /// Whether this repository has the commit.
    pub fn has_commit(&self, commit: CommitId) -> Result<bool, VcsError> {
        let object = format!("{commit}^{{commit}}");
        let found = self.run(&self.repository, &[], ["cat-file", "-e", &object])?;
        Ok(found.success)
    }

    /// The commits among `commits` that this repository lacks, in the order given.
    pub fn missing_commits(&self, commits: &[CommitId]) -> Result<Vec<CommitId>, VcsError> {
        if commits.is_empty() {
            return Ok(Vec::new());
        }
        let input: String = commits.iter().map(|commit| format!("{commit}\n")).collect();
        let checked = crate::git::spawn_with_input(
            &self.repository,
            ["cat-file", "--batch-check=%(objectname) %(objecttype)"],
            input,
        )?
        .ok()?;
        // One line per commit asked, `<id> commit`, or `<id> missing` for what is not there.
        let present: std::collections::BTreeSet<&str> = checked
            .lines()
            .filter_map(|line| line.strip_suffix(" commit"))
            .collect();
        Ok(commits
            .iter()
            .filter(|commit| !present.contains(commit.to_string().as_str()))
            .copied()
            .collect())
    }

    /// Fetches `commit` with everything it reaches that this repository lacks, from the repository
    /// the `bridge` talks to, which pipes its input and output to a device's `git upload-pack`.
    /// Every object is checked (`fetch.fsckObjects`). No ref is made: the caller points one at the
    /// commit, or the commit stays an object. A device that lacks the commit is
    /// [`VcsError::Unserved`].
    pub fn fetch_commit(&self, bridge: Bridge, commit: CommitId) -> Result<(), VcsError> {
        match self.fetch_through(bridge, &commit.to_string()) {
            Err(VcsError::Git { stderr, .. }) if stderr.contains("not our ref") => {
                Err(VcsError::Unserved(commit))
            }
            fetched => fetched,
        }
    }

    /// Fetches every branch of the repository the `bridge` talks to, and what they reach, as
    /// branches of this one: how a device that joins starts with the target and the item
    /// branches of the device it joined through. A branch this repository has already (made from
    /// a checkout, say) is only moved forward, never back or aside.
    pub fn fetch_branches_through(&self, bridge: Bridge) -> Result<(), VcsError> {
        self.fetch_through(bridge, &format!("+refs/heads/*:{JOINED_PREFIX}*"))?;
        let listed = self
            .run(
                &self.repository,
                &[],
                [
                    "for-each-ref",
                    "--format=%(objectname) %(refname)",
                    JOINED_PREFIX,
                ],
            )?
            .ok()?;
        for line in listed.lines() {
            let (commit, reference) =
                line.split_once(' ')
                    .ok_or_else(|| VcsError::UnexpectedOutput {
                        command: "for-each-ref".into(),
                        output: line.into(),
                    })?;
            let commit = self.oid("for-each-ref", commit)?;
            let branch = reference.trim_start_matches(JOINED_PREFIX);
            loop {
                let here = match self.branch_head(branch) {
                    Ok(here) => Some(here),
                    Err(VcsError::NoBranch(_)) => None,
                    Err(error) => return Err(error),
                };
                if let Some(here) = here
                    && !self.is_ancestor(here, commit)?
                {
                    break;
                }
                if here == Some(commit) || self.move_branch(branch, here, commit)? {
                    break;
                }
            }
            self.run(&self.repository, &[], ["update-ref", "-d", reference])?
                .ok()?;
        }
        Ok(())
    }

    fn fetch_through(&self, bridge: Bridge, what: &str) -> Result<(), VcsError> {
        let words: Vec<String> = bridge.command.iter().map(|word| escape_ext(word)).collect();
        let url = format!("ext::{}", words.join(" "));
        let args = [
            "-c",
            "protocol.ext.allow=always",
            "-c",
            "fetch.fsckObjects=true",
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            &url,
            what,
        ];
        crate::git::spawn_inheriting(&self.repository, &[], args, bridge.descriptor)?.ok()?;
        Ok(())
    }
}

#[cfg(any(test, feature = "testing"))]
impl GitCli {
    /// Fetches `commit` from the repository at `path`, for tests of what fetches commits without
    /// a network.
    pub fn fetch_commit_from_path(
        &self,
        path: &std::path::Path,
        commit: CommitId,
    ) -> Result<(), VcsError> {
        let path = path.to_string_lossy();
        let args = [
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            &path,
            &commit.to_string(),
        ];
        self.run(&self.repository, &[], args)?.ok()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::testing::{TestRepository, git_in};

    /// A bridge from `git fetch` to the `upload-pack` of `server`, through two named pipes and
    /// two threads: what a daemon does over iroh.
    struct Piped {
        directory: tempfile::TempDir,
        threads: Vec<std::thread::JoinHandle<()>>,
    }

    impl Piped {
        fn to(server: &GitCli) -> Self {
            let directory = tempfile::TempDir::new().unwrap();
            let (requests, answers) = (
                directory.path().join("requests"),
                directory.path().join("answers"),
            );
            for pipe in [&requests, &answers] {
                assert!(
                    std::process::Command::new("mkfifo")
                        .arg(pipe)
                        .status()
                        .unwrap()
                        .success()
                );
            }
            let mut child = server.spawn_upload_pack().unwrap();
            let mut input = child.stdin.take().unwrap();
            let mut output = child.stdout.take().unwrap();
            let into_process = std::thread::spawn(move || {
                let mut requests = std::fs::File::open(requests).unwrap();
                let _ = std::io::copy(&mut requests, &mut input);
            });
            let from_process = std::thread::spawn(move || {
                let mut answers = std::fs::OpenOptions::new()
                    .write(true)
                    .open(answers)
                    .unwrap();
                let _ = std::io::copy(&mut output, &mut answers);
                answers.flush().unwrap();
                let _ = child.wait();
            });
            Self {
                directory,
                threads: vec![into_process, from_process],
            }
        }

        fn bridge(&self) -> Bridge {
            let path = |name: &str| self.directory.path().join(name).display().to_string();
            Bridge {
                command: vec![
                    "sh".into(),
                    "-c".into(),
                    format!(
                        "cat <&0 > {} & cat < {}; wait",
                        path("requests"),
                        path("answers")
                    ),
                ],
                descriptor: None,
            }
        }

        fn finish(self) {
            for thread in self.threads {
                thread.join().unwrap();
            }
        }
    }

    /// An empty bare repository to fetch into.
    fn fetcher(root: &Path) -> GitCli {
        let path: PathBuf = tempfile::Builder::new()
            .prefix("fetcher")
            .tempdir_in(root)
            .unwrap()
            .keep();
        git_in(&path, &["init", "--quiet", "--bare"]);
        GitCli::open(&path).unwrap()
    }

    /// A bare server repository with `main` and `wf/item` at the second commit of the repository,
    /// the first and the second commit, and a third made after rewriting the second, which the
    /// server does not have yet.
    struct Served {
        repository: TestRepository,
        server: GitCli,
        first: CommitId,
        second: CommitId,
        rewritten: CommitId,
    }

    fn served() -> Served {
        let repository = TestRepository::new();
        let directory = repository.directory();
        let first = repository.commit_id("HEAD", &directory);
        repository.write(&directory, "file.txt", "changed\n");
        let second = repository.commit(&directory, "second");
        git_in(
            &directory,
            &["checkout", "--quiet", "-b", "other", &first.to_string()],
        );
        repository.write(&directory, "file.txt", "rewritten\n");
        let rewritten = repository.commit(&directory, "rewritten");
        let bare = repository.root.path().join("server.git");
        std::fs::create_dir(&bare).unwrap();
        git_in(&bare, &["init", "--quiet", "--bare"]);
        // The server's refs: the second commit on the item branch first, then moved to the
        // rewritten one, which leaves the second reachable from no ref.
        let server = GitCli::open(&bare).unwrap();
        let source = directory.display().to_string();
        git_in(
            &bare,
            &[
                "fetch",
                "--quiet",
                &source,
                "main:refs/heads/main",
                "main:refs/heads/wf/item",
            ],
        );
        Served {
            repository,
            server,
            first,
            second,
            rewritten,
        }
    }

    fn fetch_from(served: &Served, commit: CommitId) -> (Result<(), VcsError>, GitCli) {
        let into = fetcher(served.repository.root.path());
        let bridge = Piped::to(&served.server);
        let fetched = into.fetch_commit(bridge.bridge(), commit);
        bridge.finish();
        (fetched, into)
    }

    #[test]
    fn a_commit_and_its_history_are_fetched_with_the_branch_head_and_the_older_ones() {
        let served = served();
        for commit in [served.second, served.first] {
            let (fetched, into) = fetch_from(&served, commit);
            fetched.unwrap();
            assert!(into.has_commit(commit).unwrap());
        }
    }

    #[test]
    fn a_head_rewritten_away_is_still_served_and_what_the_server_lacks_is_unserved() {
        let served = served();
        // Moves the item branch and the target off the second commit, so no ref reaches it.
        let rewritten = served.rewritten.to_string();
        let bare = served.server.repository().to_path_buf();
        let source = served.repository.directory().display().to_string();
        git_in(&bare, &["fetch", "--quiet", &source, "other"]);
        git_in(&bare, &["update-ref", "refs/heads/wf/item", &rewritten]);
        git_in(
            &bare,
            &["update-ref", "refs/heads/main", &served.first.to_string()],
        );
        for commit in [served.rewritten, served.second] {
            let (fetched, into) = fetch_from(&served, commit);
            fetched.unwrap();
            assert!(into.has_commit(commit).unwrap());
        }
        let unknown = CommitId::Git(wf_core::session::GitOid::from_bytes([9; 20]));
        let (fetched, _) = fetch_from(&served, unknown);
        assert!(
            matches!(fetched, Err(VcsError::Unserved(commit)) if commit == unknown),
            "{fetched:?}"
        );
    }

    #[test]
    fn the_branches_of_a_device_are_fetched_as_branches() {
        let served = served();
        let into = fetcher(served.repository.root.path());
        let bridge = Piped::to(&served.server);
        into.fetch_branches_through(bridge.bridge()).unwrap();
        bridge.finish();
        assert_eq!(into.branch_head("main").unwrap(), served.second);
        assert_eq!(into.branch_head("wf/item").unwrap(), served.second);
    }

    #[test]
    fn the_branches_of_a_device_do_not_overwrite_branches_that_went_elsewhere() {
        let served = served();
        let into = fetcher(served.repository.root.path());
        // A repository made from a checkout, with the target somewhere else.
        let source = served.repository.directory().display().to_string();
        let into_path = into.repository().to_path_buf();
        git_in(
            &into_path,
            &["fetch", "--quiet", &source, "other:refs/heads/main"],
        );
        let bridge = Piped::to(&served.server);
        let fetched = into.fetch_branches_through(bridge.bridge());
        bridge.finish();
        fetched.unwrap();
        assert_eq!(into.branch_head("main").unwrap(), served.rewritten);
        assert_eq!(into.branch_head("wf/item").unwrap(), served.second);
    }

    #[test]
    fn a_repository_that_has_the_commit_says_so() {
        let served = served();
        assert!(served.server.has_commit(served.second).unwrap());
        let unknown = CommitId::Git(wf_core::session::GitOid::from_bytes([9; 20]));
        assert!(!served.server.has_commit(unknown).unwrap());
    }

    #[test]
    fn the_words_of_a_bridge_keep_their_spaces() {
        assert_eq!(escape_ext("a b%c"), "a% b%%c");
    }
}

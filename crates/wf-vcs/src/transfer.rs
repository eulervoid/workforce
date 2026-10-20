//! Commits between devices: serving the bare repository with `git upload-pack`, and fetching a
//! commit through a bridge to another device's. Git negotiates, packs and checks; wf only starts
//! the processes and carries their bytes (in `wf-net`, over iroh).

use std::process::{Child, Stdio};

use wf_core::session::CommitId;

use crate::git::git;
use crate::{GitCli, VcsError};

/// Escapes `word` for the command of an `ext::` URL, where a space separates words and `%`
/// starts a sequence.
fn escape_ext(word: &str) -> String {
    word.replace('%', "%%").replace(' ', "% ")
}

impl GitCli {
    /// Starts `git upload-pack` on this repository, its input and output piped: the process to
    /// serve a fetch from another device with. Fetch only, and it serves no object that no ref
    /// reaches, such as a head rewritten away (`uploadpack.allowReachableSHA1InWant`).
    ///
    /// It speaks protocol version 0, which no client of an `ext::` bridge can change (`GIT_PROTOCOL`
    /// is removed): version 2 serves any object it has by id, whatever that option says, and would
    /// serve what a rewrite left behind.
    pub fn spawn_upload_pack(&self) -> Result<Child, VcsError> {
        git()
            .args(["-c", "uploadpack.allowReachableSHA1InWant=true"])
            .args(["upload-pack", "--strict"])
            .arg(&self.repository)
            .env("LC_ALL", "C")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env_remove("GIT_PROTOCOL")
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

    /// Fetches `commit` with everything it reaches that this repository lacks, from the repository
    /// the `bridge` command talks to: a program with its arguments, which pipes its input and
    /// output to a device's `git upload-pack`. Every object is checked (`fetch.fsckObjects`). No
    /// ref is made: the caller points one at the commit, or the commit stays an object.
    pub fn fetch_commit(&self, bridge: &[String], commit: CommitId) -> Result<(), VcsError> {
        let words: Vec<String> = bridge.iter().map(|word| escape_ext(word)).collect();
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
    struct Bridge {
        directory: tempfile::TempDir,
        threads: Vec<std::thread::JoinHandle<()>>,
    }

    impl Bridge {
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

        fn command(&self) -> Vec<String> {
            let path = |name: &str| self.directory.path().join(name).display().to_string();
            vec![
                "sh".into(),
                "-c".into(),
                format!(
                    "cat <&0 > {} & cat < {}; wait",
                    path("requests"),
                    path("answers")
                ),
            ]
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
    /// and the commits of both: the first, the second, and a third made after rewriting it.
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
        let bridge = Bridge::to(&served.server);
        let fetched = into.fetch_commit(&bridge.command(), commit);
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
    fn a_head_rewritten_away_and_what_the_server_lacks_are_refused() {
        let served = served();
        // Moves the item branch off the second commit, so no ref reaches it.
        let rewritten = served.rewritten.to_string();
        let bare = served.server.repository().to_path_buf();
        let source = served.repository.directory().display().to_string();
        git_in(&bare, &["fetch", "--quiet", &source, "other"]);
        git_in(&bare, &["update-ref", "refs/heads/wf/item", &rewritten]);
        git_in(
            &bare,
            &["update-ref", "refs/heads/main", &served.first.to_string()],
        );
        assert!(served.server.has_commit(served.second).unwrap());
        let (fetched, into) = fetch_from(&served, served.rewritten);
        fetched.unwrap();
        assert!(into.has_commit(served.rewritten).unwrap());
        let (fetched, into) = fetch_from(&served, served.second);
        let error = fetched.expect_err("a head rewritten away is served");
        assert!(error.to_string().contains("not our ref"), "{error}");
        assert!(!into.has_commit(served.second).unwrap());
        let unknown = CommitId::Git(wf_core::session::GitOid::from_bytes([9; 20]));
        let (fetched, _) = fetch_from(&served, unknown);
        assert!(fetched.is_err(), "what the server lacks is served");
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

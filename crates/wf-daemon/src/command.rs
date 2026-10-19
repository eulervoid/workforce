//! Runs a command on code: a tree is checked out in a worktree, and the command runs there with
//! the environment of agents, in a sandbox, with a timeout, its output kept in a log. What the
//! outcome means is up to whoever asked, such as a check before a merge. `wf check` runs commands
//! the same way in a directory as it is.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use wf_core::name::CommandName;
use wf_core::session::{CommitId, GitOid};
use wf_harness::AcpAgentConfig;
use wf_vcs::{GitCli, VcsError};

use crate::machine_config::{EnvironmentConfig, MachineConfig, SandboxConfig};
pub use crate::sandbox::Access;

/// How many lines of the end of the output a run returns.
const TAIL_LINES: usize = 40;
/// How much of the end of the log is read for those lines at most.
const TAIL_BYTES: u64 = 16 * 1024;

/// The variable that would put cargo's build output outside the worktree, where the builds of
/// other worktrees mix into it.
const CARGO_TARGET_DIR: &str = "CARGO_TARGET_DIR";

/// The prefix of the variables wf sets for a session: `WF_SESSION`, `WF_SESSION_TOKEN`,
/// `WF_WORKFORCE`.
const SESSION_VARIABLES_PREFIX: &str = "WF_";

/// Where and how the daemon runs commands on code: in one worktree it checks trees out in,
/// confined as an agent working there would be.
pub(crate) struct CommandRunner<'a> {
    pub git: &'a GitCli,
    pub confinement: Confinement<'a>,
    /// Where the logs are kept: a directory per name and a file per run.
    pub logs: &'a Path,
}

/// Where a command runs, and how it is confined there: as an agent on this machine is.
pub enum Confinement<'a> {
    /// In the sandbox and environment the machine config gives agents.
    Agents {
        /// The directory it runs in, which [`Access::worktree`] names, and what else the sandbox
        /// allows.
        access: Access<'a>,
        sandbox: &'a SandboxConfig,
        environment: &'a EnvironmentConfig,
        /// Where Fence's settings for the command are written.
        settings: &'a Path,
        /// Where the git config the command runs with is written: see [`wf_vcs::config::write_unsigned_global_config`].
        git_config: &'a Path,
    },
    /// As this process is, which runs in a session and so in the sandbox and environment of its
    /// agent already, but without the session's variables.
    Session { directory: &'a Path },
}

impl<'a> Confinement<'a> {
    /// The confinement of a check: in the sandbox that `config` gives checks, with the
    /// environment it gives agents.
    pub fn for_checks(
        access: Access<'a>,
        config: &'a MachineConfig,
        settings: &'a Path,
        git_config: &'a Path,
    ) -> Self {
        Confinement::Agents {
            access,
            sandbox: config.check_sandbox(),
            environment: &config.environment,
            settings,
            git_config,
        }
    }
}

/// What came of a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Passed,
    Failed(ExitStatus),
    /// It was stopped after the timeout, together with everything it started.
    TimedOut,
}

/// One run of a command.
#[derive(Debug, Clone)]
pub struct CommandRun {
    pub outcome: Outcome,
    /// The last lines of the output, stdout and stderr as they came.
    pub tail: String,
    /// The whole output.
    pub log: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    #[error("the command `{0}` is empty")]
    Empty(CommandName),
    #[error("cannot check the tree out: {0}")]
    Vcs(#[from] VcsError),
    #[error("commands run in Fence on this machine, but `fence` is not on PATH")]
    NoFence,
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl CommandRunner<'_> {
    /// Checks `tree`, a tree or a commit, out in the worktree and runs `argv` there, without a
    /// shell, for `timeout` at most.
    pub async fn run(
        &self,
        tree: GitOid,
        name: &CommandName,
        argv: &[String],
        timeout: Duration,
    ) -> Result<CommandRun, CommandError> {
        let git = self.git.clone();
        let worktree = self.confinement.directory().to_path_buf();
        tokio::task::spawn_blocking(move || git.check_out_tree(&worktree, tree))
            .await
            .map_err(|error| CommandError::Io {
                path: self.confinement.directory().to_path_buf(),
                source: std::io::Error::other(error),
            })??;
        self.confinement
            .run(name, argv, timeout, &self.log_path(name, tree))
            .await
    }

    /// The log of a run of `name` on `tree` that starts now.
    fn log_path(&self, name: &CommandName, tree: GitOid) -> PathBuf {
        let started = jiff::Timestamp::now().strftime("%Y%m%dT%H%M%S%.3fZ");
        self.logs
            .join(name.as_str())
            .join(format!("{started}-{}.log", CommitId::Git(tree).short()))
    }
}

impl Confinement<'_> {
    /// The directory commands run in.
    pub fn directory(&self) -> &Path {
        match self {
            Confinement::Agents { access, .. } => access.worktree,
            Confinement::Session { directory } => directory,
        }
    }

    /// Runs `argv` in the directory as it is, without a shell, for `timeout` at most, and
    /// writes its output to `log`.
    pub async fn run(
        &self,
        name: &CommandName,
        argv: &[String],
        timeout: Duration,
        log: &Path,
    ) -> Result<CommandRun, CommandError> {
        let Some((program, arguments)) = argv.split_first() else {
            return Err(CommandError::Empty(name.clone()));
        };
        let command =
            self.confined(AcpAgentConfig::new(program).args(arguments.iter().cloned()))?;
        let io_error = |source| CommandError::Io {
            path: log.to_path_buf(),
            source,
        };
        if let Some(parent) = log.parent() {
            std::fs::create_dir_all(parent).map_err(io_error)?;
        }
        let output = std::fs::File::create(log).map_err(io_error)?;
        let errors = output.try_clone().map_err(io_error)?;
        let mut process = tokio::process::Command::new(command.command());
        process
            .args(command.arguments())
            .envs(command.environment())
            .env_remove(CARGO_TARGET_DIR)
            .current_dir(self.directory())
            .stdin(Stdio::null())
            .stdout(output)
            .stderr(errors)
            // Its own process group, so that whatever it started can be stopped with it.
            .process_group(0)
            .kill_on_drop(true);
        if let Confinement::Session { .. } = self {
            // The session's variables, its token among them, would let the command act as the
            // session; outside a session the agents' environment leaves them out already.
            for name in crate::environment::daemon_variables() {
                if name.starts_with(SESSION_VARIABLES_PREFIX) {
                    process.env_remove(name);
                }
            }
        }
        crate::inherit::only_stdio(&mut process);
        let mut child = process.spawn().map_err(|source| CommandError::Io {
            path: command.command().to_path_buf(),
            source,
        })?;
        // The group keeps the leader's id while any member lives, even once the leader is reaped.
        let group = child.id().map(|id| nix::unistd::Pid::from_raw(id as i32));
        let outcome = match tokio::time::timeout(timeout, child.wait()).await {
            Ok(status) => {
                let status = status.map_err(io_error)?;
                if status.success() {
                    Outcome::Passed
                } else {
                    Outcome::Failed(status)
                }
            }
            Err(_) => Outcome::TimedOut,
        };
        // Nothing it left running may write to the directory after the run, or into the next one.
        if let Some(group) = group {
            // The group may be gone already; then there is nothing left to stop.
            let _ = nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL);
        }
        if matches!(outcome, Outcome::TimedOut) {
            child.wait().await.map_err(io_error)?;
        }
        let tail = tail(log).map_err(io_error)?;
        Ok(CommandRun {
            outcome,
            tail,
            log: log.to_path_buf(),
        })
    }

    /// `command` in the sandbox, with the environment of agents but without the cargo target
    /// directory of the machine's, and with the git config of [`wf_vcs::config::write_unsigned_global_config`].
    fn confined(&self, command: AcpAgentConfig) -> Result<AcpAgentConfig, CommandError> {
        let Confinement::Agents {
            access,
            sandbox,
            environment,
            settings,
            git_config: git_config_path,
        } = self
        else {
            return Ok(command);
        };
        let io_error = |source| CommandError::Io {
            path: git_config_path.to_path_buf(),
            source,
        };
        wf_vcs::config::write_unsigned_global_config(git_config_path).map_err(io_error)?;
        let command = match sandbox {
            SandboxConfig::Fence(_) if !crate::sandbox::fence_installed() => {
                return Err(CommandError::NoFence);
            }
            SandboxConfig::Fence(config) => {
                crate::sandbox::wrap(config, access, &command, settings).map_err(|source| {
                    CommandError::Io {
                        path: settings.to_path_buf(),
                        source,
                    }
                })?
            }
            SandboxConfig::Off {} => command,
        };
        let mut environment = (*environment).clone();
        environment.set.remove(CARGO_TARGET_DIR);
        environment.inherit.retain(|name| name != CARGO_TARGET_DIR);
        environment.set.insert(
            wf_vcs::config::GLOBAL_CONFIG_VARIABLE.to_string(),
            git_config_path.to_string_lossy().into_owned(),
        );
        Ok(crate::environment::confine(
            command,
            &environment,
            crate::environment::daemon_variables(),
        ))
    }
}

/// The last [`TAIL_LINES`] lines of the log.
fn tail(log: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(log)?;
    let length = file.metadata()?.len();
    let start = length.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    // Read from the middle of the log, the first line is only the end of one.
    let lines: Vec<&str> = text.lines().skip(usize::from(start > 0)).collect();
    Ok(lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::os::unix::process::ExitStatusExt;

    use super::*;
    use crate::DataDir;

    /// A repository with one commit, whose tree has `file.txt`, and a place for the worktree,
    /// the logs and the settings.
    struct Setup {
        root: tempfile::TempDir,
        git: GitCli,
        tree: GitOid,
    }

    impl Setup {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let repository = root.path().join("repo");
            std::fs::create_dir(&repository).unwrap();
            std::fs::write(repository.join("file.txt"), "from the tree\n").unwrap();
            let git = |args: &[&str]| {
                let output = std::process::Command::new("git")
                    .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
                    .args(args)
                    .current_dir(&repository)
                    .env_remove("GIT_DIR")
                    .output()
                    .unwrap();
                assert!(output.status.success(), "git {args:?}");
                String::from_utf8(output.stdout).unwrap()
            };
            git(&["init", "--quiet", "--initial-branch=main"]);
            git(&["add", "--all"]);
            git(&["commit", "--quiet", "--no-gpg-sign", "-m", "first"]);
            let tree = git(&["rev-parse", "HEAD^{tree}"]).trim().parse().unwrap();
            Self {
                git: GitCli::open(&repository).unwrap(),
                root,
                tree,
            }
        }

        fn path(&self, name: &str) -> PathBuf {
            self.root.path().join(name)
        }

        async fn run_with(
            &self,
            environment: &EnvironmentConfig,
            argv: &[&str],
            timeout: Duration,
        ) -> CommandRun {
            self.run_in(&SandboxConfig::Off {}, environment, argv, timeout)
                .await
        }

        async fn run_in(
            &self,
            sandbox: &SandboxConfig,
            environment: &EnvironmentConfig,
            argv: &[&str],
            timeout: Duration,
        ) -> CommandRun {
            let (worktree, logs, settings) = (
                self.path("check"),
                self.path("logs"),
                self.path("check.json"),
            );
            let runner = CommandRunner {
                git: &self.git,
                confinement: Confinement::Agents {
                    access: Access {
                        worktree: &worktree,
                        repository: self.git.repository(),
                        branch: None,
                        socket: Path::new("/nonexistent/daemon.sock"),
                        data_dir: &DataDir::new(self.root.path().to_path_buf()),
                        machine_config: None,
                    },
                    sandbox,
                    environment,
                    settings: &settings,
                    git_config: &self.path("check.gitconfig"),
                },
                logs: &logs,
            };
            let argv: Vec<String> = argv.iter().map(|arg| arg.to_string()).collect();
            runner
                .run(
                    self.tree,
                    &CommandName::new("test").unwrap(),
                    &argv,
                    timeout,
                )
                .await
                .unwrap()
        }

        async fn run(&self, argv: &[&str]) -> CommandRun {
            self.run_with(&EnvironmentConfig::default(), argv, Duration::from_secs(30))
                .await
        }
    }

    #[tokio::test]
    async fn a_command_that_succeeds_on_the_tree_passes() {
        let setup = Setup::new();
        let run = setup.run(&["cat", "file.txt"]).await;
        assert_eq!(run.outcome, Outcome::Passed);
        assert_eq!(run.tail, "from the tree");
        assert!(run.log.starts_with(setup.path("logs").join("test")));
        assert_eq!(
            std::fs::read_to_string(&run.log).unwrap(),
            "from the tree\n"
        );
    }

    #[tokio::test]
    async fn a_command_that_exits_with_an_error_fails() {
        let setup = Setup::new();
        let run = setup
            .run(&["sh", "-c", "echo out; echo broken >&2; exit 3"])
            .await;
        assert_eq!(run.outcome, Outcome::Failed(ExitStatus::from_raw(3 << 8)));
        assert_eq!(run.tail, "out\nbroken");
    }

    #[tokio::test]
    async fn the_arguments_reach_the_program_without_a_shell() {
        let setup = Setup::new();
        let run = setup.run(&["echo", "$HOME; ls"]).await;
        assert_eq!(run.tail, "$HOME; ls");
    }

    #[tokio::test]
    async fn a_command_that_runs_too_long_is_stopped_with_what_it_started() {
        let setup = Setup::new();
        let started = std::time::Instant::now();
        let run = setup
            .run_with(
                &EnvironmentConfig::default(),
                &[
                    "sh",
                    "-c",
                    "echo working; (sleep 1; touch late.txt) & sleep 30",
                ],
                Duration::from_millis(300),
            )
            .await;
        assert_eq!(run.outcome, Outcome::TimedOut);
        assert_eq!(run.tail, "working");
        assert!(started.elapsed() < Duration::from_secs(10));
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(!setup.path("check").join("late.txt").exists());
    }

    #[tokio::test]
    async fn what_a_command_leaves_running_is_stopped_when_it_exits() {
        let setup = Setup::new();
        let run = setup
            .run(&["sh", "-c", "(sleep 1; touch late.txt) & exit 0"])
            .await;
        assert_eq!(run.outcome, Outcome::Passed);
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(!setup.path("check").join("late.txt").exists());
    }

    #[tokio::test]
    async fn only_the_end_of_the_output_is_returned_and_the_log_has_all_of_it() {
        let setup = Setup::new();
        let run = setup.run(&["seq", "1", "100"]).await;
        let expected: Vec<String> = (61..=100).map(|line| line.to_string()).collect();
        assert_eq!(run.tail, expected.join("\n"));
        assert_eq!(
            std::fs::read_to_string(&run.log).unwrap().lines().count(),
            100
        );
    }

    #[tokio::test]
    async fn the_environment_of_agents_applies_without_the_cargo_target_directory() {
        let setup = Setup::new();
        let environment = EnvironmentConfig {
            inherit: vec![CARGO_TARGET_DIR.to_string()],
            set: BTreeMap::from([
                (CARGO_TARGET_DIR.to_string(), "/shared/target".to_string()),
                ("WF_TEST_VALUE".to_string(), "set".to_string()),
            ]),
        };
        let run = setup
            .run_with(
                &environment,
                &["sh", "-c", "echo ${CARGO_TARGET_DIR-unset} $WF_TEST_VALUE"],
                Duration::from_secs(30),
            )
            .await;
        assert_eq!(run.tail, "unset set");
    }

    #[tokio::test]
    async fn each_run_checks_the_tree_out_again() {
        let setup = Setup::new();
        let worktree = setup.path("check");
        setup
            .run(&["sh", "-c", "echo edited > file.txt; echo stray > stray.txt"])
            .await;
        let run = setup.run(&["sh", "-c", "cat file.txt; ls"]).await;
        assert_eq!(run.tail, "from the tree\nfile.txt");
        assert!(!worktree.join("stray.txt").exists());
    }

    /// Commits in a new repository whose config signs with a stand-in for ssh-keygen, and fails
    /// unless the commit is signed: what the tests of signing do.
    const SIGNED_COMMIT: &str = r#"set -e
printf '#!/bin/sh\nfor last; do :; done\necho signature > "$last.sig"\n' > sign
chmod +x sign
git init --quiet signed
cd signed
git config user.name Test
git config user.email test@example.com
git config commit.gpgsign true
git config gpg.format ssh
git config user.signingkey "key::ssh-ed25519 AAAAfake"
git config gpg.ssh.program "$PWD/../sign"
git commit --quiet --allow-empty -m signed
git cat-file commit HEAD | grep -q gpgsig"#;

    #[tokio::test]
    async fn a_repository_that_signs_signs_in_a_command_and_not_in_a_session() {
        let setup = Setup::new();
        let run = setup.run(&["sh", "-c", SIGNED_COMMIT]).await;
        assert_eq!(run.outcome, Outcome::Passed, "{}", run.tail);

        // A session turns signing off for every repository, its tests' included.
        let session = EnvironmentConfig {
            inherit: Vec::new(),
            set: wf_vcs::CommitSigning::Unsigned
                .environment()
                .into_iter()
                .collect(),
        };
        let run = setup
            .run_with(
                &session,
                &["sh", "-c", SIGNED_COMMIT],
                Duration::from_secs(30),
            )
            .await;
        assert!(matches!(run.outcome, Outcome::Failed(_)), "{}", run.tail);
    }

    /// iroh watches the routes through an `AF_ROUTE` socket, which Fence denies on macOS whatever
    /// its settings, so tests of the network need a check sandbox that is off. Once this fails,
    /// Fence allows the socket, and checks can run those tests in it.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_route_socket_opens_without_a_sandbox_and_not_in_fence() {
        const ROUTE_SOCKET: &str =
            "import socket; socket.socket(socket.AF_ROUTE, socket.SOCK_RAW).close()";
        // Inside a sandbox already, such as a session's, the socket is denied, and Fence cannot
        // start.
        if std::env::var_os("FENCE_SANDBOX").is_some() {
            eprintln!("skipped: this runs in Fence already");
            return;
        }
        let setup = Setup::new();
        let argv = ["python3", "-c", ROUTE_SOCKET];
        let environment = EnvironmentConfig::default();
        let timeout = Duration::from_secs(30);
        let run = setup.run(&argv).await;
        assert_eq!(run.outcome, Outcome::Passed, "{}", run.tail);

        if !crate::sandbox::fence_installed() {
            eprintln!("skipped in Fence: it is not installed");
            return;
        }
        let fence = SandboxConfig::Fence(Default::default());
        let run = setup.run_in(&fence, &environment, &argv, timeout).await;
        assert!(matches!(run.outcome, Outcome::Failed(_)), "{}", run.tail);
        assert!(run.tail.contains("Operation not permitted"), "{}", run.tail);
    }

    #[tokio::test]
    async fn in_a_session_a_command_runs_in_the_directory_as_it_is_without_the_cargo_target_directory_and_the_session_variables()
     {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("file.txt"), "uncommitted\n").unwrap();
        let log = directory.path().join("logs").join("test.log");
        let argv: Vec<String> = [
            "sh",
            "-c",
            "cat file.txt; echo ${CARGO_TARGET_DIR-unset}; env | grep ^WF_ || echo none",
        ]
        .map(String::from)
        .to_vec();
        // SAFETY: no other test of this crate depends on these variables; the test of the agents'
        // environment removes them either way.
        unsafe {
            std::env::set_var(CARGO_TARGET_DIR, "/shared/target");
            std::env::set_var("WF_SESSION", "s1");
            std::env::set_var(crate::caller::SESSION_TOKEN_VARIABLE, "token");
            std::env::set_var("WF_WORKFORCE", "w1");
        }
        let run = Confinement::Session {
            directory: directory.path(),
        }
        .run(
            &CommandName::new("test").unwrap(),
            &argv,
            Duration::from_secs(30),
            &log,
        )
        .await
        .unwrap();
        assert_eq!(run.outcome, Outcome::Passed);
        assert_eq!(run.tail, "uncommitted\nunset\nnone");
        assert_eq!(run.log, log);
    }
}

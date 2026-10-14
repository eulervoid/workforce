//! Runs a command on code: a tree is checked out in a worktree, and the command runs there with
//! the sandbox and environment of agents and a timeout, its output kept in a log. What the outcome
//! means is up to whoever asked, such as a check before a merge.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use wf_core::name::CommandName;
use wf_core::session::GitOid;
use wf_harness::AcpAgentConfig;
use wf_vcs::{GitCli, VcsAdapter, VcsError};

use crate::machine_config::{EnvironmentConfig, SandboxConfig};
use crate::sandbox::Access;

/// How many lines of the end of the output a run returns.
const TAIL_LINES: usize = 40;
/// How much of the end of the log is read for those lines at most.
const TAIL_BYTES: u64 = 16 * 1024;

/// The variable that would put cargo's build output outside the worktree, where the builds of
/// other worktrees mix into it.
const CARGO_TARGET_DIR: &str = "CARGO_TARGET_DIR";

/// Where and how commands run: in one worktree, confined as an agent working there would be.
pub(crate) struct CommandRunner<'a> {
    pub git: &'a GitCli,
    /// The worktree, which [`Access::worktree`] names, and what else the sandbox allows.
    pub access: Access<'a>,
    pub sandbox: &'a SandboxConfig,
    pub environment: &'a EnvironmentConfig,
    /// Where Fence's settings for the command are written.
    pub settings: &'a Path,
    /// Where the logs are kept: a directory per name and a file per run.
    pub logs: &'a Path,
}

/// What came of a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    Passed,
    Failed(ExitStatus),
    /// It was stopped after the timeout, together with everything it started.
    TimedOut,
}

/// One run of a command.
#[derive(Debug, Clone)]
pub(crate) struct CommandRun {
    pub outcome: Outcome,
    /// The last lines of the output, stdout and stderr as they came.
    pub tail: String,
    /// The whole output.
    pub log: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum CommandError {
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
        let Some((program, arguments)) = argv.split_first() else {
            return Err(CommandError::Empty(name.clone()));
        };
        let git = self.git.clone();
        let worktree = self.access.worktree.to_path_buf();
        tokio::task::spawn_blocking(move || git.check_out_tree(&worktree, tree))
            .await
            .map_err(|error| CommandError::Io {
                path: self.access.worktree.to_path_buf(),
                source: std::io::Error::other(error),
            })??;

        let command =
            self.confined(AcpAgentConfig::new(program).args(arguments.iter().cloned()))?;
        let log = self.log_path(name, tree);
        let io_error = |source| CommandError::Io {
            path: log.clone(),
            source,
        };
        if let Some(parent) = log.parent() {
            std::fs::create_dir_all(parent).map_err(io_error)?;
        }
        let output = std::fs::File::create(&log).map_err(io_error)?;
        let errors = output.try_clone().map_err(io_error)?;
        let mut child = tokio::process::Command::new(command.command())
            .args(command.arguments())
            .envs(command.environment())
            .current_dir(self.access.worktree)
            .stdin(Stdio::null())
            .stdout(output)
            .stderr(errors)
            // Its own process group, so that whatever it started can be stopped with it.
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| CommandError::Io {
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
        // Nothing it left running may write to the worktree after the run, or into the next one.
        if let Some(group) = group {
            // The group may be gone already; then there is nothing left to stop.
            let _ = nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL);
        }
        if matches!(outcome, Outcome::TimedOut) {
            child.wait().await.map_err(io_error)?;
        }
        let tail = tail(&log).map_err(io_error)?;
        Ok(CommandRun { outcome, tail, log })
    }

    /// `command` in the sandbox and environment of agents, without the cargo target directory
    /// of the machine's environment.
    fn confined(&self, command: AcpAgentConfig) -> Result<AcpAgentConfig, CommandError> {
        let command = match self.sandbox {
            SandboxConfig::Fence(_) if !crate::sandbox::fence_installed() => {
                return Err(CommandError::NoFence);
            }
            SandboxConfig::Fence(config) => {
                crate::sandbox::wrap(config, &self.access, &command, self.settings).map_err(
                    |source| CommandError::Io {
                        path: self.settings.to_path_buf(),
                        source,
                    },
                )?
            }
            SandboxConfig::Off {} => command,
        };
        let mut environment = self.environment.clone();
        environment.set.remove(CARGO_TARGET_DIR);
        environment.inherit.retain(|name| name != CARGO_TARGET_DIR);
        Ok(crate::environment::confine(
            command,
            &environment,
            crate::environment::daemon_variables(),
        ))
    }

    /// The log of a run of `name` on `tree` that starts now.
    fn log_path(&self, name: &CommandName, tree: GitOid) -> PathBuf {
        let started = jiff::Timestamp::now().strftime("%Y%m%dT%H%M%S%.3fZ");
        let tree = tree.to_string();
        self.logs
            .join(name.as_str())
            .join(format!("{started}-{}.log", &tree[..8]))
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
            let (worktree, logs, settings) = (
                self.path("check"),
                self.path("logs"),
                self.path("check.json"),
            );
            let runner = CommandRunner {
                git: &self.git,
                access: Access {
                    worktree: &worktree,
                    repository: self.git.repository(),
                    socket: Path::new("/nonexistent/daemon.sock"),
                    data_dir: self.root.path(),
                    machine_config: None,
                },
                sandbox: &SandboxConfig::Off {},
                environment,
                settings: &settings,
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
}

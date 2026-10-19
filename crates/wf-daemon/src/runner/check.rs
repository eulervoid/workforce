//! The checks of the policy, run on the merge result in the workforce's check worktree before
//! the work is reviewed and again before the target moves.

use std::path::{Path, PathBuf};
use std::time::Duration;

use wf_core::event::{CommentKind, Commented, EventBody};
use wf_core::id::{ItemId, WorkforceId};
use wf_core::identity::{Actor, PrincipalKey};
use wf_core::lifecycle::AfterFailedChecks;
use wf_core::policy::CheckConfig;
use wf_core::session::{CommitId, GitOid};
use wf_core::state::State;
use wf_vcs::GitCli;

use super::merge::Attempt;
use super::{SessionError, blocking};
use crate::command::{CommandRun, CommandRunner, Confinement, Outcome};
use crate::machine_config::{CheckRunConfig, MachineConfig, SandboxConfig};
use crate::{Daemon, Loaded};

/// When checks run, which decides the sandbox they run in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CheckStage {
    /// As the work is submitted, before anyone reviewed it: always in the sandbox of every agent,
    /// whatever `[check] sandbox` says, since the code is not reviewed yet.
    Submission,
    /// Before the target moves: in the sandbox of checks on this machine, which may be off.
    Merge,
}

impl CheckStage {
    fn sandbox(self, config: &MachineConfig) -> &SandboxConfig {
        match self {
            CheckStage::Submission => &config.sandbox,
            CheckStage::Merge => config.check_sandbox(),
        }
    }
}

/// A check that did not pass on the merge result, and whose fault that is.
pub(super) struct FailedCheck {
    pub check: CheckConfig,
    /// The merge result.
    pub tree: GitOid,
    /// The run on the merge result.
    pub run: CommandRun,
    /// The sandbox it ran in.
    pub sandbox: SandboxConfig,
    pub blame: Blame,
}

impl FailedCheck {
    /// What failed, and what can be done about it, said to whoever is to resolve it.
    pub(super) fn told(&self, target: &str) -> String {
        let name = &self.check.name;
        let what = match self.blame {
            Blame::Work => format!("The check `{name}` fails on the merge into {target}."),
            Blame::Target => format!(
                "The check `{name}` fails on {target} itself, not only on the merge with this work. \
                 Resolve this once {target} passes it, and the merge is checked again."
            ),
            Blame::TimedOut(timeout) => format!(
                "The check `{name}` ran longer than {} on the merge into {target} and was stopped. \
                 Resolve this to check it again; the policy's timeout or the machine's \
                 `[check] max_timeout` may be too short.",
                humantime::format_duration(timeout)
            ),
        };
        let sandbox = match self.sandbox {
            SandboxConfig::Fence(_) => "Fence",
            SandboxConfig::Off {} => "none",
        };
        format!(
            "{what}\n\nCommand: `{}`\nTree: {}\nSandbox: {sandbox}\nLog: {}\n\n```\n{}\n```",
            self.check.run.join(" "),
            self.tree,
            self.run.log.display(),
            self.run.tail
        )
    }
}

/// What `failed` say, one after the other.
pub(super) fn told_all<'a>(
    failed: impl IntoIterator<Item = &'a FailedCheck>,
    target: &str,
) -> String {
    let told: Vec<_> = failed
        .into_iter()
        .map(|failed| failed.told(target))
        .collect();
    told.join("\n\n")
}

/// Why a check did not pass on the merge result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Blame {
    /// It passes on the target's own tree: the work broke it.
    Work,
    /// It fails on the target's own tree as well.
    Target,
    /// It ran longer than it may on this machine, and was stopped.
    TimedOut(Duration),
}

/// The checks that passed on this machine, by tree, command and sandbox: one empty file each. A
/// check's result depends on nothing else, so one that passed on a tree in a sandbox does not run
/// there again.
struct PassedChecks {
    directory: PathBuf,
}

impl PassedChecks {
    fn contains(&self, tree: GitOid, check: &CheckConfig, sandbox: &SandboxConfig) -> bool {
        self.path(tree, check, sandbox).exists()
    }

    fn insert(
        &self,
        tree: GitOid,
        check: &CheckConfig,
        sandbox: &SandboxConfig,
    ) -> Result<(), SessionError> {
        let path = self.path(tree, check, sandbox);
        std::fs::create_dir_all(&self.directory)
            .and_then(|()| std::fs::write(&path, ""))
            .map_err(|source| SessionError::Io { path, source })
    }

    fn path(&self, tree: GitOid, check: &CheckConfig, sandbox: &SandboxConfig) -> PathBuf {
        let mut key = blake3::Hasher::new();
        key.update(tree.as_bytes());
        for argument in &check.run {
            // Each argument with its length, so that no two commands share a key.
            key.update(&(argument.len() as u64).to_le_bytes());
            key.update(argument.as_bytes());
        }
        // What the sandbox allows is all of its debug form, so that a changed config is another
        // sandbox.
        key.update(format!("{sandbox:?}").as_bytes());
        self.directory.join(key.finalize().to_hex().as_str())
    }
}

/// Runs checks in the check worktree, in one sandbox.
struct Checker<'a> {
    runner: CommandRunner<'a>,
    passed: PassedChecks,
    limits: &'a CheckRunConfig,
    sandbox: &'a SandboxConfig,
}

impl Checker<'_> {
    /// Runs `check` on `tree`, unless it passed there already. `None` when it passes.
    async fn run(
        &self,
        check: &CheckConfig,
        tree: GitOid,
    ) -> Result<Option<CommandRun>, SessionError> {
        if self.passed.contains(tree, check, self.sandbox) {
            return Ok(None);
        }
        let run = self
            .runner
            .run(tree, &check.name, &check.run, self.limits.timeout(check))
            .await?;
        match run.outcome {
            Outcome::Passed => {
                self.passed.insert(tree, check, self.sandbox)?;
                Ok(None)
            }
            Outcome::Failed(_) | Outcome::TimedOut => Ok(Some(run)),
        }
    }
}

impl Daemon {
    /// Runs every one of `checks` on `tree`, the merge of the work into the target at `base`, in
    /// order, in the sandbox `stage` says, and returns those that did not pass. A check that failed
    /// runs on the target's own tree too, to tell whose fault it is.
    pub(super) async fn check_merge(
        &self,
        workforce: WorkforceId,
        repository: &Path,
        checks: &[CheckConfig],
        base: CommitId,
        tree: GitOid,
        stage: CheckStage,
    ) -> Result<Vec<FailedCheck>, SessionError> {
        if checks.is_empty() {
            return Ok(Vec::new());
        }
        // One run at a time per workforce: they share the check worktree.
        let _running = self.checking.lock(workforce).await;
        let config = self.machine_config()?;
        let git = {
            let repository = repository.to_path_buf();
            blocking(move || GitCli::open(&repository)).await??
        };
        let sandbox = stage.sandbox(&config);
        let directory = self.workforce_directory(workforce);
        let (worktree, profile_file, git_config, logs) = (
            directory.join("check"),
            directory.join("sandbox").join("check.json"),
            directory.join("sandbox").join("check.gitconfig"),
            directory.join("check-logs"),
        );
        let checker = Checker {
            runner: CommandRunner {
                git: &git,
                confinement: Confinement::for_checks(
                    crate::sandbox::Access {
                        worktree: &worktree,
                        repository,
                        branch: None,
                        socket: &self.launch.socket,
                        data_dir: &self.launch.data_dir,
                        machine_config: self.launch.machine_config.as_deref(),
                    },
                    sandbox,
                    &config.environment,
                    &profile_file,
                    &git_config,
                ),
                logs: &logs,
            },
            passed: PassedChecks {
                directory: directory.join("checks-passed"),
            },
            limits: &config.check,
            sandbox,
        };

        let mut failed = Vec::new();
        for check in checks {
            if let Some(run) = checker.run(check, tree).await? {
                failed.push((check, run));
            }
        }
        if failed.is_empty() {
            return Ok(Vec::new());
        }
        let target_tree = {
            let git = git.clone();
            blocking(move || git.tree_of(base)).await??
        };
        let mut blamed = Vec::new();
        for (check, run) in failed {
            let blame = match run.outcome {
                Outcome::TimedOut => Blame::TimedOut(config.check.timeout(check)),
                Outcome::Passed | Outcome::Failed(_) if target_tree == tree => Blame::Target,
                Outcome::Passed | Outcome::Failed(_) => {
                    match checker.run(check, target_tree).await? {
                        None => Blame::Work,
                        Some(_) => Blame::Target,
                    }
                }
            };
            blamed.push(FailedCheck {
                check: check.clone(),
                tree,
                run,
                sandbox: sandbox.clone(),
                blame,
            });
        }
        Ok(blamed)
    }
}

impl Daemon {
    /// Writes `text` on `item` as the daemon, naming the principal so that a blocker waits on
    /// them and not on a supervisor.
    pub(super) async fn comment_to_principal(
        &self,
        loaded: &Loaded,
        state: &State,
        principal: PrincipalKey,
        item: ItemId,
        kind: CommentKind,
        text: String,
    ) -> Result<(), SessionError> {
        let text = match state.member(&principal) {
            Some(member) => format!("@{} {text}", member.handle),
            None => text,
        };
        self.author(
            loaded,
            Actor::Daemon(principal),
            EventBody::Commented(Commented {
                kind,
                ..Commented::plain(item, text)
            }),
        )
        .await?;
        Ok(())
    }

    /// Tells of the checks that failed on the merge of the work of `item`, each to whoever can
    /// resolve it: what the work broke goes back to work as a change request per check, unless
    /// that happened too often since the principal last acted; what is not the work's fault, or
    /// what waits for the principal, is a blocker that keeps the item in `review`.
    pub(super) async fn checks_failed(
        &self,
        loaded: &Loaded,
        principal: PrincipalKey,
        item: ItemId,
        attempt: Attempt,
        target: &str,
        failed: Vec<FailedCheck>,
    ) -> Result<(), SessionError> {
        // The checks took a while: the principal may have acted on the item meanwhile.
        let state = self.snapshot(loaded).await?;
        let comment = |kind: CommentKind, text: String| {
            self.author(
                loaded,
                Actor::Daemon(principal),
                EventBody::Commented(Commented {
                    kind,
                    ..Commented::plain(item, text)
                }),
            )
        };
        // The principal is named, so that the blocker waits on them and not on a supervisor.
        let to_principal = |text: String| match state.member(&principal) {
            Some(member) => format!("@{} {text}", member.handle),
            None => text,
        };
        let (by_work, not_by_work): (Vec<_>, Vec<_>) = failed
            .into_iter()
            .partition(|failed| failed.blame == Blame::Work);
        if by_work.is_empty() {
            for failed in &not_by_work {
                comment(CommentKind::Blocker, to_principal(failed.told(target))).await?;
            }
            return Ok(());
        }
        match state.after_failed_checks(item) {
            AfterFailedChecks::BackToWork { to } => {
                comment(CommentKind::CheckFailure, told_all(&by_work, target)).await?;
                if !not_by_work.is_empty() {
                    let text = format!(
                        "Not sent back for these, which are not the work's fault:\n\n{}",
                        told_all(&not_by_work, target)
                    );
                    comment(CommentKind::Plain, to_principal(text)).await?;
                }
                self.move_item(loaded, principal, item, to, None, attempt.by_session())
                    .await?;
            }
            AfterFailedChecks::WaitsForPrincipal { runs } => {
                let all: Vec<_> = by_work.iter().chain(&not_by_work).collect();
                let text = format!(
                    "The checks failed on the merge into {target} again, after {runs} failed runs \
                     since you last acted on the item, so the work is not sent back by itself. \
                     Resolve this to check it again and send it back with what fails.\n\n{}",
                    told_all(all, target)
                );
                comment(CommentKind::Blocker, to_principal(text)).await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(run: &[&str]) -> CheckConfig {
        CheckConfig {
            name: wf_core::name::CommandName::new("test").unwrap(),
            run: run.iter().map(|argument| argument.to_string()).collect(),
            timeout: Duration::from_secs(60),
        }
    }

    #[test]
    fn checks_at_submission_run_in_the_sandbox_of_agents_whatever_checks_get_at_the_merge() {
        let mut config = MachineConfig::default();
        config.check.sandbox = Some(SandboxConfig::Off {});
        assert_eq!(
            CheckStage::Submission.sandbox(&config),
            &SandboxConfig::default()
        );
        assert_eq!(CheckStage::Merge.sandbox(&config), &SandboxConfig::Off {});
    }

    #[test]
    fn a_pass_is_kept_for_its_tree_command_and_sandbox_only() {
        let directory = tempfile::tempdir().unwrap();
        let passed = PassedChecks {
            directory: directory.path().join("checks-passed"),
        };
        let (tree, other_tree) = (GitOid::from_bytes([1; 20]), GitOid::from_bytes([2; 20]));
        let test = check(&["cargo", "test"]);
        let fence = SandboxConfig::default();
        assert!(!passed.contains(tree, &test, &fence));
        passed.insert(tree, &test, &fence).unwrap();
        assert!(passed.contains(tree, &test, &fence));
        assert!(!passed.contains(other_tree, &test, &fence));
        assert!(!passed.contains(tree, &check(&["cargo", "test", "--release"]), &fence));
        // The arguments are kept apart, not joined.
        assert!(!passed.contains(tree, &check(&["cargo test"]), &fence));
        assert!(!passed.contains(tree, &check(&["cargot", "est"]), &fence));
        // A pass in one sandbox is not one in another.
        assert!(!passed.contains(tree, &test, &SandboxConfig::Off {}));
    }
}

//! The checks of the policy, run on the merge result in the workforce's check worktree before
//! the target moves.

use std::path::{Path, PathBuf};
use std::time::Duration;

use wf_core::event::{Comment, CommentKind, EventBody};
use wf_core::id::{ItemId, WorkforceId};
use wf_core::identity::{Actor, PrincipalKey};
use wf_core::lifecycle::AfterFailedChecks;
use wf_core::policy::CheckConfig;
use wf_core::session::{CommitId, GitOid};
use wf_vcs::{GitCli, VcsAdapter};

use super::merge::Attempt;
use super::{SessionError, blocking};
use crate::command::{CommandRun, CommandRunner, Outcome};
use crate::machine_config::CheckLimitsConfig;
use crate::{Daemon, Loaded};

/// A check that did not pass on the merge result, and whose fault that is.
pub(super) struct FailedCheck {
    pub check: CheckConfig,
    /// The merge result.
    pub tree: GitOid,
    /// The run on the merge result.
    pub run: CommandRun,
    pub blame: Blame,
}

impl FailedCheck {
    /// What failed, and what can be done about it, said to whoever is to resolve it.
    fn told(&self, target: &str) -> String {
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
        format!(
            "{what}\n\nCommand: `{}`\nTree: {}\nLog: {}\n\n```\n{}\n```",
            self.check.run.join(" "),
            self.tree,
            self.run.log.display(),
            self.run.tail
        )
    }
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

/// The checks that passed on this machine, by tree and command: one empty file each. A check's
/// result depends on nothing else, so one that passed on a tree does not run there again.
struct PassedChecks {
    directory: PathBuf,
}

impl PassedChecks {
    fn contains(&self, tree: GitOid, check: &CheckConfig) -> bool {
        self.path(tree, check).exists()
    }

    fn insert(&self, tree: GitOid, check: &CheckConfig) -> Result<(), SessionError> {
        let path = self.path(tree, check);
        std::fs::create_dir_all(&self.directory)
            .and_then(|()| std::fs::write(&path, ""))
            .map_err(|source| SessionError::Io { path, source })
    }

    fn path(&self, tree: GitOid, check: &CheckConfig) -> PathBuf {
        let mut key = blake3::Hasher::new();
        key.update(tree.as_bytes());
        for argument in &check.run {
            // Each argument with its length, so that no two commands share a key.
            key.update(&(argument.len() as u64).to_le_bytes());
            key.update(argument.as_bytes());
        }
        self.directory.join(key.finalize().to_hex().as_str())
    }
}

/// Runs checks in the check worktree, as an agent working there would run them.
struct Checker<'a> {
    runner: CommandRunner<'a>,
    passed: PassedChecks,
    limits: &'a CheckLimitsConfig,
}

impl Checker<'_> {
    /// Runs `check` on `tree`, unless it passed there already. `None` when it passes.
    async fn run(
        &self,
        check: &CheckConfig,
        tree: GitOid,
    ) -> Result<Option<CommandRun>, SessionError> {
        if self.passed.contains(tree, check) {
            return Ok(None);
        }
        let run = self
            .runner
            .run(tree, &check.name, &check.run, self.limits.timeout(check))
            .await?;
        match run.outcome {
            Outcome::Passed => {
                self.passed.insert(tree, check)?;
                Ok(None)
            }
            Outcome::Failed(_) | Outcome::TimedOut => Ok(Some(run)),
        }
    }
}

impl Daemon {
    /// Runs every one of `checks` on `tree`, the merge of the work into the target at `base`, in
    /// order, and returns those that did not pass. A check that failed runs on the target's own
    /// tree too, to tell whose fault it is.
    pub(super) async fn check_merge(
        &self,
        workforce: WorkforceId,
        repository: &Path,
        checks: &[CheckConfig],
        base: CommitId,
        tree: GitOid,
    ) -> Result<Vec<FailedCheck>, SessionError> {
        if checks.is_empty() {
            return Ok(Vec::new());
        }
        let config = self.machine_config()?;
        let git = GitCli::open(repository)?;
        let directory = self.workforce_directory(workforce);
        let (worktree, settings, logs) = (
            directory.join("check"),
            directory.join("sandbox").join("check.json"),
            directory.join("check-logs"),
        );
        let checker = Checker {
            runner: CommandRunner {
                git: &git,
                access: crate::sandbox::Access {
                    worktree: &worktree,
                    repository,
                    socket: &self.config.socket,
                    data_dir: &self.config.data_dir,
                    machine_config: self.config.machine_config.as_deref(),
                },
                sandbox: &config.sandbox,
                environment: &config.environment,
                settings: &settings,
                logs: &logs,
            },
            passed: PassedChecks {
                directory: directory.join("checks-passed"),
            },
            limits: &config.check,
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
                blame,
            });
        }
        Ok(blamed)
    }
}

impl Daemon {
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
                EventBody::Commented(Comment {
                    item,
                    text,
                    kind,
                    resolves: Vec::new(),
                    step: None,
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
                for failed in &by_work {
                    comment(CommentKind::ChangeRequest, failed.told(target)).await?;
                }
                if !not_by_work.is_empty() {
                    let told: Vec<_> = not_by_work
                        .iter()
                        .map(|failed| failed.told(target))
                        .collect();
                    let text = format!(
                        "Not sent back for these, which are not the work's fault:\n\n{}",
                        told.join("\n\n")
                    );
                    comment(CommentKind::Plain, to_principal(text)).await?;
                }
                self.move_item(loaded, principal, item, to, None, attempt.by_session())
                    .await?;
            }
            AfterFailedChecks::WaitsForPrincipal { runs } => {
                let told: Vec<_> = by_work
                    .iter()
                    .chain(&not_by_work)
                    .map(|failed| failed.told(target))
                    .collect();
                let text = format!(
                    "The checks failed on the merge into {target} again, after {runs} failed runs \
                     since you last acted on the item, so the work is not sent back by itself. \
                     Resolve this to check it again and send it back with what fails.\n\n{}",
                    told.join("\n\n")
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
    fn a_pass_is_kept_for_its_tree_and_command_only() {
        let directory = tempfile::tempdir().unwrap();
        let passed = PassedChecks {
            directory: directory.path().join("checks-passed"),
        };
        let (tree, other_tree) = (GitOid::from_bytes([1; 20]), GitOid::from_bytes([2; 20]));
        let test = check(&["cargo", "test"]);
        assert!(!passed.contains(tree, &test));
        passed.insert(tree, &test).unwrap();
        assert!(passed.contains(tree, &test));
        assert!(!passed.contains(other_tree, &test));
        assert!(!passed.contains(tree, &check(&["cargo", "test", "--release"])));
        // The arguments are kept apart, not joined.
        assert!(!passed.contains(tree, &check(&["cargo test"])));
        assert!(!passed.contains(tree, &check(&["cargot", "est"])));
    }
}

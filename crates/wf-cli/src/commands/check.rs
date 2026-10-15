//! `wf check`: the checks of the policy, run on the checkout here as the merge runs them.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, bail};
use serde::Serialize;
use wf_core::name::CommandName;
use wf_core::policy::CheckConfig;
use wf_daemon::command::{Access, CommandRun, Confinement, Outcome};
use wf_daemon::machine_config::MachineConfig;

use super::emit;
use crate::theme;
use crate::workspace::Workspace;

#[derive(Serialize)]
struct CheckView {
    name: String,
    command: Vec<String>,
    #[serde(flatten)]
    result: CheckResult,
}

/// What came of a check; what did not pass keeps the end of its output and a log of all of it.
#[derive(Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum CheckResult {
    Passed,
    Failed {
        /// Such as `exit status: 101`.
        status: String,
        tail: String,
        log: String,
    },
    TimedOut {
        /// How long it ran before it was stopped, such as `20m`.
        timeout: String,
        tail: String,
        log: String,
    },
}

impl CheckView {
    fn new(check: &CheckConfig, timeout: Duration, run: CommandRun) -> Self {
        let (tail, log) = (run.tail, run.log.display().to_string());
        let result = match run.outcome {
            Outcome::Passed => CheckResult::Passed,
            Outcome::Failed(status) => CheckResult::Failed {
                status: status.to_string(),
                tail,
                log,
            },
            Outcome::TimedOut => CheckResult::TimedOut {
                timeout: humantime::format_duration(timeout).to_string(),
                tail,
                log,
            },
        };
        Self {
            name: check.name.to_string(),
            command: check.run.clone(),
            result,
        }
    }

    fn passed(&self) -> bool {
        match self.result {
            CheckResult::Passed => true,
            CheckResult::Failed { .. } | CheckResult::TimedOut { .. } => false,
        }
    }
}

/// Runs the checks of the policy named in `names`, or all of them, one after another in the
/// checkout that contains `cwd`. Fails when one does not pass.
pub(super) fn check(
    json: bool,
    workspace: &Workspace,
    cwd: &Path,
    names: Vec<CommandName>,
) -> anyhow::Result<()> {
    let policy = &workspace
        .state()
        .policy()
        .expect("a workforce has a policy")
        .policy;
    let checks = chosen(&policy.merge.checks, &names)?;
    if checks.is_empty() {
        return emit(json, &Vec::<CheckView>::new(), |_| {
            "the policy has no checks: `[[merge.check]]`\n".to_string()
        });
    }
    let directory = wf_vcs::GitCli::checkout_root(cwd)?;
    // The logs of failed checks stay for reading; the rest goes with the directory.
    let scratch = tempfile::Builder::new().prefix("wf-check-").tempdir()?;
    let machine = Machine::here(workspace, &directory, scratch.path())?;
    let confinement = machine.confinement();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    let mut views = Vec::new();
    for check in checks {
        let timeout = machine.timeout(check);
        let log = scratch
            .path()
            .join("logs")
            .join(format!("{}.log", check.name));
        let run = runtime.block_on(confinement.run(&check.name, &check.run, timeout, &log))?;
        let view = CheckView::new(check, timeout, run);
        if !json {
            anstream::print!("{}", render(&view));
        }
        views.push(view);
    }

    let failed: Vec<&str> = views
        .iter()
        .filter(|view| !view.passed())
        .map(|view| view.name.as_str())
        .collect();
    if json {
        emit(json, &views, |_| String::new())?;
    }
    if failed.is_empty() {
        return Ok(());
    }
    let _ = scratch.keep();
    bail!(
        "{} of {} checks did not pass: {}",
        failed.len(),
        views.len(),
        failed.join(", ")
    )
}

/// The checks of the policy that `names` names, in the policy's order; all of them without
/// names.
fn chosen<'a>(
    checks: &'a [CheckConfig],
    names: &[CommandName],
) -> anyhow::Result<Vec<&'a CheckConfig>> {
    if let Some(unknown) = names
        .iter()
        .find(|name| !checks.iter().any(|check| &check.name == *name))
    {
        let known: Vec<String> = checks.iter().map(|check| check.name.to_string()).collect();
        match known.as_slice() {
            [] => bail!("the policy has no checks, and so no check `{unknown}`"),
            _ => bail!(
                "the policy has no check `{unknown}`; it has {}",
                known.join(", ")
            ),
        }
    }
    Ok(checks
        .iter()
        .filter(|check| names.is_empty() || names.contains(&check.name))
        .collect())
}

/// How the checks run on this machine.
enum Machine<'a> {
    /// In a session, in the sandbox and environment of its agent already. The machine config is
    /// out of its reach, so the policy's timeouts apply as they are.
    Session { directory: &'a Path },
    /// Outside a session: confined as the machine config confines agents.
    Agents {
        directory: &'a Path,
        repository: std::path::PathBuf,
        socket: std::path::PathBuf,
        data_dir: std::path::PathBuf,
        config_path: std::path::PathBuf,
        config: Box<MachineConfig>,
        /// Where Fence's settings are written.
        settings: std::path::PathBuf,
    },
}

impl<'a> Machine<'a> {
    fn here(workspace: &Workspace, directory: &'a Path, scratch: &Path) -> anyhow::Result<Self> {
        if workspace.session().is_some() {
            return Ok(Machine::Session { directory });
        }
        let config_path = MachineConfig::path_from_env().context("HOME is not set")?;
        let config = Box::new(MachineConfig::load(&config_path)?);
        Ok(Machine::Agents {
            directory,
            repository: wf_vcs::GitCli::open(directory)?.repository().to_path_buf(),
            socket: workspace.data.daemon_socket(),
            data_dir: workspace.data.root().to_path_buf(),
            config_path,
            config,
            settings: scratch.join("fence.json"),
        })
    }

    fn confinement(&self) -> Confinement<'_> {
        match self {
            Machine::Session { directory } => Confinement::Session { directory },
            Machine::Agents {
                directory,
                repository,
                socket,
                data_dir,
                config_path,
                config,
                settings,
            } => Confinement::Agents {
                access: Access {
                    worktree: directory,
                    repository,
                    socket,
                    data_dir,
                    machine_config: Some(config_path),
                },
                sandbox: &config.sandbox,
                environment: &config.environment,
                settings,
            },
        }
    }

    fn timeout(&self, check: &CheckConfig) -> Duration {
        match self {
            Machine::Session { .. } => check.timeout,
            Machine::Agents { config, .. } => config.check.timeout(check),
        }
    }
}

/// `✓ fmt`, or `✗ clippy` with why, the end of its output and where the rest is.
fn render(view: &CheckView) -> String {
    let (why, tail, log) = match &view.result {
        CheckResult::Passed => {
            return format!(
                "{}✓{} {}\n",
                theme::SUCCESS.render(),
                theme::SUCCESS.render_reset(),
                view.name
            );
        }
        CheckResult::Failed { status, tail, log } => (status.clone(), tail, log),
        CheckResult::TimedOut { timeout, tail, log } => {
            (format!("stopped after {timeout}"), tail, log)
        }
    };
    let (dim, dim_reset) = (theme::DIM.render(), theme::DIM.render_reset());
    format!(
        "{}✗{} {}  {dim}{why}{dim_reset}\n  {}{}{}\n\n{tail}\n\n  {dim}log: {log}{dim_reset}\n",
        theme::ERROR.render(),
        theme::ERROR.render_reset(),
        view.name,
        theme::CODE.render(),
        view.command.join(" "),
        theme::CODE.render_reset(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(name: &str) -> CheckConfig {
        CheckConfig {
            name: CommandName::new(name).unwrap(),
            run: vec!["true".to_string()],
            timeout: Duration::from_secs(60),
        }
    }

    fn names(names: &[&str]) -> Vec<CommandName> {
        names
            .iter()
            .map(|name| CommandName::new(*name).unwrap())
            .collect()
    }

    #[test]
    fn named_checks_run_in_the_policys_order_and_all_without_names() {
        let checks = [check("fmt"), check("clippy"), check("test")];
        let chosen_names = |given: &[&str]| -> Vec<String> {
            chosen(&checks, &names(given))
                .unwrap()
                .iter()
                .map(|check| check.name.to_string())
                .collect()
        };
        assert_eq!(chosen_names(&[]), ["fmt", "clippy", "test"]);
        assert_eq!(chosen_names(&["test", "fmt"]), ["fmt", "test"]);
    }

    #[test]
    fn a_name_the_policy_does_not_have_is_refused_with_those_it_has() {
        let checks = [check("fmt"), check("clippy")];
        let error = chosen(&checks, &names(&["fmt", "lint"])).unwrap_err();
        assert_eq!(
            error.to_string(),
            "the policy has no check `lint`; it has fmt, clippy"
        );
    }
}

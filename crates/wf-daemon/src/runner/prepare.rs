//! Preparing a new agent repository before its agent starts, with the command `[prepare]` in the
//! machine config names, such as one that copies a build cache into it. wf only runs it: what it
//! does is the principal's.

use std::borrow::Cow;
use std::path::Path;
use std::process::ExitStatus;
use std::time::Duration;

use wf_core::id::SessionId;
use wf_core::name::CommandName;

use super::SessionError;
use super::start::Setup;
use crate::Daemon;
use crate::command::{Confinement, Outcome};
use crate::machine_config::PrepareConfig;
use crate::sandbox::Access;

/// The variable that names the check worktree of the workforce to the command: the checks build
/// there, so its build output is close to the tip of the target.
const CHECK_DIRECTORY: &str = "WF_CHECK_DIRECTORY";

/// How the command of `[prepare]` did not pass.
#[derive(Debug)]
pub enum PrepareFailure {
    Failed(ExitStatus),
    TimedOut(Duration),
}

impl std::fmt::Display for PrepareFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PrepareFailure::Failed(status) => write!(formatter, "failed with {status}"),
            PrepareFailure::TimedOut(timeout) => write!(
                formatter,
                "was stopped after {}",
                humantime::format_duration(*timeout)
            ),
        }
    }
}

impl Daemon {
    /// Runs the command of `prepare` in `directory`, the repository just made for `session`,
    /// confined as the session's agent is, with the check worktree named in [`CHECK_DIRECTORY`].
    /// Its log is kept when it does not pass.
    pub(super) async fn prepare_repository(
        &self,
        setup: &Setup,
        session: SessionId,
        directory: &Path,
        prepare: &PrepareConfig,
    ) -> Result<(), SessionError> {
        let workforce_directory = self.workforce_directory(setup.workforce);
        let mut environment = setup.environment.clone();
        environment.set.insert(
            CHECK_DIRECTORY.to_string(),
            workforce_directory
                .join("check")
                .to_string_lossy()
                .into_owned(),
        );
        let sandbox_directory = workforce_directory.join("sandbox");
        let (profile_file, git_config) = (
            sandbox_directory.join(format!("prepare-{session}.json")),
            sandbox_directory.join(format!("prepare-{session}.gitconfig")),
        );
        let log = workforce_directory
            .join("prepare-logs")
            .join(format!("{session}.log"));
        let confinement = Confinement::Agents {
            access: Access {
                directory,
                socket: &self.launch.socket,
                data_dir: &self.launch.data_dir,
                machine_config: self.launch.machine_config.as_deref(),
                harness: None,
            },
            sandbox: &setup.sandbox,
            environment: Cow::Owned(environment),
            profile_file: &profile_file,
            git_config: &git_config,
        };
        let name = CommandName::new("prepare").expect("`prepare` is a command name");
        let run = confinement
            .run(&name, &prepare.run, prepare.timeout, &log)
            .await;
        for file in [&profile_file, &git_config] {
            // Not there when the command did not get as far as writing it.
            let _ = std::fs::remove_file(file);
        }
        let run = run.map_err(SessionError::PrepareCommand)?;
        let failure = match run.outcome {
            Outcome::Passed => {
                let _ = std::fs::remove_file(&log);
                return Ok(());
            }
            Outcome::Failed(status) => PrepareFailure::Failed(status),
            Outcome::TimedOut => PrepareFailure::TimedOut(prepare.timeout),
        };
        Err(SessionError::PrepareFailed {
            failure,
            tail: run.tail,
            log: run.log,
        })
    }
}

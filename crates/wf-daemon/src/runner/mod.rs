//! Running sessions: the daemon starts a harness for a work session, prompts it turn after turn
//! until the session ends, and applies what follows from the end to the item.

mod brief;
mod check;
mod check_turns;
mod claims;
mod dispatch;
mod drive;
mod end;
mod merge;
mod offered;
mod repositories;
mod resume;
mod start;
mod submit;
mod target;

pub(crate) use check_turns::CheckTurns;
pub(crate) use dispatch::run_dispatcher;
pub(crate) use merge::{MergeQueue, run_merger};
pub use resume::{copy_session_record, session_model};
pub(crate) use start::sandboxed_in;
pub(crate) use submit::SubmissionChecks;
pub(crate) use target::{TargetBranch, run_target};

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use wf_core::claims::ItemClaimed;
use wf_core::event::{Event, EventBody};
use wf_core::id::{HashId, ItemId, SessionId, WorkforceId};
use wf_core::identity::Actor;
use wf_core::lifecycle::Interruption;
use wf_core::name::{AgentName, HarnessName};
use wf_core::session::{CommitId, SessionRole};
use wf_core::state::State;
use wf_harness::{AcpAgentConfig, Permissions};
use wf_store::{AuthorError, ReceiveError};
use wf_vcs::{GitCli, VcsError};

use crate::command::CommandError;
use crate::machine_config::{MachineConfig, MachineConfigError, SandboxConfig};
use crate::{Daemon, Loaded, StorageClosed};

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("this machine is not bound to a member of the workforce")]
    NotBound,
    #[error("`{0}` is not an agent of this machine's member")]
    UnknownAgent(AgentName),
    #[error("harness `{0}` is not one wf knows how to start; see `wf agent list`")]
    UnknownHarness(HarnessName),
    #[error("the workforce has no policy yet")]
    NoPolicy,
    #[error("there is no item #{}", .0.short())]
    UnknownItem(ItemId),
    #[error("#{} is archived", .0.short())]
    Archived(ItemId),
    #[error(transparent)]
    Claimed(#[from] ItemClaimed),
    #[error("a {} session is already starting on #{}", .1.name(), .0.short())]
    Starting(ItemId, SessionRole),
    #[error("commit {} has not arrived from the device that has it yet", .0.short())]
    AwaitingCommit(CommitId),
    #[error("there is no session {}", .0.short())]
    UnknownSession(SessionId),
    #[error("the session runs on another machine")]
    NotOurs(SessionId),
    #[error("only a work session can be started this way")]
    NotWork,
    #[error("{0}")]
    Vcs(#[from] VcsError),
    #[error(
        "the branch does not contain the tip of `{target}`, which moved since it was rebased; \
         submit again to rebase it onto the new tip"
    )]
    BehindTarget { target: String },
    #[error(
        "the checks of the submission are running; you are told when they are done, so end your \
         turn"
    )]
    ChecksRunning,
    #[error(transparent)]
    Authoring(#[from] AuthorError),
    #[error(transparent)]
    Storage(#[from] StorageClosed),
    #[error(transparent)]
    MachineConfig(#[from] MachineConfigError),
    #[error(transparent)]
    SigningKey(#[from] wf_vcs::signing::SigningKeyError),
    #[error("cannot run a check: {0}")]
    Check(#[from] CommandError),
    #[error(
        "agents run in Fence on this machine, but `fence` is not on PATH; install it, or set \
         `kind = \"off\"` under `[sandbox]` in the machine config to run them without a sandbox"
    )]
    NoFence,
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("a background task did not finish: {0}")]
    Task(#[from] tokio::task::JoinError),
}

impl SessionError {
    /// Whether something could not be written, so that the same call may succeed later, for
    /// example once there is space on the disk again.
    pub(crate) fn is_failed_write(&self) -> bool {
        match self {
            Self::Authoring(AuthorError::Receive(ReceiveError::Store(_)))
            | Self::Storage(_)
            | Self::Check(CommandError::Io { .. })
            | Self::Io { .. } => true,
            Self::Authoring(
                AuthorError::Receive(ReceiveError::Invalid(_))
                | AuthorError::Build(_)
                | AuthorError::Policy(_)
                | AuthorError::PolicyHlcInFuture
                | AuthorError::OwnFeedEquivocated
                | AuthorError::Sign(_),
            )
            | Self::NotBound
            | Self::NoPolicy
            | Self::UnknownAgent(_)
            | Self::UnknownHarness(_)
            | Self::UnknownItem(_)
            | Self::Archived(_)
            | Self::Claimed(_)
            | Self::Starting(..)
            | Self::AwaitingCommit(_)
            | Self::UnknownSession(_)
            | Self::NotOurs(_)
            | Self::NotWork
            | Self::Vcs(_)
            | Self::BehindTarget { .. }
            | Self::ChecksRunning
            | Self::MachineConfig(_)
            | Self::SigningKey(_)
            | Self::Task(_)
            | Self::Check(
                CommandError::Empty(_)
                | CommandError::Vcs(_)
                | CommandError::NoFence
                | CommandError::Task(_),
            )
            | Self::NoFence => false,
        }
    }
}

/// Tells the task of a running session that it has ended and its harness is to be stopped.
#[derive(Clone, Copy)]
enum Stop {
    /// Cancel the turn that is running.
    Now,
    /// The agent ended the session from inside its turn, so it is still waiting for the answer
    /// of the command that did: let it finish the turn, within a grace period.
    AfterTurn,
}

/// How long an agent that ended its own session gets to finish its turn.
const GRACE: Duration = Duration::from_secs(20);

/// How long a harness may take to start and open its session. Wrappers such as `npx` may first
/// download the agent.
const START_TIMEOUT: Duration = Duration::from_secs(180);

/// How long a turn of an agent may go without a word from it, while no permission request of it
/// waits, before the session ends: the harness has stopped answering. Longer than any quiet tool
/// call, such as a build, should take.
pub(crate) const SILENCE_LIMIT: Duration = Duration::from_secs(20 * 60);

/// How long a cancelled turn may take to end before the harness is closed anyway.
const CANCEL_GRACE: Duration = Duration::from_secs(20);

/// The sessions this daemon is running. Lost with the process, which is why a daemon that starts
/// resumes every live session of its machine, or ends it.
#[derive(Default)]
pub(crate) struct Runners {
    running: Mutex<BTreeMap<SessionId, mpsc::UnboundedSender<Stop>>>,
    /// The tasks of the sessions, to wait for when the daemon halts.
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

/// How long a halting daemon waits for its sessions' harnesses to close.
const HALT_GRACE: Duration = Duration::from_secs(40);

impl Runners {
    fn add(&self, session: SessionId, stop: mpsc::UnboundedSender<Stop>) {
        self.lock().insert(session, stop);
    }

    fn track(&self, task: tokio::task::JoinHandle<()>) {
        let mut tasks = self
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        tasks.retain(|task| !task.is_finished());
        tasks.push(task);
    }

    /// Stops every session's harness and waits until they are closed, at most [`HALT_GRACE`].
    pub(crate) async fn halt(&self) {
        for (_, stop) in std::mem::take(&mut *self.lock()) {
            let _ = stop.send(Stop::Now);
        }
        let waited = tokio::time::Instant::now();
        while waited.elapsed() < HALT_GRACE {
            let running = {
                let mut tasks = self
                    .tasks
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                tasks.retain(|task| !task.is_finished());
                !tasks.is_empty()
            };
            if !running {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Stops the task of `session`, if it runs here.
    fn stop(&self, session: SessionId, how: Stop) {
        if let Some(stop) = self.lock().remove(&session) {
            let _ = stop.send(how);
        }
    }

    /// The sessions whose tasks run here.
    fn running(&self) -> Vec<SessionId> {
        self.lock().keys().copied().collect()
    }

    pub(crate) fn is_running(&self, session: SessionId) -> bool {
        self.lock().contains_key(&session)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<SessionId, mpsc::UnboundedSender<Stop>>> {
        self.running
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The sessions starting right now, by item and role, so that two starts of one session, such as
/// one by hand and one by the dispatcher, do not both see it free. Different roles on one item,
/// such as reviews of different steps, start side by side.
#[derive(Default)]
pub(crate) struct StartLocks {
    starting: Mutex<std::collections::BTreeSet<(ItemId, SessionRole)>>,
}

pub(crate) struct StartLock<'a> {
    locks: &'a StartLocks,
    key: (ItemId, SessionRole),
}

impl StartLocks {
    /// The lock of a session of `role` on `item`, or `None` while someone else holds it.
    fn take(&self, item: ItemId, role: &SessionRole) -> Option<StartLock<'_>> {
        let mut starting = self
            .starting
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let key = (item, role.clone());
        // Built only when taken: a lock built and dropped here would release the holder's, and
        // deadlock on `starting` besides.
        starting
            .insert(key.clone())
            .then(|| StartLock { locks: self, key })
    }
}

impl Drop for StartLock<'_> {
    fn drop(&mut self) {
        self.locks
            .starting
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.key);
    }
}

/// What a session is, to run it.
struct Run {
    workforce: WorkforceId,
    session: SessionId,
    item: ItemId,
    role: SessionRole,
    agent: AgentName,
    /// The harness of the agent, and the command that starts it.
    harness: HarnessName,
    command: AcpAgentConfig,
    model: Option<String>,
    /// The repository or directory the session runs in.
    directory: PathBuf,
    transcript: PathBuf,
    /// Where the agent's own id of the session is kept, to resume it after a restart.
    agent_session: PathBuf,
    begin: Begin,
    branch: String,
    target: String,
    /// The repository, opened once for the whole run.
    git: GitCli,
    nudges: u32,
    park_timeout: Duration,
    max_duration: Option<Duration>,
    permissions: Arc<dyn Permissions>,
    sandbox: SandboxConfig,
    /// What git in the agent's processes signs with and commits as.
    git_environment: Vec<(String, String)>,
    /// What the machine config gives the agent in its environment.
    environment: crate::machine_config::EnvironmentConfig,
    autonomy_permissions: wf_core::autonomy::PermissionMode,
}

impl Run {
    /// How the agent is confined, for the log line that says it started.
    fn isolation(&self) -> &'static str {
        match (&self.sandbox, self.autonomy_permissions) {
            (SandboxConfig::Fence(_), _) => "",
            (SandboxConfig::Off {}, wf_core::autonomy::PermissionMode::Allow) => {
                " without a sandbox, so what it may do is asked"
            }
            (
                SandboxConfig::Off {},
                wf_core::autonomy::PermissionMode::Ask | wf_core::autonomy::PermissionMode::Deny,
            ) => " without a sandbox",
        }
    }
}

/// How the task of a session begins.
enum Begin {
    /// A new conversation, which is told the brief.
    New,
    /// The conversation the agent had in the session before this daemon started.
    Resume(resume::AgentSessionRecord),
}

/// How the task of a session ends.
enum End {
    /// Someone else ended the session, or stopped it.
    Over,
    Interrupt(Interruption),
}

async fn blocking<T: Send + 'static>(
    job: impl FnOnce() -> T + Send + 'static,
) -> Result<T, SessionError> {
    tokio::task::spawn_blocking(job)
        .await
        .map_err(SessionError::Task)
}

impl Daemon {
    pub(crate) async fn snapshot(&self, loaded: &Loaded) -> Result<Arc<State>, StorageClosed> {
        loaded.storage.with(|replica| replica.shared_state()).await
    }

    /// Runs `read` on the state where it lives, so that only its answer is copied out.
    pub(crate) async fn read<T: Send + 'static>(
        &self,
        loaded: &Loaded,
        read: impl FnOnce(&State) -> T + Send + 'static,
    ) -> Result<T, StorageClosed> {
        loaded
            .storage
            .with(move |replica| read(replica.state()))
            .await
    }

    /// The config of this machine, read anew so that edits apply to the next session.
    pub(crate) fn machine_config(&self) -> Result<MachineConfig, MachineConfigError> {
        match &self.launch.machine_config {
            Some(path) => MachineConfig::load(path),
            None => Ok(MachineConfig::default()),
        }
    }

    /// Writes an event as the device. `actor` says on whose behalf.
    pub(crate) async fn author(
        &self,
        loaded: &Loaded,
        actor: Actor,
        body: EventBody,
    ) -> Result<Event, SessionError> {
        if self.write_fails(&body) {
            return Err(SessionError::Io {
                path: PathBuf::from("the log"),
                source: std::io::Error::other("write failed on purpose"),
            });
        }
        let signer = self.signer().clone();
        let now_ms = wf_store::now_ms();
        Ok(loaded
            .storage
            .with(move |replica| replica.author(&signer, actor, now_ms, body))
            .await??)
    }

    pub(crate) fn principal_of_device(
        &self,
        state: &State,
    ) -> Result<wf_core::identity::PrincipalKey, SessionError> {
        state
            .principal_of(&self.device())
            .ok_or(SessionError::NotBound)
    }

    pub(crate) fn harness_command(&self, harness: &HarnessName) -> Option<AcpAgentConfig> {
        self.launch.harnesses.get(harness).cloned().or_else(|| {
            wf_harness::KnownHarness::from_name(harness.as_str()).map(|known| known.command())
        })
    }

    pub(crate) fn workforce_directory(&self, workforce: WorkforceId) -> PathBuf {
        self.launch.data_dir.workforce(&workforce)
    }

    /// The bare repository of a workforce on this machine, where the daemon runs git.
    pub(crate) fn bare_repository(&self, workforce: WorkforceId) -> PathBuf {
        wf_vcs::bare_repository(&self.workforce_directory(workforce))
    }

    /// The bare repository of a workforce, opened the first time it is there and kept. `None`
    /// while it is not made yet.
    pub(crate) async fn open_bare_repository(
        &self,
        workforce: WorkforceId,
        loaded: &Loaded,
    ) -> Result<Option<GitCli>, SessionError> {
        if let Some(git) = loaded.bare.get() {
            return Ok(Some(git.clone()));
        }
        let repository = self.bare_repository(workforce);
        if !repository.join("HEAD").is_file() {
            return Ok(None);
        }
        let git = blocking(move || GitCli::open(&repository)).await??;
        Ok(Some(loaded.bare.get_or_init(|| git).clone()))
    }
}

//! Running sessions: the daemon starts a harness for a work session, prompts it turn after turn
//! until the session ends, and applies what follows from the end to the item.

mod brief;
mod dispatch;
mod drive;
mod end;
mod merge;
mod start;

pub(crate) use dispatch::run_dispatcher;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use wf_core::event::{Event, EventBody};
use wf_core::id::{ItemId, SessionId, WorkforceId};
use wf_core::identity::Actor;
use wf_core::lifecycle::Interruption;
use wf_core::name::{AgentName, HarnessName};
use wf_core::session::SessionRole;
use wf_core::state::State;
use wf_harness::{AcpAgentConfig, Permissions};
use wf_store::AuthorError;
use wf_vcs::VcsError;

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
    #[error("there is no item {0}")]
    UnknownItem(ItemId),
    #[error("{0} is archived")]
    Archived(ItemId),
    #[error("{0} already has a live session; stop it first")]
    Claimed(ItemId),
    #[error("there is no session {0}")]
    UnknownSession(SessionId),
    #[error("the session runs on another machine")]
    NotOurs(SessionId),
    #[error("only a work session can be started this way")]
    NotWork,
    #[error("{0}")]
    Vcs(#[from] VcsError),
    #[error(transparent)]
    Authoring(#[from] AuthorError),
    #[error(transparent)]
    Storage(#[from] StorageClosed),
    #[error(transparent)]
    MachineConfig(#[from] MachineConfigError),
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

/// How long a cancelled turn may take to end before the harness is closed anyway.
const CANCEL_GRACE: Duration = Duration::from_secs(20);

/// The sessions this daemon is running. Lost with the process, which is why a daemon that starts
/// ends every live session of its machine.
#[derive(Default)]
pub(crate) struct Runners {
    running: Mutex<BTreeMap<SessionId, mpsc::UnboundedSender<Stop>>>,
}

impl Runners {
    fn add(&self, session: SessionId, stop: mpsc::UnboundedSender<Stop>) {
        self.lock().insert(session, stop);
    }

    /// Stops the task of `session`, if it runs here.
    fn stop(&self, session: SessionId, how: Stop) {
        if let Some(stop) = self.lock().remove(&session) {
            let _ = stop.send(how);
        }
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

/// The items one kind of operation runs on right now, such as merging, so that it runs on an
/// item once at a time.
#[derive(Default)]
pub(crate) struct ItemLocks {
    items: Mutex<std::collections::BTreeSet<ItemId>>,
}

pub(crate) struct ItemLock<'a> {
    locks: &'a ItemLocks,
    item: ItemId,
}

impl ItemLocks {
    /// The lock of `item`, or `None` while someone else holds it.
    fn take(&self, item: ItemId) -> Option<ItemLock<'_>> {
        let mut items = self
            .items
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        items.insert(item).then_some(ItemLock { locks: self, item })
    }
}

impl Drop for ItemLock<'_> {
    fn drop(&mut self) {
        self.locks
            .items
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.item);
    }
}

/// What a session is, to run it.
struct Run {
    workforce: WorkforceId,
    session: SessionId,
    item: ItemId,
    role: SessionRole,
    agent: AgentName,
    harness: AcpAgentConfig,
    model: Option<String>,
    worktree: PathBuf,
    transcript: PathBuf,
    branch: String,
    target: String,
    repository: PathBuf,
    nudges: u32,
    park_timeout: Duration,
    max_duration: Option<Duration>,
    permissions: Arc<dyn Permissions>,
    sandbox: SandboxConfig,
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
        .map_err(|error| SessionError::Io {
            path: PathBuf::new(),
            source: std::io::Error::other(error),
        })
}

impl Daemon {
    pub(crate) async fn snapshot(&self, loaded: &Loaded) -> Result<State, StorageClosed> {
        loaded.storage.with(|replica| replica.state().clone()).await
    }

    /// The config of this machine, read anew so that edits apply to the next session.
    fn machine_config(&self) -> Result<MachineConfig, MachineConfigError> {
        match &self.config.machine_config {
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
        let signer = self.signer().clone();
        let now_ms = crate::now_ms();
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
        self.config
            .harnesses
            .get(harness)
            .cloned()
            .or_else(|| wf_harness::command_for(harness.as_str()))
    }

    pub(crate) fn workforce_directory(&self, workforce: WorkforceId) -> PathBuf {
        self.config.data_dir.join(workforce.to_string())
    }

    /// Where the repository of a workforce is on this machine, as the last `wf` run there said.
    pub(crate) fn repository_of(&self, workforce: WorkforceId) -> Option<PathBuf> {
        let text = std::fs::read_to_string(
            self.workforce_directory(workforce)
                .join(crate::REPOSITORY_FILE),
        )
        .ok()?;
        Some(PathBuf::from(text.trim())).filter(|path| !path.as_os_str().is_empty())
    }

    fn remember_repository(
        &self,
        workforce: WorkforceId,
        repository: &Path,
    ) -> Result<(), SessionError> {
        let directory = self.workforce_directory(workforce);
        crate::remember_repository(&directory, repository).map_err(|source| SessionError::Io {
            path: directory,
            source,
        })
    }
}

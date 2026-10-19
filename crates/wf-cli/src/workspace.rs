//! Finding the workforce for the current directory and opening it.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail};
use serde::{Deserialize, Serialize};
use wf_core::event::{Event, EventBody};
use wf_core::id::{EventId, ItemId, SessionId, WorkforceId};
use wf_core::identity::{Actor, AgentId, DeviceKey, DeviceSigner, PrincipalKey, PrincipalSigner};
use wf_core::lifecycle::Finish;
use wf_core::session::CommitId;
use wf_core::state::State;
use wf_daemon::MachineFaults;
use wf_daemon::caches::CacheUsage;
use wf_daemon::caller::Caller;
use wf_daemon::client::{ClientError, DaemonClient};
use wf_daemon::data_dir::DataDirError;
use wf_daemon::proto::{EndAnswer, HeldPermission, LiveDiff, ProtoError};
use wf_harness::Permission;
use wf_store::{EventStore, RedbStore, Replica};

use wf_daemon::DataDir;
use wf_daemon::caller::{SESSION_VARIABLE, WORKFORCE_VARIABLE};

pub const WORKFORCE_FILE: &str = ".workforce.toml";

/// The repository only holds the workforce id. It identifies, and is not a capability.
#[derive(Serialize, Deserialize)]
pub struct WorkforceFile {
    pub id: WorkforceId,
}

fn agent_actor(state: &State, session: &str) -> anyhow::Result<Actor> {
    let id: SessionId = session
        .parse()
        .with_context(|| format!("{SESSION_VARIABLE} is not a session id"))?;
    session_actor(state, id)
}

fn session_actor(state: &State, id: SessionId) -> anyhow::Result<Actor> {
    let folded = state
        .session(id)
        .ok_or_else(|| anyhow!("session {id} is not one of this workforce"))?;
    Ok(folded.actor())
}

/// Looks for `.workforce.toml` in `start` and its parents. Inside a session the workforce is the
/// session's, whatever the file says.
pub fn find_workforce(start: &Path) -> anyhow::Result<(PathBuf, WorkforceId)> {
    for directory in start.ancestors() {
        let path = directory.join(WORKFORCE_FILE);
        if path.is_file() {
            let text = std::fs::read_to_string(&path)?;
            let file: WorkforceFile = toml::from_str(&text)
                .with_context(|| format!("{} is not a valid workforce file", path.display()))?;
            let id = match std::env::var(WORKFORCE_VARIABLE) {
                Ok(id) => id
                    .parse()
                    .with_context(|| format!("{WORKFORCE_VARIABLE} is not a workforce id"))?,
                Err(_) => file.id,
            };
            return Ok((path, id));
        }
    }
    bail!("no {WORKFORCE_FILE} found here or in a parent directory; run `wf init` first")
}

pub fn store_path(data: &DataDir, id: &WorkforceId) -> anyhow::Result<PathBuf> {
    let directory = data.workforce(id);
    std::fs::create_dir_all(&directory)?;
    Ok(data.store(id))
}

/// Where a command reads and writes the workforce: a running daemon, which owns the store, or
/// the store itself, opened by this process.
enum Source {
    /// The store, and the device key this process signs with.
    Local(Replica<RedbStore>, Box<DeviceSigner>),
    Daemon {
        client: RefCell<DaemonClient>,
        state: State,
    },
}

pub struct Workspace {
    source: Source,
    pub data: DataDir,
    workforce: WorkforceId,
    device: DeviceKey,
    /// The member this machine belongs to; `actor` is the principal or a session of theirs.
    principal: PrincipalKey,
    actor: Actor,
    /// The directory of the workforce file, if there is one.
    repository: Option<PathBuf>,
}

/// Why [`Workspace::reconnect`] did not connect.
#[derive(Debug)]
pub enum Reconnect {
    /// No daemon listens, as while one restarts.
    NoDaemon,
    /// The daemon speaks another protocol than this build.
    Refused { daemon: u32, client: u32 },
    Failed(anyhow::Error),
}

/// Whether `error` is the connection to the daemon breaking, as when the daemon stops, rather
/// than the daemon refusing a call.
pub fn connection_broke(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<ClientError>(),
        Some(ClientError::Proto(ProtoError::Io(_) | ProtoError::Frame(_)))
    )
}

/// The replica of a command that talks to the network itself, or the daemon that does.
pub enum Replicated {
    Local(Box<Replica<RedbStore>>, Box<DeviceSigner>),
    Daemon(Box<DaemonClient>),
}

impl Workspace {
    /// Uses the daemon when one is running, and opens the store otherwise.
    pub fn open(data: DataDir, start: &Path, refold: bool) -> anyhow::Result<Self> {
        if refold {
            data.require_no_daemon("refolding the log")?;
        }
        // A session may run in an empty directory, without a workforce file to find.
        let (file, id) = match std::env::var(WORKFORCE_VARIABLE) {
            Ok(id) => (
                find_workforce(start).ok().map(|(file, _)| file),
                id.parse()
                    .with_context(|| format!("{WORKFORCE_VARIABLE} is not a workforce id"))?,
            ),
            Err(_) => {
                let (file, id) = find_workforce(start)?;
                (Some(file), id)
            }
        };
        let named_by = file.as_ref().map_or_else(
            || WORKFORCE_VARIABLE.to_string(),
            |file| file.display().to_string(),
        );
        let repository = file
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf);
        // The checkouts of sessions have the workforce file too, but they come and go.
        if let Some(repository) = repository
            .as_deref()
            .filter(|directory| directory.join(".git").is_dir())
        {
            data.remember_repository(&id, repository);
        }
        // With a daemon, only the daemon reads the device key: agents are not let to.
        if !refold && let Some(mut client) = data.connect_daemon()? {
            let device = client.device();
            let state = client.state(id).with_context(|| {
                format!("the daemon cannot serve workforce {id}, named by {named_by}")
            })?;
            return Self::bound(
                Source::Daemon {
                    client: RefCell::new(client),
                    state,
                },
                data,
                id,
                device,
                repository,
            );
        }
        let signer = data
            .device()?
            .ok_or_else(|| anyhow!("no device key; this machine has not joined the workforce"))?;
        let store = RedbStore::open(store_path(&data, &id)?)?;
        let replica = if refold {
            Replica::refold(store, id)?
        } else {
            Replica::open(store, id)?
        };
        if replica.state().workforce_id() != Some(id) {
            bail!(
                "the local log for workforce {id} is missing or does not match; is this machine part of it?"
            );
        }
        let device = signer.key();
        Self::bound(
            Source::Local(replica, Box::new(signer)),
            data,
            id,
            device,
            repository,
        )
    }

    fn bound(
        source: Source,
        data: DataDir,
        workforce: WorkforceId,
        device: DeviceKey,
        repository: Option<PathBuf>,
    ) -> anyhow::Result<Self> {
        let state = match &source {
            Source::Local(replica, _) => replica.state(),
            Source::Daemon { state, .. } => state,
        };
        let principal = state.principal_of(&device).ok_or_else(|| {
            anyhow!("this machine's device key is not bound to a member of the workforce")
        })?;
        let actor = match &source {
            Source::Daemon { client, .. } => match client.borrow().caller() {
                Caller::Principal => Actor::Principal(principal),
                Caller::Session { session, .. } => session_actor(state, session)?,
            },
            Source::Local(..) => match std::env::var(SESSION_VARIABLE) {
                Ok(session) => agent_actor(state, &session)?,
                Err(_) => Actor::Principal(principal),
            },
        };
        Ok(Self {
            source,
            data,
            workforce,
            device,
            principal,
            actor,
            repository,
        })
    }

    /// Hands over the replica, for commands that share it with the network.
    pub fn into_replicated(self) -> (Replicated, WorkforceId) {
        let replicated = match self.source {
            Source::Local(replica, signer) => Replicated::Local(Box::new(replica), signer),
            Source::Daemon { client, .. } => Replicated::Daemon(Box::new(client.into_inner())),
        };
        (replicated, self.workforce)
    }

    /// The session this process works in: the daemon's say, or `WF_SESSION` without a daemon.
    pub fn session(&self) -> Option<SessionId> {
        match &self.actor {
            Actor::Agent { session, .. } => Some(*session),
            Actor::Principal(_) | Actor::Daemon(_) => None,
        }
    }

    pub fn device_key(&self) -> DeviceKey {
        self.device
    }

    /// The member this machine belongs to, checked when the workspace was opened.
    pub fn principal(&self) -> PrincipalKey {
        self.principal
    }

    /// The repository of the workforce: the directory of its `.workforce.toml`.
    pub fn repository(&self) -> anyhow::Result<wf_vcs::GitCli> {
        let directory = self
            .repository
            .as_deref()
            .ok_or_else(|| anyhow!("no {WORKFORCE_FILE} found here or in a parent directory"))?;
        Ok(wf_vcs::GitCli::open(directory)?)
    }

    /// When devices were last heard from. Only a running daemon knows.
    pub fn last_seen(&self) -> anyhow::Result<crate::devices::LastSeen> {
        match &self.source {
            Source::Local(..) => Ok(crate::devices::LastSeen::new()),
            Source::Daemon { client, .. } => Ok(client
                .borrow_mut()
                .last_seen(self.workforce)?
                .into_iter()
                .collect()),
        }
    }

    pub fn state(&self) -> &State {
        match &self.source {
            Source::Local(replica, _) => replica.state(),
            Source::Daemon { state, .. } => state,
        }
    }

    /// Connects to the daemon again, after the connection broke because it restarted, and reads
    /// the state anew.
    pub fn reconnect(&mut self) -> Result<(), Reconnect> {
        let Source::Daemon { client, state } = &mut self.source else {
            return Ok(());
        };
        let mut fresh = match self.data.connect_daemon() {
            Ok(Some(fresh)) => fresh,
            Ok(None) => return Err(Reconnect::NoDaemon),
            Err(DataDirError::Client(ClientError::Proto(ProtoError::VersionMismatch {
                daemon,
                client,
            }))) => return Err(Reconnect::Refused { daemon, client }),
            Err(error) => return Err(Reconnect::Failed(error.into())),
        };
        *state = fresh
            .state(self.workforce)
            .map_err(|error| Reconnect::Failed(error.into()))?;
        *client.get_mut() = fresh;
        Ok(())
    }

    /// Fetches the state again from the daemon, to show what was written or what arrived since.
    pub fn refresh(&mut self) -> anyhow::Result<()> {
        match &mut self.source {
            Source::Daemon { client, state } => {
                *state = client.get_mut().state(self.workforce)?;
            }
            Source::Local(..) => {}
        }
        Ok(())
    }

    pub fn event(&self, id: &EventId) -> anyhow::Result<Option<Event>> {
        match &self.source {
            Source::Local(replica, _) => Ok(replica.store().event(id)?),
            Source::Daemon { client, .. } => Ok(client.borrow_mut().event(self.workforce, *id)?),
        }
    }

    /// The events from position `from` of the log on, in the order this machine applied them.
    pub fn events_since(&self, from: u64) -> anyhow::Result<Vec<Event>> {
        match &self.source {
            Source::Local(replica, _) => Ok(replica.store().log_since(from)?),
            Source::Daemon { client, .. } => {
                Ok(client.borrow_mut().events_since(self.workforce, from)?)
            }
        }
    }

    /// Whether the daemon serves this workspace, so that what others write shows up in it.
    pub fn follows_daemon(&self) -> bool {
        match &self.source {
            Source::Daemon { .. } => true,
            Source::Local(..) => false,
        }
    }

    /// Every event, in the order this machine applied them. That is a causal order, except that
    /// a fork has the history it was made from newest event first.
    pub fn events(&self) -> anyhow::Result<Vec<Event>> {
        match &self.source {
            Source::Local(replica, _) => Ok(replica.store().log_since(0)?),
            Source::Daemon { client, .. } => Ok(client.borrow_mut().events(self.workforce)?),
        }
    }

    /// The events that can show in the activity of `item`, in the order this machine applied them.
    pub fn item_events(&self, item: ItemId) -> anyhow::Result<Vec<Event>> {
        match &self.source {
            Source::Local(replica, _) => Ok(replica
                .store()
                .log_since(0)?
                .into_iter()
                .filter(|event| replica.state().can_be_activity_of(item, event))
                .collect()),
            Source::Daemon { client, .. } => {
                Ok(client.borrow_mut().item_events(self.workforce, item)?)
            }
        }
    }

    /// The events that defined or removed `agent`, in the order this machine applied them.
    pub fn agent_events(&self, agent: &AgentId) -> anyhow::Result<Vec<Event>> {
        match &self.source {
            Source::Local(replica, _) => Ok(replica
                .store()
                .log_since(0)?
                .into_iter()
                .filter(|event| replica.state().agent_change(agent, event).is_some())
                .collect()),
            Source::Daemon { client, .. } => Ok(client
                .borrow_mut()
                .agent_events(self.workforce, agent.clone())?),
        }
    }

    /// Checks the policy rules, signs with this machine's device key and stores the event; the
    /// state then shows it.
    pub fn author(&mut self, body: EventBody) -> anyhow::Result<Event> {
        let event = self.author_without_refresh(body)?;
        self.refresh()?;
        Ok(event)
    }

    /// Like [`Self::author`], but the state does not show the event until [`Self::refresh`]: a
    /// command that writes several events refreshes once, after the last.
    pub fn author_without_refresh(&mut self, body: EventBody) -> anyhow::Result<Event> {
        match &mut self.source {
            Source::Local(replica, signer) => {
                Ok(replica.author(signer, self.actor.clone(), wf_store::now_ms(), body)?)
            }
            Source::Daemon { client, .. } => Ok(client.get_mut().author(self.workforce, body)?),
        }
    }

    pub fn workforce(&self) -> WorkforceId {
        self.workforce
    }

    /// Starts a work session of `agent` on `item`. Sessions run in the daemon.
    pub fn start_session(
        &mut self,
        agent: wf_core::name::AgentName,
        item: ItemId,
    ) -> anyhow::Result<SessionId> {
        let repository = self.repository()?.repository().to_path_buf();
        let workforce = self.workforce;
        let (client, state) = self.daemon("start sessions")?;
        let session = client.start_session(workforce, agent, item, &repository)?;
        *state = client.state(workforce)?;
        Ok(session)
    }

    /// The permission requests that wait for the principal. Only a running daemon holds any.
    pub fn held_permissions(&self) -> anyhow::Result<Vec<HeldPermission>> {
        match &self.source {
            Source::Local(..) => Ok(Vec::new()),
            Source::Daemon { client, .. } => Ok(client.borrow_mut().permissions(self.workforce)?),
        }
    }

    /// What the caches in the worktrees take, as the daemon last checked them against its budget;
    /// nothing without a daemon, or when the machine config sets no budget.
    pub fn cache_usage(&self) -> anyhow::Result<Option<CacheUsage>> {
        match &self.source {
            Source::Local(..) => Ok(None),
            Source::Daemon { client, .. } => Ok(client.borrow_mut().cache_usage(self.workforce)?),
        }
    }

    /// The work of `item` as the device of its live session shows it, which the daemon asks for;
    /// `None` without a daemon. `since` is the head of the answer the caller last had.
    pub fn live_diff(
        &self,
        item: ItemId,
        since: Option<CommitId>,
    ) -> anyhow::Result<Option<LiveDiff>> {
        match &self.source {
            Source::Local(..) => Ok(None),
            Source::Daemon { client, .. } => Ok(Some(client.borrow_mut().live_diff(
                self.workforce,
                item,
                since,
            )?)),
        }
    }

    /// What is wrong with the machine, as the daemon learnt it; nothing without a daemon.
    pub fn machine_faults(&self) -> anyhow::Result<MachineFaults> {
        match &self.source {
            Source::Local(..) => Ok(MachineFaults::default()),
            Source::Daemon { client, .. } => {
                Ok(client.borrow_mut().machine_faults(self.workforce)?)
            }
        }
    }

    pub fn answer_permission(
        &mut self,
        session: SessionId,
        since_ms: u64,
        answer: Permission,
    ) -> anyhow::Result<HeldPermission> {
        let workforce = self.workforce;
        let (client, _) = self.daemon("answer permission requests")?;
        Ok(client.answer_permission(workforce, session, since_ms, answer)?)
    }

    /// Ends the session this process works in, which runs in the daemon.
    pub fn end_session(&mut self, finish: Finish) -> anyhow::Result<EndAnswer> {
        let workforce = self.workforce;
        let (client, state) = self.daemon("end sessions")?;
        let answer = client.end_session(workforce, finish)?;
        *state = client.state(workforce)?;
        Ok(answer)
    }

    /// Stops a session that runs in the daemon now.
    pub fn stop_session(&mut self, session: SessionId) -> anyhow::Result<()> {
        let workforce = self.workforce;
        let (client, state) = self.daemon("stop sessions")?;
        client.stop_session(workforce, session)?;
        *state = client.state(workforce)?;
        Ok(())
    }

    fn daemon(&mut self, what: &str) -> anyhow::Result<(&mut DaemonClient, &mut State)> {
        match &mut self.source {
            Source::Daemon { client, state } => Ok((client.get_mut(), state)),
            Source::Local(..) => {
                bail!("only the daemon can {what}; start it with `wf daemon start`")
            }
        }
    }

    /// Founds the fork that `genesis` starts from this workforce, with its history, in the store
    /// of whoever owns the stores of this machine.
    pub fn create_fork(&mut self, genesis: Event) -> anyhow::Result<()> {
        match &mut self.source {
            Source::Local(replica, _) => {
                let child = genesis.id().into();
                let store = RedbStore::open(store_path(&self.data, &child)?)?;
                Replica::create_fork(store, genesis, &*replica)?;
                Ok(())
            }
            Source::Daemon { client, .. } => {
                client.get_mut().fork(self.workforce, genesis)?;
                Ok(())
            }
        }
    }

    /// The key that signs what only the principal may change.
    pub fn principal_signer(&self) -> anyhow::Result<PrincipalSigner> {
        let signer = self.data.principal(&self.workforce)?.ok_or_else(|| {
            anyhow!("this machine has no principal key for this workforce, so it cannot sign policy changes")
        })?;
        if signer.key() != self.principal {
            bail!("the principal key on this machine belongs to someone else");
        }
        Ok(signer)
    }

    pub fn resolve_item(&self, text: &str) -> anyhow::Result<ItemId> {
        Ok(self.state().resolver().item(text.trim_start_matches('#'))?)
    }
}

//! Finding the workforce for the current directory and opening it.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail};
use serde::{Deserialize, Serialize};
use wf_core::event::{Event, EventBody};
use wf_core::id::{EventId, ItemId, SessionId, WorkforceId};
use wf_core::identity::{Actor, DeviceSigner, PrincipalSigner};
use wf_core::lifecycle::Finish;
use wf_core::state::State;
use wf_daemon::client::DaemonClient;
use wf_daemon::proto::{Answer, HeldPermission};
use wf_store::{EventStore, RedbStore, Replica};

use crate::keys::DataDir;

pub const WORKFORCE_FILE: &str = ".workforce.toml";

/// The repository only holds the workforce id. It identifies, and is not a capability.
#[derive(Serialize, Deserialize)]
pub struct WorkforceFile {
    pub id: WorkforceId,
}

/// Set by the runner for the processes of an agent session: what they write is the session's.
pub const SESSION_VARIABLE: &str = "WF_SESSION";

fn agent_actor(state: &State, session: &str) -> anyhow::Result<Actor> {
    let id: SessionId = session
        .parse()
        .with_context(|| format!("{SESSION_VARIABLE} is not a session id"))?;
    let folded = state
        .session(id)
        .ok_or_else(|| anyhow!("{SESSION_VARIABLE} names no session of this workforce"))?;
    Ok(Actor::Agent {
        agent: folded.agent().clone(),
        session: id,
    })
}

pub fn now_ms() -> u64 {
    u64::try_from(jiff::Timestamp::now().as_millisecond()).unwrap_or_default()
}

/// Set by the runner for the processes of a session: the workforce the session belongs to. The
/// checkout of a session carries the workforce file as it was committed, which names an older
/// workforce when this one was created after the commit.
pub const WORKFORCE_VARIABLE: &str = "WF_WORKFORCE";

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
    Ok(directory.join("store.redb"))
}

/// Where a command reads and writes the workforce: a running daemon, which owns the store, or
/// the store itself, opened by this process.
enum Source {
    Local(Replica<RedbStore>),
    Daemon {
        client: RefCell<DaemonClient>,
        state: State,
    },
}

pub struct Workspace {
    source: Source,
    pub data: DataDir,
    workforce: WorkforceId,
    device: DeviceSigner,
    actor: Actor,
}

/// The replica of a command that talks to the network itself, or the daemon that does.
pub enum Replicated {
    Local(Box<Replica<RedbStore>>),
    Daemon(Box<DaemonClient>),
}

impl Workspace {
    /// Uses the daemon when one is running, and opens the store otherwise.
    pub fn open(data: DataDir, start: &Path, refold: bool) -> anyhow::Result<Self> {
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
        // The checkouts of sessions have the workforce file too, but they come and go.
        if let Some(repository) = file
            .as_deref()
            .and_then(Path::parent)
            .filter(|directory| directory.join(".git").is_dir())
        {
            data.remember_repository(&id, repository);
        }
        let device = data
            .device(false)?
            .ok_or_else(|| anyhow!("no device key; this machine has not joined the workforce"))?;
        if !refold && let Some(mut client) = DaemonClient::connect(&data.daemon_socket())? {
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
            );
        }
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
        Self::bound(Source::Local(replica), data, id, device)
    }

    fn bound(
        source: Source,
        data: DataDir,
        workforce: WorkforceId,
        device: DeviceSigner,
    ) -> anyhow::Result<Self> {
        let state = match &source {
            Source::Local(replica) => replica.state(),
            Source::Daemon { state, .. } => state,
        };
        let principal = state.principal_of(&device.key()).ok_or_else(|| {
            anyhow!("this machine's device key is not bound to a member of the workforce")
        })?;
        let actor = match std::env::var(SESSION_VARIABLE) {
            Ok(session) => agent_actor(state, &session)?,
            Err(_) => Actor::Principal(principal),
        };
        Ok(Self {
            source,
            data,
            workforce,
            device,
            actor,
        })
    }

    /// Hands over the replica, for commands that share it with the network.
    pub fn into_replicated(self) -> (Replicated, DeviceSigner, WorkforceId) {
        let replicated = match self.source {
            Source::Local(replica) => Replicated::Local(Box::new(replica)),
            Source::Daemon { client, .. } => Replicated::Daemon(Box::new(client.into_inner())),
        };
        (replicated, self.device, self.workforce)
    }

    /// The session this process works in, when `WF_SESSION` names one.
    pub fn session(&self) -> Option<SessionId> {
        match &self.actor {
            Actor::Agent { session, .. } => Some(*session),
            Actor::Principal(_) | Actor::Daemon(_) => None,
        }
    }

    pub fn device_key(&self) -> wf_core::identity::DeviceKey {
        self.device.key()
    }

    /// When devices were last heard from. Only a running daemon knows.
    pub fn last_seen(&self) -> crate::devices::LastSeen {
        match &self.source {
            Source::Local(_) => crate::devices::LastSeen::new(),
            Source::Daemon { client, .. } => client
                .borrow_mut()
                .last_seen(self.workforce)
                .map(|seen| seen.into_iter().collect())
                .unwrap_or_default(),
        }
    }

    pub fn state(&self) -> &State {
        match &self.source {
            Source::Local(replica) => replica.state(),
            Source::Daemon { state, .. } => state,
        }
    }

    /// Refreshes the cached state from the daemon while a long-running command follows it.
    pub fn refresh(&mut self) -> anyhow::Result<()> {
        match &mut self.source {
            Source::Daemon { client, state } => {
                *state = client.get_mut().state(self.workforce)?;
            }
            Source::Local(_) => {}
        }
        Ok(())
    }

    pub fn event(&self, id: &EventId) -> anyhow::Result<Option<Event>> {
        match &self.source {
            Source::Local(replica) => Ok(replica.store().event(id)?),
            Source::Daemon { client, .. } => Ok(client.borrow_mut().event(self.workforce, *id)?),
        }
    }

    /// The events from position `from` of the log on, in the order this machine applied them.
    pub fn events_since(&self, from: u64) -> anyhow::Result<Vec<Event>> {
        match &self.source {
            Source::Local(replica) => Ok(replica.store().log_since(from)?),
            Source::Daemon { client, .. } => {
                Ok(client.borrow_mut().events_since(self.workforce, from)?)
            }
        }
    }

    /// Whether the daemon serves this workspace, so that what others write shows up in it.
    pub fn follows_daemon(&self) -> bool {
        match &self.source {
            Source::Daemon { .. } => true,
            Source::Local(_) => false,
        }
    }

    /// Every event, in the order this machine applied them. That is a causal order, except that
    /// a fork has the history it was made from newest event first.
    pub fn events(&self) -> anyhow::Result<Vec<Event>> {
        match &self.source {
            Source::Local(replica) => Ok(replica.store().log_since(0)?),
            Source::Daemon { client, .. } => Ok(client.borrow_mut().events(self.workforce)?),
        }
    }

    /// Checks the policy rules, signs with this machine's device key and stores the event.
    pub fn author(&mut self, body: EventBody) -> anyhow::Result<Event> {
        match &mut self.source {
            Source::Local(replica) => {
                Ok(replica.author(&self.device, self.actor.clone(), now_ms(), body)?)
            }
            Source::Daemon { client, state } => {
                let mut client = client.borrow_mut();
                let event = client.author(self.workforce, self.actor.clone(), body)?;
                *state = client.state(self.workforce)?;
                Ok(event)
            }
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
        repository: &Path,
    ) -> anyhow::Result<SessionId> {
        let workforce = self.workforce;
        let (client, state) = self.daemon("start sessions")?;
        let session = client.start_session(workforce, agent, item, repository)?;
        *state = client.state(workforce)?;
        Ok(session)
    }

    /// The permission requests that wait for the principal. Only a running daemon holds any.
    pub fn held_permissions(&self) -> Vec<HeldPermission> {
        match &self.source {
            Source::Local(_) => Vec::new(),
            Source::Daemon { client, .. } => client
                .borrow_mut()
                .permissions(self.workforce)
                .unwrap_or_default(),
        }
    }

    pub fn answer_permission(
        &mut self,
        session: SessionId,
        answer: Answer,
    ) -> anyhow::Result<HeldPermission> {
        let workforce = self.workforce;
        let (client, _) = self.daemon("answer permission requests")?;
        Ok(client.answer_permission(workforce, session, answer)?)
    }

    /// Ends a session that runs in the daemon.
    pub fn end_session(&mut self, session: SessionId, finish: Finish) -> anyhow::Result<()> {
        let workforce = self.workforce;
        let (client, state) = self.daemon("end sessions")?;
        client.end_session(workforce, session, finish)?;
        *state = client.state(workforce)?;
        Ok(())
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
            Source::Local(_) => {
                bail!("only the daemon can {what}; start it with `wf daemon start`")
            }
        }
    }

    /// Founds the fork that `genesis` starts from this workforce, with its history, in the store
    /// of whoever owns the stores of this machine.
    pub fn create_fork(&mut self, genesis: Event) -> anyhow::Result<()> {
        match &mut self.source {
            Source::Local(replica) => {
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

    pub fn principal_signer(&self) -> anyhow::Result<PrincipalSigner> {
        self.data.principal(&self.workforce)?.ok_or_else(|| {
            anyhow!("this machine has no principal key for this workforce, so it cannot sign policy changes")
        })
    }

    pub fn resolve_item(&self, text: &str) -> anyhow::Result<ItemId> {
        Ok(self.state().resolver().item(text.trim_start_matches('#'))?)
    }
}

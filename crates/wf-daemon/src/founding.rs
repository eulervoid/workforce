//! Founding a new workforce on this machine: what `wf init` hands to the device, done by the
//! daemon when one runs and by the CLI otherwise.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use wf_core::event::{
    AgentDefined, DeviceProfile, Event, EventBody, EventBuilder, EventError, Genesis,
};
use wf_core::id::WorkforceId;
use wf_core::identity::{Actor, DeviceSigner, Signed};
use wf_store::{AuthorError, ReceiveError, RedbStore, Replica, StoreError};

use crate::DataDir;

#[derive(Debug, thiserror::Error)]
pub enum FoundingError {
    #[error("a local log for workforce {0} already exists")]
    Exists(WorkforceId),
    #[error(transparent)]
    Sign(#[from] EventError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Receive(#[from] ReceiveError),
    #[error(transparent)]
    Author(#[from] AuthorError),
    #[error(transparent)]
    Vcs(#[from] wf_vcs::VcsError),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// What the principal founds a workforce with: the genesis they signed, and what this device
/// writes into the workforce right after it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Founding {
    pub genesis: Signed<Genesis>,
    /// The profile of this device.
    pub profile: DeviceProfile,
    /// The agents the founder defines from the start.
    pub agents: Vec<AgentDefined>,
    /// The principal's checkout, which the bare repository starts from and becomes the remote
    /// of; `None` outside a git repository.
    pub checkout: Option<PathBuf>,
}

impl Founding {
    /// Signs the genesis as `device`, which names the workforce.
    pub fn sign(self, device: &DeviceSigner, now_ms: u64) -> Result<SignedFounding, EventError> {
        let actor = Actor::Principal(self.genesis.signer);
        let genesis = EventBuilder::genesis(device.key(), actor, now_ms)
            .sign(device, EventBody::Genesis(self.genesis))?;
        Ok(SignedFounding {
            genesis,
            profile: self.profile,
            agents: self.agents,
            checkout: self.checkout,
        })
    }
}

/// A [`Founding`] whose genesis event this device signed, so the id of the workforce is known
/// before anything is written.
pub struct SignedFounding {
    genesis: Event,
    profile: DeviceProfile,
    agents: Vec<AgentDefined>,
    checkout: Option<PathBuf>,
}

impl SignedFounding {
    pub fn workforce(&self) -> WorkforceId {
        self.genesis.id().into()
    }

    /// Creates the workforce in `data_dir`: its log, with the genesis, the profile of `device`
    /// and the agents, and its bare repository. If it cannot be created nothing is left behind.
    pub fn create(self, data_dir: &DataDir, device: &DeviceSigner) -> Result<(), FoundingError> {
        let id = self.workforce();
        let directory = data_dir.workforce(&id);
        if directory.exists() {
            return Err(FoundingError::Exists(id));
        }
        std::fs::create_dir_all(&directory).map_err(|source| FoundingError::Io {
            path: directory.clone(),
            source,
        })?;
        let created = self.create_in(data_dir, device);
        if created.is_err() {
            let _ = std::fs::remove_dir_all(&directory);
        }
        created
    }

    fn create_in(self, data_dir: &DataDir, device: &DeviceSigner) -> Result<(), FoundingError> {
        let id = self.workforce();
        let actor = self.genesis.content.actor.clone();
        let mut replica = Replica::open(RedbStore::open(data_dir.store(&id))?, id)?;
        replica.receive(self.genesis)?;
        replica.author(
            device,
            actor.clone(),
            wf_store::now_ms(),
            EventBody::DeviceProfile(self.profile),
        )?;
        for agent in self.agents {
            replica.author(
                device,
                actor.clone(),
                wf_store::now_ms(),
                EventBody::AgentDefined(agent),
            )?;
        }
        let target = replica
            .state()
            .policy()
            .expect("a workforce has a policy")
            .policy
            .merge
            .target
            .clone();
        let bare = wf_vcs::bare_repository(&data_dir.workforce(&id));
        wf_vcs::GitCli::create_bare_repository(&bare, &target, self.checkout.as_deref())?;
        Ok(())
    }
}

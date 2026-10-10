//! Finding the workforce for the current directory and opening it.

use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail};
use serde::{Deserialize, Serialize};
use wf_core::event::{Event, EventBody};
use wf_core::id::{ItemId, WorkforceId};
use wf_core::identity::{Actor, DeviceSigner, PrincipalSigner};
use wf_core::state::State;
use wf_store::{RedbStore, Replica};

use crate::keys::DataDir;

pub const WORKFORCE_FILE: &str = ".workforce.toml";

/// The repository only holds the workforce id. It identifies, and is not a capability.
#[derive(Serialize, Deserialize)]
pub struct WorkforceFile {
    pub id: WorkforceId,
}

pub fn now_ms() -> u64 {
    u64::try_from(jiff::Timestamp::now().as_millisecond()).unwrap_or_default()
}

/// Looks for `.workforce.toml` in `start` and its parents.
pub fn find_workforce(start: &Path) -> anyhow::Result<(PathBuf, WorkforceId)> {
    for directory in start.ancestors() {
        let path = directory.join(WORKFORCE_FILE);
        if path.is_file() {
            let text = std::fs::read_to_string(&path)?;
            let file: WorkforceFile = toml::from_str(&text)
                .with_context(|| format!("{} is not a valid workforce file", path.display()))?;
            return Ok((path, file.id));
        }
    }
    bail!("no {WORKFORCE_FILE} found here or in a parent directory; run `wf init` first")
}

pub fn store_path(data: &DataDir, id: &WorkforceId) -> anyhow::Result<PathBuf> {
    let directory = data.workforce(id);
    std::fs::create_dir_all(&directory)?;
    Ok(directory.join("store.redb"))
}

pub struct Workspace {
    pub replica: Replica<RedbStore>,
    pub data: DataDir,
    device: DeviceSigner,
    actor: Actor,
}

impl Workspace {
    pub fn open(data: DataDir, start: &Path, refold: bool) -> anyhow::Result<Self> {
        let (_, id) = find_workforce(start)?;
        let store = RedbStore::open(store_path(&data, &id)?)?;
        let replica = if refold {
            Replica::refold(store)?
        } else {
            Replica::open(store)?
        };
        if replica.state().workforce_id() != Some(id) {
            bail!(
                "the local log for workforce {id} is missing or does not match; is this machine part of it?"
            );
        }
        let device = data
            .device(false)?
            .ok_or_else(|| anyhow!("no device key; this machine has not joined the workforce"))?;
        let principal = replica.state().principal_of(&device.key()).ok_or_else(|| {
            anyhow!("this machine's device key is not bound to a member of the workforce")
        })?;
        Ok(Self {
            replica,
            data,
            device,
            actor: Actor::Human(principal),
        })
    }

    /// Hands over the replica, for commands that share it with the network.
    pub fn into_parts(self) -> (Replica<RedbStore>, DeviceSigner) {
        (self.replica, self.device)
    }

    pub fn device_key(&self) -> wf_core::identity::DeviceKey {
        self.device.key()
    }

    pub fn state(&self) -> &State {
        self.replica.state()
    }

    /// Checks the policy rules, signs with this machine's device key and stores the event.
    pub fn author(&mut self, body: EventBody) -> anyhow::Result<Event> {
        Ok(self
            .replica
            .author(&self.device, self.actor.clone(), now_ms(), body)?)
    }

    pub fn principal_signer(&self) -> anyhow::Result<PrincipalSigner> {
        self.data.principal(false)?.ok_or_else(|| {
            anyhow!("this machine has no principal key, so it cannot sign policy changes")
        })
    }

    pub fn resolve_item(&self, text: &str) -> anyhow::Result<ItemId> {
        Ok(self.state().resolver().item(text.trim_start_matches('#'))?)
    }
}

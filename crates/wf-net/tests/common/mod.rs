//! What the tests of wf-net share.

use std::sync::{Arc, Mutex, MutexGuard};

use wf_core::event::Event;
use wf_core::id::EventId;
use wf_core::identity::DeviceKey;
use wf_core::invite::JoinRefusal;
use wf_core::sync::Versions;
use wf_net::{LogError, ReceiveSummary, Signature, SyncLog, replica};
use wf_store::{EventStore, Replica};

/// A replica the tests reach into directly while the sync protocol runs against it.
pub struct SharedReplica<S: EventStore>(Arc<Mutex<Replica<S>>>);

impl<S: EventStore> Clone for SharedReplica<S> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<S: EventStore> SharedReplica<S> {
    pub fn new(replica: Replica<S>) -> Self {
        Self(Arc::new(Mutex::new(replica)))
    }

    pub fn lock(&self) -> MutexGuard<'_, Replica<S>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl<S: EventStore + Send> SyncLog for SharedReplica<S> {
    async fn versions(&self) -> Result<Versions, LogError> {
        Ok(replica::versions(&self.lock()))
    }

    async fn events_after(&self, theirs: &Versions) -> Result<Vec<Event>, LogError> {
        Ok(replica::events_after(&self.lock(), theirs)?)
    }

    async fn events_by_id(&self, ids: &[EventId]) -> Result<Vec<Event>, LogError> {
        Ok(replica::events_by_id(&self.lock(), ids)?)
    }

    async fn receive(&self, events: Vec<Event>) -> Result<ReceiveSummary, LogError> {
        Ok(replica::receive(&mut self.lock(), events)?)
    }

    async fn is_bound(&self, device: DeviceKey) -> Result<bool, LogError> {
        Ok(replica::is_bound(&self.lock(), device))
    }

    async fn check_join(
        &self,
        invite: EventId,
        device: DeviceKey,
        proof: Signature,
        now_ms: u64,
    ) -> Result<Result<(), JoinRefusal>, LogError> {
        Ok(replica::check_join(
            &self.lock(),
            invite,
            device,
            proof,
            now_ms,
        ))
    }

    async fn missing(&self) -> Result<Vec<EventId>, LogError> {
        Ok(replica::missing(&mut self.lock())?)
    }
}

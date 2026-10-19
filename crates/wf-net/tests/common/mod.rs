//! What the tests of wf-net share.

use std::sync::{Arc, Mutex, MutexGuard};

use wf_core::event::Event;
use wf_core::id::EventId;
use wf_core::identity::DeviceKey;
use wf_core::identity::Signature;
use wf_core::invite::JoinRefusal;
use wf_core::sync::{BatchReceived, Versions};
use wf_net::{LogError, SyncLog};
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
        Ok(self.lock().versions())
    }

    async fn events_after(&self, theirs: &Versions) -> Result<Vec<Event>, LogError> {
        self.lock().events_after(theirs).map_err(LogError::new)
    }

    async fn events_by_id(&self, ids: &[EventId]) -> Result<Vec<Event>, LogError> {
        self.lock().events_by_id(ids).map_err(LogError::new)
    }

    async fn receive(&self, events: Vec<Event>) -> Result<BatchReceived, LogError> {
        self.lock().receive_batch(events).map_err(LogError::new)
    }

    async fn is_bound(&self, device: DeviceKey) -> Result<bool, LogError> {
        Ok(self.lock().state().principal_of(&device).is_some())
    }

    async fn check_join(
        &self,
        invite: EventId,
        device: DeviceKey,
        proof: Signature,
        now_ms: u64,
    ) -> Result<Result<(), JoinRefusal>, LogError> {
        Ok(self
            .lock()
            .state()
            .check_join(&invite, device, &proof, now_ms))
    }

    async fn fill_history(&self) -> Result<(), LogError> {
        self.lock().fill_history().map(drop).map_err(LogError::new)
    }

    async fn missing_parents(&self) -> Result<Vec<EventId>, LogError> {
        Ok(self.lock().missing_parents())
    }
}

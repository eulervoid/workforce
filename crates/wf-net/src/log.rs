//! What the sync protocol needs from a replica, so the same protocol runs against a replica
//! opened in this process or one owned by the daemon's storage task.

use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};

use iroh::Signature;
use wf_core::event::Event;
use wf_core::id::EventId;
use wf_core::identity::DeviceKey;
use wf_core::invite::JoinRefusal;
use wf_core::sync::Versions;
use wf_store::{EventStore, Replica, StoreError};

use crate::replica;

#[derive(Debug, thiserror::Error)]
#[error("the local log failed: {0}")]
pub struct LogError(#[source] pub Box<dyn std::error::Error + Send + Sync>);

impl From<StoreError> for LogError {
    fn from(error: StoreError) -> Self {
        Self(Box::new(error))
    }
}

/// What happened to a batch of received events.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReceiveSummary {
    pub applied: usize,
    /// Events that are invalid. A peer may send them; they are dropped.
    pub rejected: usize,
}

pub trait SyncLog: Send + Sync {
    fn versions(&self) -> impl Future<Output = Result<Versions, LogError>> + Send;

    /// Everything the peer lacks given its versions, in an order every event follows its parents.
    fn events_after(
        &self,
        theirs: &Versions,
    ) -> impl Future<Output = Result<Vec<Event>, LogError>> + Send;

    /// The applied events among `ids`.
    fn events_by_id(
        &self,
        ids: &[EventId],
    ) -> impl Future<Output = Result<Vec<Event>, LogError>> + Send;

    fn receive(
        &self,
        events: Vec<Event>,
    ) -> impl Future<Output = Result<ReceiveSummary, LogError>> + Send;

    /// Whether `device` is bound to a member of this workforce.
    fn is_bound(&self, device: DeviceKey) -> impl Future<Output = Result<bool, LogError>> + Send;

    /// The handshake rules for letting a device join with an invite.
    fn check_join(
        &self,
        invite: EventId,
        device: DeviceKey,
        proof: Signature,
        now_ms: u64,
    ) -> impl Future<Output = Result<Result<(), JoinRefusal>, LogError>> + Send;

    /// Parents of parked events that are not known yet.
    fn missing(&self) -> impl Future<Output = Result<Vec<EventId>, LogError>> + Send;
}

/// A replica shared between the sync protocol and whoever else needs it in this process.
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
        Ok(replica::missing(&self.lock()))
    }
}

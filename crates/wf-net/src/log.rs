//! What the sync protocol needs from a replica, so the same protocol runs against a replica
//! opened in this process or one owned by the daemon's storage task.

use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};

use wf_core::event::Event;
use wf_core::id::EventId;
use wf_core::identity::DeviceKey;
use wf_core::sync::{self, Versions};
use wf_store::{EventStore, ReceiveError, Replica, StoreError};

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
        Ok(self.lock().state().versions())
    }

    async fn events_after(&self, theirs: &Versions) -> Result<Vec<Event>, LogError> {
        let replica = self.lock();
        let mut events = Vec::new();
        for outgoing in sync::ahead_of(&replica.state().versions(), theirs) {
            events.extend(
                replica
                    .store()
                    .feed_since(&outgoing.author, outgoing.after_seq)?,
            );
            for id in &outgoing.tips {
                events.extend(replica.store().event(id)?);
            }
        }
        sync::causal_order(&mut events);
        events.dedup_by_key(|event| event.id());
        Ok(events)
    }

    async fn events_by_id(&self, ids: &[EventId]) -> Result<Vec<Event>, LogError> {
        let replica = self.lock();
        let mut events = Vec::new();
        for id in ids {
            if let Some(event) = replica.store().event(id)? {
                events.push(event);
            }
        }
        Ok(events)
    }

    async fn receive(&self, events: Vec<Event>) -> Result<ReceiveSummary, LogError> {
        let mut replica = self.lock();
        let mut summary = ReceiveSummary::default();
        for event in events {
            match replica.receive(event) {
                Ok(received) => summary.applied += received.applied.len(),
                Err(ReceiveError::Invalid(_)) => summary.rejected += 1,
                Err(ReceiveError::Store(error)) => return Err(error.into()),
            }
        }
        Ok(summary)
    }

    async fn is_bound(&self, device: DeviceKey) -> Result<bool, LogError> {
        Ok(self.lock().state().principal_of(&device).is_some())
    }

    async fn missing(&self) -> Result<Vec<EventId>, LogError> {
        Ok(self.lock().state().missing_parents().into_iter().collect())
    }
}

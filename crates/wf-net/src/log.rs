//! What the sync protocol needs from a replica, so the same protocol runs against a replica
//! opened in this process or one owned by the daemon's storage task.

use std::future::Future;

use wf_core::event::Event;
use wf_core::id::EventId;
use wf_core::identity::{DeviceKey, Signature};
use wf_core::invite::JoinRefusal;
use wf_core::sync::{BatchReceived, Versions};

#[derive(Debug, thiserror::Error)]
#[error("the local log failed")]
pub struct LogError(#[source] pub Box<dyn std::error::Error + Send + Sync>);

impl LogError {
    pub fn new(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self(Box::new(error))
    }
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
    ) -> impl Future<Output = Result<BatchReceived, LogError>> + Send;

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
    fn missing_parents(&self) -> impl Future<Output = Result<Vec<EventId>, LogError>> + Send;
}

//! The sync protocol's questions, answered from a replica. Whoever owns a replica (a mutex in
//! one process, the daemon's storage task) implements [`crate::SyncLog`] by calling these.

use iroh::Signature;
use wf_core::event::Event;
use wf_core::id::EventId;
use wf_core::identity::DeviceKey;
use wf_core::invite::JoinRefusal;
use wf_core::sync::{self, Versions};
use wf_store::{EventStore, ReceiveError, Replica, StoreError};

use crate::log::ReceiveSummary;

pub fn versions<S: EventStore>(replica: &Replica<S>) -> Versions {
    replica.state().versions()
}

/// Everything the peer lacks given its versions, in an order every event follows its parents.
pub fn events_after<S: EventStore>(
    replica: &Replica<S>,
    theirs: &Versions,
) -> Result<Vec<Event>, StoreError> {
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

pub fn events_by_id<S: EventStore>(
    replica: &Replica<S>,
    ids: &[EventId],
) -> Result<Vec<Event>, StoreError> {
    let mut events = Vec::new();
    for id in ids {
        if let Some(event) = replica.store().event(id)? {
            events.push(event);
        }
    }
    Ok(events)
}

/// Invalid events are counted and dropped; only a failing store is an error.
pub fn receive<S: EventStore>(
    replica: &mut Replica<S>,
    events: Vec<Event>,
) -> Result<ReceiveSummary, StoreError> {
    let mut summary = ReceiveSummary::default();
    for event in events {
        match replica.receive(event) {
            Ok(received) => summary.applied += received.applied.len(),
            Err(ReceiveError::Invalid(_)) => summary.rejected += 1,
            Err(ReceiveError::Store(error)) => return Err(error),
        }
    }
    Ok(summary)
}

pub fn is_bound<S: EventStore>(replica: &Replica<S>, device: DeviceKey) -> bool {
    replica.state().principal_of(&device).is_some()
}

pub fn check_join<S: EventStore>(
    replica: &Replica<S>,
    invite: EventId,
    device: DeviceKey,
    proof: Signature,
    now_ms: u64,
) -> Result<(), JoinRefusal> {
    replica.state().check_join(&invite, device, &proof, now_ms)
}

pub fn missing<S: EventStore>(replica: &Replica<S>) -> Vec<EventId> {
    replica.state().missing_parents().into_iter().collect()
}

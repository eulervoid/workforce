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

/// How many events of a fork's history one request for it may bring along.
const MAX_HISTORY_PER_REQUEST: usize = 4096;

/// Whether the event was written in the workforce of the replica, as opposed to being part of
/// the history it was forked from.
fn is_own<S: EventStore>(replica: &Replica<S>, event: &Event) -> bool {
    match replica.state().workforce_id() {
        Some(workforce) => {
            event.content.workforce == Some(workforce)
                || wf_core::id::WorkforceId::from(event.id()) == workforce
        }
        None => false,
    }
}

/// Everything the peer lacks given its versions, in an order every event follows its parents.
/// The history a fork was made from is not part of it: the peer asks for that by id.
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
    events.retain(|event| is_own(replica, event));
    sync::causal_order(&mut events);
    events.dedup_by_key(|event| event.id());
    Ok(events)
}

/// The events asked for that this replica has. Events of the workforce come parents first. A
/// request for history of the fork it was made from brings the history behind it along, newest
/// first, because the receiver takes in an event only once something it already has names it.
pub fn events_by_id<S: EventStore>(
    replica: &Replica<S>,
    ids: &[EventId],
) -> Result<Vec<Event>, StoreError> {
    let mut own = Vec::new();
    let mut history = Vec::new();
    for id in ids {
        if let Some(event) = replica.store().event(id)? {
            if is_own(replica, &event) {
                own.push(event);
            } else {
                history.push(event);
            }
        }
    }
    let mut seen: std::collections::BTreeSet<EventId> = history.iter().map(Event::id).collect();
    let mut frontier: Vec<EventId> = history
        .iter()
        .flat_map(|event| event.causal_parents())
        .collect();
    while let Some(id) = frontier.pop() {
        if history.len() >= MAX_HISTORY_PER_REQUEST {
            break;
        }
        if !seen.insert(id) {
            continue;
        }
        if let Some(event) = replica.store().event(&id)? {
            frontier.extend(event.causal_parents());
            history.push(event);
        }
    }
    sync::causal_order(&mut own);
    history.sort_by_key(|event| std::cmp::Reverse((event.content.hlc, event.id())));
    own.extend(history);
    Ok(own)
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

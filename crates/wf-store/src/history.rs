//! Where a fork finds the history it was made from: on this machine, before asking a peer.

use std::collections::BTreeSet;

use wf_core::event::Event;
use wf_core::id::{EventId, WorkforceId};

use crate::replica::Replica;
use crate::store::{EventStore, StoreError};

/// Something that may hold events of the workforce a fork was made from.
pub trait HistorySource: Send {
    /// The events of workforce `parent` that `wanted` names or that come before them, as far as
    /// this source has them, newest first. A source that fails or does not hold `parent` answers
    /// with fewer events, or none: the fork then asks its peers for the rest.
    fn history_behind(&self, parent: WorkforceId, wanted: &BTreeSet<EventId>) -> Vec<Event>;
}

/// The events `store` holds that `wanted` names or that come before them, newest first. Every
/// event follows the ones that name it, which is the order a fork takes history in.
pub fn history_behind<S: EventStore>(
    store: &S,
    wanted: &BTreeSet<EventId>,
) -> Result<Vec<Event>, StoreError> {
    let mut seen = BTreeSet::new();
    let mut frontier: Vec<EventId> = wanted.iter().copied().collect();
    let mut events = Vec::new();
    while let Some(id) = frontier.pop() {
        if !seen.insert(id) {
            continue;
        }
        if let Some(event) = store.event(&id)? {
            frontier.extend(event.causal_parents());
            events.push(event);
        }
    }
    events.sort_by_key(|event| std::cmp::Reverse((event.content.hlc, event.id())));
    Ok(events)
}

impl<S: EventStore + Send> HistorySource for Replica<S> {
    fn history_behind(&self, parent: WorkforceId, wanted: &BTreeSet<EventId>) -> Vec<Event> {
        if self.state().workforce_id() != Some(parent) {
            return Vec::new();
        }
        history_behind(self.store(), wanted).unwrap_or_default()
    }
}

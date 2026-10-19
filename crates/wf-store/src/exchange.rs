//! What peers ask of a replica when they sync with it: the questions of the sync protocol,
//! answered from the log and the folded state.

use std::collections::BTreeSet;

use wf_core::event::Event;
use wf_core::id::{EventId, WorkforceId};
use wf_core::sync::{self, Versions};

use crate::history::history_behind;
use crate::replica::Replica;
use crate::store::{EventStore, StoreError};

/// How many events of a fork's history one request for it may bring along.
const MAX_HISTORY_PER_REQUEST: usize = 4096;

impl<S: EventStore> Replica<S> {
    pub fn versions(&self) -> Versions {
        self.state().versions()
    }

    /// Everything the peer lacks given its versions, in an order every event follows its
    /// parents. The history a fork was made from is not part of it: the peer asks for that by id.
    pub fn events_after(&self, theirs: &Versions) -> Result<Vec<Event>, StoreError> {
        let mut events = Vec::new();
        for outgoing in sync::ahead_of(&self.state().versions(), theirs) {
            events.extend(
                self.store()
                    .feed_since(&outgoing.author, outgoing.after_seq)?,
            );
            for id in &outgoing.tips {
                events.extend(self.store().event(id)?);
            }
        }
        events.retain(|event| self.is_own(event));
        sync::causal_order(&mut events);
        events.dedup_by_key(|event| event.id());
        Ok(events)
    }

    /// The events asked for that this replica has. Events of the workforce come parents first. A
    /// request for history of the fork it was made from brings the history behind it along,
    /// newest first, because the receiver takes in an event only once something it already has
    /// names it.
    pub fn events_by_id(&self, ids: &[EventId]) -> Result<Vec<Event>, StoreError> {
        let mut own = Vec::new();
        let mut asked_history = BTreeSet::new();
        for id in ids {
            if let Some(event) = self.store().event(id)? {
                if self.is_own(&event) {
                    own.push(event);
                } else {
                    asked_history.insert(*id);
                }
            }
        }
        sync::causal_order(&mut own);
        own.extend(history_behind(
            self.store(),
            &asked_history,
            MAX_HISTORY_PER_REQUEST,
        )?);
        Ok(own)
    }

    /// Parents of parked events that are not known yet.
    pub fn missing_parents(&self) -> Vec<EventId> {
        self.state().missing_parents().into_iter().collect()
    }

    /// Whether the event was written in the workforce of the replica, as opposed to being part
    /// of the history it was forked from.
    fn is_own(&self, event: &Event) -> bool {
        match self.state().workforce_id() {
            Some(workforce) => {
                event.content.workforce == Some(workforce)
                    || WorkforceId::from(event.id()) == workforce
            }
            None => false,
        }
    }
}

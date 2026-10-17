use std::collections::BTreeSet;

use wf_core::event::{Event, EventBody, EventError};
use wf_core::hlc::HLC_TOLERANCE_MS;
use wf_core::id::{EventId, WorkforceId};
use wf_core::identity::{Actor, DeviceSigner};
use wf_core::refs::PARSER_VERSION;
use wf_core::state::{BuildError, Disposition, Received, State, Trust};
use wf_core::validate::{Invalid, PolicyViolation};

use crate::history::HistorySource;
use crate::store::{Commit, EventStore, PendingChange, SnapshotChange, StoreError};

/// A snapshot is written once this many events have been applied since the last one.
pub const SNAPSHOT_INTERVAL: usize = 256;

/// Bumped whenever the serialized shape of [`State`] changes. Snapshots are a cache: a mismatch
/// discards the snapshot and refolds from the log.
const SNAPSHOT_FORMAT: u32 = 9;

#[derive(Debug, thiserror::Error)]
pub enum ReceiveError {
    #[error(transparent)]
    Invalid(#[from] Invalid),
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[derive(Debug, thiserror::Error)]
pub enum AuthorError {
    #[error(transparent)]
    Build(#[from] BuildError),
    #[error(transparent)]
    Policy(#[from] PolicyViolation),
    #[error("the policy names an hlc too far ahead of this device's clock")]
    PolicyHlcInFuture,
    #[error(
        "another event was written under this device's key; join the workforce again as a new device"
    )]
    OwnFeedEquivocated,
    #[error(transparent)]
    Sign(#[from] EventError),
    #[error(transparent)]
    Receive(#[from] ReceiveError),
}

#[derive(Debug, thiserror::Error)]
pub enum ForkError {
    #[error(transparent)]
    Receive(#[from] ReceiveError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("the history this fork was made from is not complete here")]
    HistoryIncomplete,
}

/// What a batch of received events did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatchReceived {
    pub applied: usize,
    pub rejected: usize,
}

/// A local replica: the durable log plus the state folded from it.
///
/// After a [`StoreError`] the in-memory state may be ahead of the log; drop the replica and
/// reopen it.
pub struct Replica<S: EventStore> {
    store: S,
    state: State,
    snapshot_event_count: usize,
    history: Option<Box<dyn HistorySource>>,
    /// The first write that failed, with its causes.
    failed_write: Option<String>,
}

impl<S: EventStore> Replica<S> {
    /// Loads the latest usable snapshot and replays the events written after it. The replica is
    /// for workforce `expected` and refuses the genesis of any other.
    pub fn open(store: S, expected: WorkforceId) -> Result<Self, StoreError> {
        let loaded = load_snapshot(&store)?;
        Self::fold(store, loaded, expected)
    }

    /// Ignores any snapshot and folds the whole log, then writes a fresh snapshot.
    pub fn refold(store: S, expected: WorkforceId) -> Result<Self, StoreError> {
        Self::fold(store, None, expected)
    }

    fn fold(store: S, snapshot: Option<State>, expected: WorkforceId) -> Result<Self, StoreError> {
        let snapshot = snapshot.filter(|state| state.workforce_id().is_some());
        let from_snapshot = snapshot.is_some();
        let mut state = snapshot.unwrap_or_else(|| State::expecting(expected));
        if let Some(found) = state.workforce_id().filter(|found| *found != expected) {
            return Err(StoreError::WrongWorkforce { expected, found });
        }
        let snapshot_event_count = state.event_count();

        let unseen = store.log_since(snapshot_event_count as u64)?;
        let replayed = unseen.len();
        for event in unseen {
            state
                .receive(event, Trust::Stored)
                .map_err(|error| match error {
                    Invalid::WrongWorkforce(found) => {
                        StoreError::WrongWorkforce { expected, found }
                    }
                    other => StoreError::Inconsistent(wf_core::error_chain(&other)),
                })?;
        }
        if state.pending_events().next().is_some() {
            return Err(StoreError::Inconsistent(
                "stored events are missing their parents".into(),
            ));
        }

        let mut replica = Self {
            store,
            state,
            snapshot_event_count,
            history: None,
            failed_write: None,
        };
        for event in replica.store.pending()? {
            // Admission may refuse what an earlier version parked unchecked; such an event is
            // dropped from the pending set rather than failing the open.
            match replica.state.receive(event.clone(), Trust::Stored) {
                Ok(received) => replica.persist(&event, &received)?,
                Err(_) => replica.drop_pending(&event)?,
            }
        }
        let needs_snapshot = replayed >= SNAPSHOT_INTERVAL || (!from_snapshot && replayed > 0);
        if needs_snapshot {
            replica.write_snapshot()?;
        }
        Ok(replica)
    }

    /// The write that failed after the state had taken the event in, so the state may be ahead
    /// of the log. Drop the replica and open it again.
    pub fn failed_write(&self) -> Option<&str> {
        self.failed_write.as_deref()
    }

    fn note_failed_write<T>(&mut self, result: &Result<T, StoreError>) {
        if let Err(error) = result {
            self.failed_write
                .get_or_insert_with(|| wf_core::error_chain(error));
        }
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    /// Validates an event and persists whatever it changed: itself, events it released from the
    /// pending set, or its own place in the pending set.
    pub fn receive(&mut self, event: Event) -> Result<Received, ReceiveError> {
        let received = self
            .state
            .receive_at(event.clone(), Trust::Untrusted, now_ms())?;
        self.persist(&event, &received)?;
        Ok(received)
    }

    /// Receives many events and persists the outcome once, which is much cheaper than one commit
    /// (and snapshot) per event when a peer sends a long log. Events that are invalid are counted
    /// and dropped.
    pub fn receive_batch(&mut self, events: Vec<Event>) -> Result<BatchReceived, StoreError> {
        self.receive_events(events, Trust::Untrusted)
    }

    fn receive_events(
        &mut self,
        events: Vec<Event>,
        trust: Trust,
    ) -> Result<BatchReceived, StoreError> {
        let result = self.receive_events_unchecked(events, trust);
        self.note_failed_write(&result);
        result
    }

    fn receive_events_unchecked(
        &mut self,
        events: Vec<Event>,
        trust: Trust,
    ) -> Result<BatchReceived, StoreError> {
        let mut applied: Vec<Event> = Vec::new();
        let mut parked: Vec<Event> = Vec::new();
        let mut released: Vec<EventId> = Vec::new();
        let mut rejected = 0;
        for event in events {
            let result = match trust {
                Trust::Untrusted => self.state.receive_at(event.clone(), trust, now_ms()),
                Trust::Stored => self.state.receive(event.clone(), trust),
            };
            match result {
                Ok(received) => {
                    released.extend(received.applied.iter().map(Event::id));
                    released.extend(received.rejected.iter().map(|(id, _)| *id));
                    rejected += received.rejected.len();
                    if received.disposition == Disposition::Pending {
                        parked.push(event);
                    }
                    applied.extend(received.applied);
                }
                Err(_) => rejected += 1,
            }
        }
        let released: BTreeSet<EventId> = released.into_iter().collect();
        parked.retain(|event| !released.contains(&event.id()));
        let released: Vec<EventId> = released.into_iter().collect();
        let applied_count = applied.len();
        if applied.is_empty() && parked.is_empty() && released.is_empty() {
            return Ok(BatchReceived {
                applied: 0,
                rejected,
            });
        }
        let due = self.snapshot_due();
        let snapshot = due.then(|| self.snapshot()).transpose()?;
        self.store.commit(Commit {
            append: &applied,
            pending: PendingChange::Update {
                put: &parked,
                remove: &released,
            },
            snapshot: snapshot
                .as_deref()
                .map_or(SnapshotChange::Keep, SnapshotChange::Replace),
        })?;
        if snapshot.is_some() {
            self.snapshot_event_count = self.state.event_count();
        }
        Ok(BatchReceived {
            applied: applied_count,
            rejected,
        })
    }

    /// Where this replica looks for the history of the workforce it was forked from, before it
    /// asks a peer.
    pub fn set_history_source(&mut self, source: Box<dyn HistorySource>) {
        self.history = Some(source);
    }

    /// Takes in what `source` has of the history this fork is still waiting for. The events come
    /// from a store that checked them, so their signatures are not checked again.
    pub fn take_in_history(&mut self, source: &dyn HistorySource) -> Result<usize, StoreError> {
        let Some((parent, _)) = self.state.fork_point() else {
            return Ok(0);
        };
        let wanted = self.state.wanted_history();
        if wanted.is_empty() {
            return Ok(0);
        }
        let events = source.history_behind(parent, &wanted);
        Ok(self.receive_events(events, Trust::Stored)?.applied)
    }

    /// [`Replica::take_in_history`] from the source set with [`Replica::set_history_source`].
    pub fn fill_history(&mut self) -> Result<usize, StoreError> {
        let Some(source) = self.history.take() else {
            return Ok(0);
        };
        let result = self.take_in_history(source.as_ref());
        self.history = Some(source);
        result
    }

    /// Founds a fork in `store` from its genesis and the history `parent` holds. The fork is
    /// complete when this returns: nothing needs to be fetched from a peer.
    pub fn create_fork(
        store: S,
        genesis: Event,
        parent: &dyn HistorySource,
    ) -> Result<Self, ForkError> {
        let mut replica = Self::open(store, WorkforceId::from(genesis.id()))?;
        replica.receive(genesis)?;
        replica.take_in_history(parent)?;
        if replica.state.history_incomplete() {
            return Err(ForkError::HistoryIncomplete);
        }
        Ok(replica)
    }

    /// Checks the policy rules, signs the next event of `signer` and persists it. Refuses when
    /// another event exists under the same key: this device was cloned or restored from a
    /// snapshot, and writing on would fork its feed further.
    pub fn author(
        &mut self,
        signer: &DeviceSigner,
        actor: Actor,
        now_ms: u64,
        body: EventBody,
    ) -> Result<Event, AuthorError> {
        let forked = self
            .state
            .feeds()
            .get(&signer.key())
            .is_some_and(|feed| feed.equivocated().is_some());
        if forked {
            return Err(AuthorError::OwnFeedEquivocated);
        }
        self.state.check_actor(signer.key(), &actor, &body)?;
        self.state.check_policy(&body)?;
        let mut builder = self.state.builder(signer.key(), actor, now_ms)?;
        if let EventBody::DeviceBound(bound) = &body {
            builder = builder.depending_on(bound.invite);
        }
        // The signer chose the hlc of a policy before the event existed.
        if let EventBody::BoundPolicySet(signed) = &body {
            let hlc = signed.statement.hlc;
            if hlc.wall_ms() > now_ms.saturating_add(HLC_TOLERANCE_MS) {
                return Err(AuthorError::PolicyHlcInFuture);
            }
            builder = builder.with_hlc(hlc);
        }
        let event = builder.sign(signer, body)?;
        // The builder's floor may lie beyond this device's clock (stored events, peers admitted
        // under an earlier clock), so the tolerance of `receive_at` must not judge own events.
        let received = self
            .state
            .receive(event.clone(), Trust::Untrusted)
            .map_err(ReceiveError::from)?;
        self.persist(&event, &received)
            .map_err(ReceiveError::from)?;
        Ok(event)
    }

    fn drop_pending(&mut self, event: &Event) -> Result<(), StoreError> {
        let result = self.store.commit(Commit {
            append: &[],
            pending: PendingChange::Update {
                put: &[],
                remove: &[event.id()],
            },
            snapshot: SnapshotChange::Keep,
        });
        self.note_failed_write(&result);
        result
    }

    fn persist(&mut self, submitted: &Event, received: &Received) -> Result<(), StoreError> {
        let result = self.persist_unchecked(submitted, received);
        self.note_failed_write(&result);
        result
    }

    fn persist_unchecked(
        &mut self,
        submitted: &Event,
        received: &Received,
    ) -> Result<(), StoreError> {
        let parked = received.disposition == Disposition::Pending;
        if received.applied.is_empty() && received.rejected.is_empty() && !parked {
            return Ok(());
        }
        let released: Vec<EventId> = received
            .applied
            .iter()
            .map(Event::id)
            .chain(received.rejected.iter().map(|(id, _)| *id))
            .collect();
        let parked_events = [submitted.clone()];
        let put: &[Event] = if parked { &parked_events } else { &[] };

        let due = self.snapshot_due();
        let snapshot = due.then(|| self.snapshot()).transpose()?;
        self.store.commit(Commit {
            append: &received.applied,
            pending: PendingChange::Update {
                put,
                remove: &released,
            },
            snapshot: snapshot
                .as_deref()
                .map_or(SnapshotChange::Keep, SnapshotChange::Replace),
        })?;
        if snapshot.is_some() {
            self.snapshot_event_count = self.state.event_count();
        }
        Ok(())
    }

    /// A snapshot costs as much as the state is large, so the larger it is the longer the wait:
    /// writing one per few hundred events would make a long log quadratic to take in.
    fn snapshot_due(&self) -> bool {
        let interval = SNAPSHOT_INTERVAL.max(self.snapshot_event_count / 8);
        self.state.event_count() >= self.snapshot_event_count + interval
    }

    fn snapshot(&self) -> Result<Vec<u8>, StoreError> {
        Ok(wf_core::encoding::encode(&(
            SNAPSHOT_FORMAT,
            PARSER_VERSION,
            &self.state,
        ))?)
    }

    fn write_snapshot(&mut self) -> Result<(), StoreError> {
        let snapshot = self.snapshot()?;
        self.store.commit(Commit {
            snapshot: SnapshotChange::Replace(&snapshot),
            ..Commit::default()
        })?;
        self.snapshot_event_count = self.state.event_count();
        Ok(())
    }
}

/// `None` when there is no snapshot, or it is stale, unreadable or ahead of the log.
fn load_snapshot<S: EventStore>(store: &S) -> Result<Option<State>, StoreError> {
    let Some(stored) = store.snapshot()? else {
        return Ok(None);
    };
    let Ok(((format, parser), rest)) = postcard::take_from_bytes::<(u32, u16)>(&stored) else {
        return Ok(None);
    };
    if format != SNAPSHOT_FORMAT || parser != PARSER_VERSION {
        return Ok(None);
    }
    let Ok(state) = postcard::from_bytes::<State>(rest) else {
        return Ok(None);
    };
    let position = state.event_count() as u64;
    if position > store.log_len()? {
        return Ok(None);
    }
    let consistent = match position.checked_sub(1) {
        None => true,
        Some(last) => store.log_id(last)?.is_some_and(|id| state.contains(&id)),
    };
    Ok(consistent.then_some(state))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

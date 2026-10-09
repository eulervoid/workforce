use wf_core::event::{Event, EventBody, EventError};
use wf_core::id::EventId;
use wf_core::identity::{Actor, DeviceSigner};
use wf_core::refs::PARSER_VERSION;
use wf_core::state::{Disposition, Received, State, Trust};
use wf_core::validate::{Invalid, PolicyViolation};

use crate::store::{Commit, EventStore, PendingChange, SnapshotChange, StoreError, StoredSnapshot};

/// A snapshot is written once this many events have been applied since the last one.
pub const SNAPSHOT_INTERVAL: usize = 256;

/// Bumped whenever the serialized shape of [`State`] changes. Snapshots are a cache: a mismatch
/// discards the snapshot and refolds from the log.
const SNAPSHOT_FORMAT: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum ReceiveError {
    #[error(transparent)]
    Invalid(#[from] Invalid),
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[derive(Debug, thiserror::Error)]
pub enum AuthorError {
    #[error("the workforce has no genesis yet")]
    NoWorkforce,
    #[error(transparent)]
    Policy(#[from] PolicyViolation),
    #[error(transparent)]
    Sign(#[from] EventError),
    #[error(transparent)]
    Receive(#[from] ReceiveError),
}

/// A local replica: the durable log plus the state folded from it.
///
/// After a [`StoreError`] the in-memory state may be ahead of the log; drop the replica and
/// reopen it.
pub struct Replica<S: EventStore> {
    store: S,
    state: State,
    snapshot_event_count: usize,
}

impl<S: EventStore> Replica<S> {
    /// Loads the latest usable snapshot and replays the events written after it.
    pub fn open(store: S) -> Result<Self, StoreError> {
        let loaded = load_snapshot(&store)?;
        Self::fold(store, loaded)
    }

    /// Ignores any snapshot and folds the whole log, then writes a fresh snapshot.
    pub fn refold(store: S) -> Result<Self, StoreError> {
        Self::fold(store, None)
    }

    fn fold(store: S, snapshot: Option<State>) -> Result<Self, StoreError> {
        let from_snapshot = snapshot.is_some();
        let mut state = snapshot.unwrap_or_default();
        let snapshot_event_count = state.event_count();

        let mut unseen = Vec::new();
        for (author, head) in store.feed_heads()? {
            let applied = state.feed_heads().get(&author).map_or(0, |known| known.seq);
            if head.seq > applied {
                unseen.extend(store.feed_since(&author, applied)?);
            }
        }
        unseen.sort_by_key(|event| (event.content.hlc, event.id()));
        let replayed = unseen.len();
        for event in unseen {
            state
                .receive(event, Trust::Stored)
                .map_err(|error| StoreError::Inconsistent(error.to_string()))?;
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
        };
        for event in replica.store.pending()? {
            let received = replica
                .state
                .receive(event.clone(), Trust::Stored)
                .map_err(|error| StoreError::Inconsistent(error.to_string()))?;
            replica.persist(&event, &received)?;
        }
        let needs_snapshot = replayed >= SNAPSHOT_INTERVAL || (!from_snapshot && replayed > 0);
        if needs_snapshot {
            replica.write_snapshot()?;
        }
        Ok(replica)
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
        let received = self.state.receive(event.clone(), Trust::Untrusted)?;
        self.persist(&event, &received)?;
        Ok(received)
    }

    /// Checks the policy rules, signs the next event of `signer` and persists it.
    pub fn author(
        &mut self,
        signer: &DeviceSigner,
        actor: Actor,
        now_ms: u64,
        body: EventBody,
    ) -> Result<Event, AuthorError> {
        self.state.check_policy(&body)?;
        let builder = self
            .state
            .builder(signer.key(), actor, now_ms)
            .ok_or(AuthorError::NoWorkforce)?;
        let event = builder.sign(signer, body)?;
        self.receive(event.clone())?;
        Ok(event)
    }

    fn persist(&mut self, submitted: &Event, received: &Received) -> Result<(), StoreError> {
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

        let due = self.state.event_count() >= self.snapshot_event_count + SNAPSHOT_INTERVAL;
        let snapshot = due.then(|| self.snapshot()).transpose()?;
        self.store.commit(Commit {
            append: &received.applied,
            pending: PendingChange::Update {
                put,
                remove: &released,
            },
            snapshot: snapshot
                .as_ref()
                .map_or(SnapshotChange::Keep, SnapshotChange::Replace),
        })?;
        if snapshot.is_some() {
            self.snapshot_event_count = self.state.event_count();
        }
        Ok(())
    }

    fn snapshot(&self) -> Result<StoredSnapshot, StoreError> {
        let bytes = wf_core::encoding::encode(&(SNAPSHOT_FORMAT, PARSER_VERSION, &self.state))?;
        Ok(StoredSnapshot {
            covers: self.state.heads().clone(),
            bytes,
        })
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
    let Ok(((format, parser), rest)) = postcard::take_from_bytes::<(u32, u16)>(&stored.bytes)
    else {
        return Ok(None);
    };
    if format != SNAPSHOT_FORMAT || parser != PARSER_VERSION {
        return Ok(None);
    }
    let Ok(state) = postcard::from_bytes::<State>(rest) else {
        return Ok(None);
    };
    let log_heads = store.feed_heads()?;
    let behind_log = state.feed_heads().iter().all(|(author, head)| {
        log_heads
            .iter()
            .any(|(candidate, logged)| candidate == author && logged.seq >= head.seq)
    });
    Ok(behind_log.then_some(state))
}

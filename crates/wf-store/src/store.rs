use std::collections::BTreeSet;

use wf_core::encoding::EncodingError;
use wf_core::event::Event;
use wf_core::id::{EventId, WorkforceId};
use wf_core::identity::DeviceKey;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Database(#[from] redb::DatabaseError),
    #[error("transaction error: {0}")]
    Transaction(#[from] redb::TransactionError),
    #[error("table error: {0}")]
    Table(#[from] redb::TableError),
    #[error("storage error: {0}")]
    Storage(#[from] redb::StorageError),
    #[error("commit error: {0}")]
    Commit(#[from] redb::CommitError),
    #[error("stored data does not decode: {0}")]
    Corrupt(#[from] EncodingError),
    #[error("the store holds workforce {found}, not {expected}")]
    WrongWorkforce {
        expected: WorkforceId,
        found: WorkforceId,
    },
    #[error("the stored log is inconsistent: {0}")]
    Inconsistent(String),
    #[error("the database is locked by another wf process")]
    Locked,
}

/// A folded state serialized by the replica. The store treats the bytes as opaque.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredSnapshot {
    /// The heads of the state the snapshot covers.
    pub covers: BTreeSet<EventId>,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PendingChange<'a> {
    #[default]
    Keep,
    /// Events that are parked, and ids that left the pending set.
    Update {
        put: &'a [Event],
        remove: &'a [EventId],
    },
}

#[derive(Clone, Copy, Debug, Default)]
pub enum SnapshotChange<'a> {
    #[default]
    Keep,
    Replace(&'a StoredSnapshot),
    Discard,
}

/// Everything that changes in one transaction: appended events, pending set and snapshot.
#[derive(Clone, Copy, Debug, Default)]
pub struct Commit<'a> {
    pub append: &'a [Event],
    pub pending: PendingChange<'a>,
    pub snapshot: SnapshotChange<'a>,
}

/// The narrow interface the rest of the system depends on, so the backend can be swapped.
pub trait EventStore {
    fn event(&self, id: &EventId) -> Result<Option<Event>, StoreError>;

    /// The events of one author with `seq` greater than `after_seq`, in `(seq, id)` order. A
    /// equivocated feed yields every branch.
    fn feed_since(&self, author: &DeviceKey, after_seq: u64) -> Result<Vec<Event>, StoreError>;

    /// Every event, ordered by `(hlc, id)`, which is a causal order within one workforce.
    fn events(&self) -> Result<Vec<Event>, StoreError>;

    /// How many events have been appended. Events are numbered in the order they were applied,
    /// which is not always a causal one: a fork takes in its history newest event first.
    fn log_len(&self) -> Result<u64, StoreError>;

    fn log_id(&self, position: u64) -> Result<Option<EventId>, StoreError>;

    /// The events appended at `position` and after, in order.
    fn log_since(&self, position: u64) -> Result<Vec<Event>, StoreError>;

    fn pending(&self) -> Result<Vec<Event>, StoreError>;

    fn snapshot(&self) -> Result<Option<StoredSnapshot>, StoreError>;

    /// Atomic. Events are appended in the order they were applied, which is not always causal: a
    /// fork takes in the history it was made from starting with the newest event.
    fn commit(&self, commit: Commit<'_>) -> Result<(), StoreError>;
}

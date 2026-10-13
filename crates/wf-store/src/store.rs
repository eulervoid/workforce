use wf_core::encoding::EncodingError;
use wf_core::event::Event;
use wf_core::id::{EventId, WorkforceId};
use wf_core::identity::DeviceKey;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error")]
    Database(#[from] redb::DatabaseError),
    #[error("transaction error")]
    Transaction(#[from] redb::TransactionError),
    #[error("table error")]
    Table(#[from] redb::TableError),
    #[error("storage error")]
    Storage(#[from] redb::StorageError),
    #[error("commit error")]
    Commit(#[from] redb::CommitError),
    #[error("stored data does not decode")]
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

/// The snapshot is a folded state serialized by the replica; the store treats its bytes as opaque.
#[derive(Clone, Copy, Debug, Default)]
pub enum SnapshotChange<'a> {
    #[default]
    Keep,
    Replace(&'a [u8]),
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

    /// How many events have been appended. Events are numbered in the order they were applied,
    /// which is not always a causal one: a fork takes in its history newest event first.
    fn log_len(&self) -> Result<u64, StoreError>;

    fn log_id(&self, position: u64) -> Result<Option<EventId>, StoreError>;

    /// The events appended at `position` and after, in order.
    fn log_since(&self, position: u64) -> Result<Vec<Event>, StoreError> {
        self.log_page(position, usize::MAX)
    }

    /// At most `limit` of the events appended at `position` and after, in order.
    fn log_page(&self, position: u64, limit: usize) -> Result<Vec<Event>, StoreError>;

    fn pending(&self) -> Result<Vec<Event>, StoreError>;

    /// The bytes of the latest snapshot.
    fn snapshot(&self) -> Result<Option<Vec<u8>>, StoreError>;

    /// Atomic. Events are appended in the order they were applied, which is not always causal: a
    /// fork takes in the history it was made from starting with the newest event.
    fn commit(&self, commit: Commit<'_>) -> Result<(), StoreError>;
}

//! Durable storage for the event log, and the replica that keeps folded state in step with it.

mod redb_store;
mod replica;
mod store;

pub use redb_store::RedbStore;
pub use replica::{AuthorError, ReceiveError, Replica, SNAPSHOT_INTERVAL};
pub use store::{Commit, EventStore, PendingChange, SnapshotChange, StoreError, StoredSnapshot};

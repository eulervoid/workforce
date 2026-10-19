//! Durable storage for the event log, and the replica that keeps folded state in step with it.

mod binding;
mod exchange;
mod history;
mod redb_store;
mod replica;
mod store;

pub use binding::{BindError, DeviceBinding};
pub use history::{HistorySource, history_behind};
pub use redb_store::RedbStore;
pub use replica::{AuthorError, ForkError, ReceiveError, Replica, SNAPSHOT_INTERVAL};
pub use store::{Commit, EventStore, PendingChange, SnapshotChange, StoreError};

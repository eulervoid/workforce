//! Networking for workforces: the sync protocol over any byte stream, and (later) the iroh
//! transport that carries it.

mod frame;
mod log;
mod sync;

pub use log::{LogError, ReceiveSummary, SharedReplica, SyncLog};
pub use sync::{SYNC_ALPN, SyncError, SyncReport, initiate, respond};

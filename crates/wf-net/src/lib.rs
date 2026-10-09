//! Networking for workforces: the sync protocol over any byte stream, and (later) the iroh
//! transport that carries it.

mod frame;
mod log;
mod node;
mod sync;

pub use log::{LogError, ReceiveSummary, SharedReplica, SyncLog};
pub use node::{NetError, Network, Node, device_key, endpoint_id};
pub use sync::{SYNC_ALPN, SyncError, SyncReport, initiate, read_open, respond};

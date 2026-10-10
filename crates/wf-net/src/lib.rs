//! Networking for workforces: the sync protocol over any byte stream, and (later) the iroh
//! transport that carries it.

pub mod frame;
mod join;
mod log;
mod node;
pub mod replica;
mod sync;

pub use join::JOIN_ALPN;
pub use log::{LogError, ReceiveSummary, SharedReplica, SyncLog};
pub use node::{NetError, Network, Node, device_key, endpoint_id};
pub use sync::{SYNC_ALPN, SyncError, SyncReport, initiate, read_open, respond};

pub use iroh::{EndpointAddr, Signature};

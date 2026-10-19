//! Networking for workforces: the sync protocol over any byte stream, and the iroh
//! transport that carries it.

pub mod frame;
mod gossip;
mod join;
mod log;
mod node;
mod peek;
mod sync;

pub use gossip::{GOSSIP_ALPN, HeadsTopic, Heard, heads_digest, topic_id};
pub use join::JOIN_ALPN;
pub use log::{LogError, SyncLog};
pub use node::{NetError, Network, Node, SourceError, device_key, endpoint_id};
pub use peek::{
    Answer, PEEK_ALPN, PeekError, PeekSource, Request, Work, initiate as peek_initiate,
    initiate_within as peek_initiate_within, read_request, respond as peek_respond,
    respond_within as peek_respond_within,
};
pub use sync::{
    IDLE_TIMEOUT, SYNC_ALPN, SyncError, SyncReport, initiate, initiate_within, read_open, respond,
    respond_within,
};

pub use iroh::{EndpointAddr, Signature};

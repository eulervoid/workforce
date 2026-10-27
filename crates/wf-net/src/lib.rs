//! Networking for workforces: the sync protocol over any byte stream, and the iroh
//! transport that carries it.

pub mod frame;
mod git;
mod gossip;
mod join;
mod log;
mod node;
mod query;
mod sync;

pub use git::{
    GIT_ALPN, GitError, GitProcess, GitRequest, GitSource, initiate as git_initiate,
    read_git_request, respond as git_respond,
};
pub use gossip::{GOSSIP_ALPN, HeadsTopic, Heard, heads_digest, topic_id};
pub use join::JOIN_ALPN;
pub use log::{LogError, SyncLog};
pub use node::{NetError, Network, Node, SourceError, device_key, endpoint_id};
pub use query::{
    DiffAnswer, DiffRequest, QUERY_ALPN, Query, QueryError, QueryRequest, QuerySource,
    TRANSCRIPT_PART_LEN, TranscriptAnswer, TranscriptRequest, Work, initiate as query_initiate,
    initiate_within as query_initiate_within, read_query, respond as query_respond,
    respond_within as query_respond_within,
};
pub use sync::{
    IDLE_TIMEOUT, SYNC_ALPN, SyncError, SyncReport, initiate, initiate_within, read_open, respond,
    respond_within,
};

pub use iroh::{EndpointAddr, Signature};

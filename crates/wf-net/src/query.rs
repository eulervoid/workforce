//! `wf/query/1`: ask the device that has the worktree of an item what its agent is changing.
//!
//! The initiator sends one [`Query`] and the responder answers with one [`DiffAnswer`]. A live diff
//! is local data nobody needs to keep (P11), so it is asked of the one device that has it and
//! stored nowhere. A peer that sends or accepts nothing for [`IDLE_TIMEOUT`] ends the exchange.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};
use wf_core::id::{ItemId, WorkforceId};
use wf_core::session::CommitId;

use crate::frame::{FrameError, MAX_FRAME_LEN, read_frame_within, write_frame_within};
use crate::node::SourceError;
use crate::sync::IDLE_TIMEOUT;

pub const QUERY_ALPN: &[u8] = b"wf/query/1";

/// A request is a few ids; a peer that sends more is not speaking the protocol.
const REQUEST_FRAME_LEN: u32 = 1024;

#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error("the device could not read the work of the item")]
    Source(#[source] SourceError),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Query {
    /// The changes of the work on an item. `since` is the head commit the asker last saw; the
    /// answer is [`DiffAnswer::Unchanged`] while the head is still that one, so polling is one small
    /// frame.
    Diff {
        workforce: WorkforceId,
        item: ItemId,
        since: Option<CommitId>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffAnswer {
    /// Nothing moved since `since`.
    Unchanged,
    Work(Work),
    /// This device has no branch or worktree of the item.
    NotHere,
    /// The work is larger than the largest answer; the asker sees it is not a broken connection.
    TooLarge,
}

/// The changes of the work on an item. What is not committed yet is kept apart from what is, and
/// is not part of this version of the protocol.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Work {
    /// The branch the commits are compared against.
    pub target: String,
    /// The head commit of the work.
    pub head: CommitId,
    /// The diff of the head commit against the target, as git prints it. The asker parses it
    /// where it needs files.
    pub committed: String,
}

/// What the responder reads the work of an item from, which the daemon implements with git.
pub trait QuerySource: Send + Sync {
    /// The answer to a request for the diff of `item`: [`DiffAnswer::Unchanged`] when its head is
    /// `since`, [`DiffAnswer::NotHere`] when this device has no work on it.
    fn diff(
        &self,
        workforce: WorkforceId,
        item: ItemId,
        since: Option<CommitId>,
    ) -> n0_future::boxed::BoxFuture<Result<DiffAnswer, SourceError>>;
}

/// Reads the request of the initiator, giving up when nothing arrives for `idle`. The responder
/// checks the workforce, then calls [`respond`].
pub async fn read_query<S>(stream: &mut S, idle: Duration) -> Result<Query, QueryError>
where
    S: AsyncRead + Unpin,
{
    Ok(read_frame_within(stream, REQUEST_FRAME_LEN, idle).await?)
}

/// Asks for the answer to `request` over `stream`.
pub async fn initiate<S>(stream: S, request: &Query) -> Result<DiffAnswer, QueryError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    initiate_within(stream, request, IDLE_TIMEOUT).await
}

/// Like [`initiate`], giving up when the peer is silent for `idle`.
pub async fn initiate_within<S>(
    mut stream: S,
    request: &Query,
    idle: Duration,
) -> Result<DiffAnswer, QueryError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    write_frame_within(&mut stream, request, idle).await?;
    Ok(read_frame_within(&mut stream, MAX_FRAME_LEN, idle).await?)
}

/// Answers `request`, which came from [`read_query`].
pub async fn respond<S>(
    stream: S,
    request: Query,
    source: &dyn QuerySource,
) -> Result<(), QueryError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    respond_within(stream, request, source, IDLE_TIMEOUT).await
}

/// Like [`respond`], giving up when the peer stops accepting for `idle`.
pub async fn respond_within<S>(
    mut stream: S,
    request: Query,
    source: &dyn QuerySource,
    idle: Duration,
) -> Result<(), QueryError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let answer = match request {
        Query::Diff {
            workforce,
            item,
            since,
        } => source
            .diff(workforce, item, since)
            .await
            .map_err(QueryError::Source)?,
    };
    match write_frame_within(&mut stream, &answer, idle).await {
        Err(FrameError::TooLarge(_)) => {
            Ok(write_frame_within(&mut stream, &DiffAnswer::TooLarge, idle).await?)
        }
        written => Ok(written?),
    }
}

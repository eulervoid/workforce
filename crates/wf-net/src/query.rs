//! `wf/query/1`: ask the device a session runs on about it: what its agent is changing, and what
//! its transcript says.
//!
//! The initiator sends one [`Query`] and the responder answers with one answer of the type the
//! request names ([`QueryRequest::Answer`]). A live diff and a transcript are local data nobody
//! needs to keep (P11), so they are asked of the one device that has them. A peer that sends or
//! accepts nothing for [`IDLE_TIMEOUT`] ends the exchange.

use std::io::{Read, Seek, SeekFrom};
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};
use wf_core::id::{ItemId, SessionId, WorkforceId};
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

/// The most bytes of a transcript one answer carries; the asker asks again for the rest.
pub const TRANSCRIPT_PART_LEN: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Query {
    Diff(DiffRequest),
    Transcript(TranscriptRequest),
}

/// The changes of the work on an item. `since` is the head commit the asker last saw; the answer
/// is [`DiffAnswer::Unchanged`] while the head is still that one, so polling is one small frame.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffRequest {
    pub workforce: WorkforceId,
    pub item: ItemId,
    pub since: Option<CommitId>,
}

/// The transcript of a session of the answering device, from byte `from` on: the asker keeps a
/// copy and asks for what follows its end.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptRequest {
    pub workforce: WorkforceId,
    pub session: SessionId,
    pub from: u64,
}

/// A request of `wf/query/1` and the answer it gets.
pub trait QueryRequest: Clone + Into<Query> {
    type Answer: DeserializeOwned;
}

impl From<DiffRequest> for Query {
    fn from(request: DiffRequest) -> Self {
        Self::Diff(request)
    }
}

impl From<TranscriptRequest> for Query {
    fn from(request: TranscriptRequest) -> Self {
        Self::Transcript(request)
    }
}

impl QueryRequest for DiffRequest {
    type Answer = DiffAnswer;
}

impl QueryRequest for TranscriptRequest {
    type Answer = TranscriptAnswer;
}

impl Query {
    pub fn workforce(&self) -> WorkforceId {
        match self {
            Self::Diff(request) => request.workforce,
            Self::Transcript(request) => request.workforce,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffAnswer {
    /// Nothing moved since `since`.
    Unchanged,
    Work(Work),
    /// This device has no branch or agent repository of the item.
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

/// What follows byte `from` of a session's transcript.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TranscriptAnswer {
    /// The bytes after `from`, up to [`TRANSCRIPT_PART_LEN`]; more may follow, because the session
    /// is live or the part stops at the limit.
    Part(Vec<u8>),
    /// The session ended and these are the last bytes of its transcript, maybe none.
    Last(Vec<u8>),
    /// The session is not one of the answering device.
    NotHere,
}

impl TranscriptAnswer {
    /// The answer from `transcript` to a request from byte `from`, up to `limit` bytes. `ended`
    /// is whether the session had ended before the transcript was opened, so that nothing more
    /// can be written to it.
    pub fn read(
        mut transcript: impl Read + Seek,
        from: u64,
        limit: usize,
        ended: bool,
    ) -> std::io::Result<Self> {
        transcript.seek(SeekFrom::Start(from))?;
        let mut bytes = Vec::new();
        // One byte past the limit tells a part that stops at the limit from the last one.
        transcript.take(limit as u64 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > limit {
            bytes.truncate(limit);
            return Ok(Self::Part(bytes));
        }
        Ok(if ended {
            Self::Last(bytes)
        } else {
            Self::Part(bytes)
        })
    }
}

/// What the responder reads the work and the transcripts of sessions from, which the daemon
/// implements with git and its transcript files.
pub trait QuerySource: Send + Sync {
    /// The answer to a request for the diff of `item`: [`DiffAnswer::Unchanged`] when its head is
    /// `since`, [`DiffAnswer::NotHere`] when this device has no work on it.
    fn diff(
        &self,
        workforce: WorkforceId,
        item: ItemId,
        since: Option<CommitId>,
    ) -> n0_future::boxed::BoxFuture<Result<DiffAnswer, SourceError>>;

    /// The answer to a request for the transcript of `session` from byte `from`, of at most
    /// `limit` bytes ([`TranscriptAnswer::read`]); [`TranscriptAnswer::NotHere`] when the session
    /// is not one of this device.
    fn transcript(
        &self,
        workforce: WorkforceId,
        session: SessionId,
        from: u64,
        limit: usize,
    ) -> n0_future::boxed::BoxFuture<Result<TranscriptAnswer, SourceError>>;
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
pub async fn initiate<S, R>(stream: S, request: &R) -> Result<R::Answer, QueryError>
where
    S: AsyncRead + AsyncWrite + Unpin,
    R: QueryRequest,
{
    initiate_within(stream, request, IDLE_TIMEOUT).await
}

/// Like [`initiate`], giving up when the peer is silent for `idle`.
pub async fn initiate_within<S, R>(
    mut stream: S,
    request: &R,
    idle: Duration,
) -> Result<R::Answer, QueryError>
where
    S: AsyncRead + AsyncWrite + Unpin,
    R: QueryRequest,
{
    let query: Query = request.clone().into();
    write_frame_within(&mut stream, &query, idle).await?;
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
    match request {
        Query::Diff(DiffRequest {
            workforce,
            item,
            since,
        }) => {
            let answer = source
                .diff(workforce, item, since)
                .await
                .map_err(QueryError::Source)?;
            match write_frame_within(&mut stream, &answer, idle).await {
                Err(FrameError::TooLarge(_)) => {
                    Ok(write_frame_within(&mut stream, &DiffAnswer::TooLarge, idle).await?)
                }
                written => Ok(written?),
            }
        }
        Query::Transcript(TranscriptRequest {
            workforce,
            session,
            from,
        }) => {
            let answer = source
                .transcript(workforce, session, from, TRANSCRIPT_PART_LEN)
                .await
                .map_err(QueryError::Source)?;
            Ok(write_frame_within(&mut stream, &answer, idle).await?)
        }
    }
}

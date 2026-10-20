//! `wf/git/1`: git objects between devices, by piping a stream to and from a process.
//!
//! The initiator sends one [`GitRequest`] naming the workforce; from then on the stream carries
//! what `git upload-pack` on the responder's repository and `git fetch` on the initiator's say to
//! each other, which git alone understands. This crate only moves the bytes; the daemon starts the
//! process, through `wf-vcs`.

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use wf_core::id::WorkforceId;

use crate::frame::{FrameError, read_frame_within, write_frame_within};
use crate::node::SourceError;
use crate::sync::IDLE_TIMEOUT;

pub const GIT_ALPN: &[u8] = b"wf/git/1";

/// A request is one workforce id; a peer that sends more is not speaking the protocol.
const REQUEST_FRAME_LEN: u32 = 256;

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error("the device could not start git")]
    Source(#[source] SourceError),
    #[error("the stream to git broke")]
    Io(#[from] std::io::Error),
    #[error("git did not finish cleanly")]
    Process(#[source] SourceError),
}

/// Whose repository the initiator wants to fetch from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitRequest {
    pub workforce: WorkforceId,
}

/// A running `git upload-pack` on the repository of a workforce.
pub struct GitProcess {
    /// What the peer sends goes here; dropping it is the end of the peer's input.
    pub input: Box<dyn AsyncWrite + Send + Unpin>,
    /// What the process answers.
    pub output: Box<dyn AsyncRead + Send + Unpin>,
    /// Resolves when the process has exited, with the error of one that failed.
    pub finished: n0_future::boxed::BoxFuture<Result<(), SourceError>>,
}

/// Where the responder's `git upload-pack` comes from, which the daemon implements with git.
pub trait GitSource: Send + Sync {
    /// Starts `git upload-pack` on the repository of `workforce`, fetch only.
    fn upload_pack(
        &self,
        workforce: WorkforceId,
    ) -> n0_future::boxed::BoxFuture<Result<GitProcess, SourceError>>;
}

/// Reads the request of the initiator, giving up when nothing arrives for `idle`. The responder
/// checks the workforce, then calls [`respond`].
pub async fn read_git_request<S>(
    stream: &mut S,
    idle: std::time::Duration,
) -> Result<GitRequest, GitError>
where
    S: AsyncRead + Unpin,
{
    Ok(read_frame_within(stream, REQUEST_FRAME_LEN, idle).await?)
}

/// Pipes `stream` to the process of `source` until both are done.
pub async fn respond<S>(
    stream: S,
    request: GitRequest,
    source: &dyn GitSource,
) -> Result<(), GitError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let process = source
        .upload_pack(request.workforce)
        .await
        .map_err(GitError::Source)?;
    splice(stream, process.output, process.input).await?;
    process.finished.await.map_err(GitError::Process)
}

/// Asks the responder for its repository over `stream`, and pipes it to `local`: the side of
/// `git fetch` that talks to the daemon.
pub async fn initiate<S, L>(mut stream: S, request: &GitRequest, local: L) -> Result<(), GitError>
where
    S: AsyncRead + AsyncWrite + Unpin,
    L: AsyncRead + AsyncWrite + Unpin,
{
    write_frame_within(&mut stream, request, IDLE_TIMEOUT).await?;
    let (read, write) = tokio::io::split(local);
    splice(stream, read, write).await
}

/// Copies `stream` into `into` and `from` into `stream`, until both directions have ended. An end
/// is passed on: when one side stops sending, the other is told by a shutdown, which is how git
/// learns that the peer is done. A broken direction ends both.
async fn splice<S, R, W>(stream: S, mut from: R, mut into: W) -> Result<(), GitError>
where
    S: AsyncRead + AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let (mut stream_read, mut stream_write) = tokio::io::split(stream);
    let inbound = async {
        tokio::io::copy(&mut stream_read, &mut into).await?;
        into.shutdown().await
    };
    let outbound = async {
        tokio::io::copy(&mut from, &mut stream_write).await?;
        stream_write.shutdown().await
    };
    tokio::try_join!(inbound, outbound)?;
    Ok(())
}

//! `wf/git/1`: git objects between devices, by piping a stream to and from a process.
//!
//! The initiator sends one [`GitRequest`] naming the workforce; from then on the stream carries
//! what `git upload-pack` on the responder's repository and `git fetch` on the initiator's say to
//! each other, which git alone understands. This crate only moves the bytes; the daemon starts the
//! process, through `wf-vcs`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::Instant;
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
    #[error("nothing moved either way for {} seconds", .0.as_secs())]
    Silent(Duration),
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
pub async fn read_git_request<S>(stream: &mut S, idle: Duration) -> Result<GitRequest, GitError>
where
    S: AsyncRead + Unpin,
{
    Ok(read_frame_within(stream, REQUEST_FRAME_LEN, idle).await?)
}

/// Pipes `stream` to the process of `source` until both are done, or until neither side has said
/// anything for `silence`. The process is waited for either way, so none is left behind.
pub async fn respond<S>(
    stream: S,
    request: GitRequest,
    source: &dyn GitSource,
    silence: Duration,
) -> Result<(), GitError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let GitProcess {
        input,
        output,
        finished,
    } = source
        .upload_pack(request.workforce)
        .await
        .map_err(GitError::Source)?;
    let spliced = splice(stream, output, input, silence).await;
    // Its input and output are closed by now, so the process ends whatever it was doing.
    let finished = finished.await;
    spliced?;
    finished.map_err(GitError::Process)
}

/// Asks the responder for its repository over `stream`, and pipes it to `local`, the side of
/// `git fetch` that talks to the daemon, until both are done or neither side has said anything
/// for `silence`.
pub async fn initiate<S, L>(
    mut stream: S,
    request: &GitRequest,
    local: L,
    silence: Duration,
) -> Result<(), GitError>
where
    S: AsyncRead + AsyncWrite + Unpin,
    L: AsyncRead + AsyncWrite + Unpin,
{
    write_frame_within(&mut stream, request, IDLE_TIMEOUT).await?;
    let (read, write) = tokio::io::split(local);
    splice(stream, read, write, silence).await
}

/// When either direction of a splice last moved a byte, in milliseconds since it started.
struct Heard {
    start: Instant,
    last: AtomicU64,
}

impl Heard {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            last: AtomicU64::new(0),
        }
    }

    fn now(&self) {
        let elapsed = u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last.fetch_max(elapsed, Ordering::Relaxed);
    }

    /// Resolves once nothing was heard for `silence`.
    async fn silent_for(&self, silence: Duration) {
        loop {
            let last = self.start + Duration::from_millis(self.last.load(Ordering::Relaxed));
            let quiet = last.elapsed();
            if quiet >= silence {
                return;
            }
            tokio::time::sleep(silence - quiet).await;
        }
    }
}

/// Copies `from` into `into` until `from` ends, then passes the end on with a shutdown, which is
/// how git learns that the peer is done. Every byte that moves is `heard`. A reader that went
/// away (a process that answered and exited) ends the copy without an error: what it said
/// already still goes the other way.
async fn copy<R, W>(mut from: R, mut into: W, heard: &Heard) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buffer = vec![0; 16 * 1024];
    loop {
        let read = from.read(&mut buffer).await?;
        heard.now();
        if read == 0 {
            return match into.shutdown().await {
                Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
                shut => shut,
            };
        }
        let mut written = 0;
        while written < read {
            match into.write(&buffer[written..read]).await {
                Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
                Ok(more) => written += more,
                Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => return Ok(()),
                Err(error) => return Err(error),
            }
            heard.now();
        }
    }
}

/// Copies `stream` into `into` and `from` into `stream`, until both directions have ended, or
/// until nothing moved either way for `silence`: a long pack keeps bytes flowing, a stalled peer
/// does not. A broken direction does not cut the other short, so an answer still arrives.
async fn splice<S, R, W>(stream: S, from: R, into: W, silence: Duration) -> Result<(), GitError>
where
    S: AsyncRead + AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let (stream_read, stream_write) = tokio::io::split(stream);
    let heard = Heard::new();
    let both = async {
        tokio::join!(
            copy(stream_read, into, &heard),
            copy(from, stream_write, &heard)
        )
    };
    tokio::select! {
        (inbound, outbound) = both => {
            outbound?;
            inbound?;
            Ok(())
        }
        () = heard.silent_for(silence) => Err(GitError::Silent(silence)),
    }
}

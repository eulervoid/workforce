//! The transcripts of sessions on other devices.
//!
//! The device of a session answers for its transcript over `wf/query/1`. While the principal
//! follows a session of another device, the daemon asks that device for what is new and appends it
//! to a copy, which the CLI reads as it reads the transcripts of this device. A copy is a cache: it
//! is kept apart from the transcripts of this device, and once removed it is fetched again from
//! the start.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use tokio::time::Instant;
use wf_core::id::{SessionId, WorkforceId};
use wf_core::identity::DeviceKey;
use wf_net::TranscriptAnswer;

use crate::proto::TranscriptCopy;
use crate::{Daemon, StorageClosed};

/// How often a followed transcript is asked for what is new.
const ASK_EVERY: Duration = Duration::from_secs(1);
/// The longest wait between two asks of a device that cannot be reached.
const LONGEST_BACKOFF: Duration = Duration::from_secs(30);
/// A transcript is followed this long after the last call that follows it.
const FOLLOWED_FOR: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum TranscriptError {
    #[error("workforce {0} is not loaded")]
    NotLoaded(WorkforceId),
    #[error("there is no session {0}")]
    NoSession(SessionId),
    #[error("session {0} runs on this device; its transcript is no copy")]
    Here(SessionId),
    #[error(transparent)]
    Storage(#[from] StorageClosed),
    #[error("could not read the transcript")]
    Read(#[source] std::io::Error),
    #[error("reading the transcript was cut short")]
    Task(#[source] tokio::task::JoinError),
}

/// The copies followed, by workforce and session. An entry that is not [`TranscriptCopy::Complete`]
/// has a task that keeps its copy up to date until `until`.
#[derive(Default)]
pub(crate) struct Followed(Mutex<HashMap<(WorkforceId, SessionId), Follow>>);

struct Follow {
    until: Instant,
    copy: TranscriptCopy,
}

impl Followed {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(WorkforceId, SessionId), Follow>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Daemon {
    /// The answer of this device to a request for the transcript of `session` from byte `from`:
    /// [`TranscriptAnswer::NotHere`] when the session is not one of this device.
    pub(crate) async fn transcript_here(
        &self,
        workforce: WorkforceId,
        session: SessionId,
        from: u64,
        limit: usize,
    ) -> Result<TranscriptAnswer, TranscriptError> {
        let loaded = self
            .loaded_workforce(&workforce)
            .ok_or(TranscriptError::NotLoaded(workforce))?;
        let here = self.device();
        // Read before the transcript, so that an ended session's transcript is all there.
        let ended = loaded
            .storage
            .with(move |replica| {
                replica
                    .state()
                    .session(session)
                    .filter(|session| session.device() == here)
                    .map(|session| !session.is_live())
            })
            .await?;
        let Some(ended) = ended else {
            return Ok(TranscriptAnswer::NotHere);
        };
        let path = self.launch.data_dir.transcript(&workforce, session);
        tokio::task::spawn_blocking(move || match std::fs::File::open(&path) {
            Ok(transcript) => TranscriptAnswer::read(transcript, from, limit, ended),
            // The agent has not written anything yet, or never did.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                TranscriptAnswer::read(std::io::Cursor::new([]), from, limit, ended)
            }
            Err(error) => Err(error),
        })
        .await
        .map_err(TranscriptError::Task)?
        .map_err(TranscriptError::Read)
    }

    /// Follows the transcript of `session`, a session of another device, for a while: what the
    /// copy of it came to so far. The first call starts fetching it; each call keeps it followed
    /// for [`FOLLOWED_FOR`].
    pub(crate) async fn follow_transcript(
        self: &Arc<Self>,
        workforce: WorkforceId,
        session: SessionId,
    ) -> Result<TranscriptCopy, TranscriptError> {
        let loaded = self
            .loaded_workforce(&workforce)
            .ok_or(TranscriptError::NotLoaded(workforce))?;
        let device = loaded
            .storage
            .with(move |replica| {
                replica
                    .state()
                    .session(session)
                    .map(|session| session.device())
            })
            .await?
            .ok_or(TranscriptError::NoSession(session))?;
        if device == self.device() {
            return Err(TranscriptError::Here(session));
        }
        let copy = self.launch.data_dir.transcript_copy(&workforce, session);
        let until = Instant::now() + FOLLOWED_FOR;
        let mut followed = self.followed.lock();
        if let Some(follow) = followed.get_mut(&(workforce, session)) {
            follow.until = until;
            // A complete copy that was removed is fetched again.
            if follow.copy != TranscriptCopy::Complete || copy.is_file() {
                return Ok(follow.copy.clone());
            }
        }
        followed.insert(
            (workforce, session),
            Follow {
                until,
                copy: TranscriptCopy::Fetching,
            },
        );
        crate::spawn(keep_copy(
            Arc::downgrade(self),
            workforce,
            session,
            device,
            copy,
        ));
        Ok(TranscriptCopy::Fetching)
    }
}

/// Asks `device` for what is new in the transcript of `session` and appends it to `copy`, until
/// the session ended and the copy is complete or nobody followed it for [`FOLLOWED_FOR`].
async fn keep_copy(
    daemon: Weak<Daemon>,
    workforce: WorkforceId,
    session: SessionId,
    device: DeviceKey,
    copy: PathBuf,
) {
    let mut backoff = ASK_EVERY;
    loop {
        let Some(daemon) = daemon.upgrade() else {
            return;
        };
        {
            let mut followed = daemon.followed.lock();
            let expired = followed
                .get(&(workforce, session))
                .is_none_or(|follow| follow.until <= Instant::now());
            if expired || *daemon.halted.borrow() {
                followed.remove(&(workforce, session));
                return;
            }
        }
        let (now, wait) = match fetch(&daemon, workforce, session, device, &copy).await {
            Ok(Fetched::Part { full: true }) => (TranscriptCopy::Fetching, Duration::ZERO),
            Ok(Fetched::Part { full: false }) => (TranscriptCopy::CaughtUp, ASK_EVERY),
            Ok(Fetched::Last) => (TranscriptCopy::Complete, Duration::ZERO),
            Ok(Fetched::NotHere) => (TranscriptCopy::NotHere { device }, backoff),
            Err(reason) => (TranscriptCopy::Unreachable { device, reason }, backoff),
        };
        backoff = match now {
            TranscriptCopy::Unreachable { .. } | TranscriptCopy::NotHere { .. } => {
                (backoff * 2).min(LONGEST_BACKOFF)
            }
            TranscriptCopy::Fetching | TranscriptCopy::CaughtUp | TranscriptCopy::Complete => {
                ASK_EVERY
            }
        };
        let complete = now == TranscriptCopy::Complete;
        if let Some(follow) = daemon.followed.lock().get_mut(&(workforce, session)) {
            follow.copy = now;
        }
        if complete {
            return;
        }
        drop(daemon);
        tokio::time::sleep(wait).await;
    }
}

/// What one ask for the transcript came to.
enum Fetched {
    /// More may follow; `full` when the part stopped at the limit, so more is there already.
    Part {
        full: bool,
    },
    Last,
    NotHere,
}

/// Asks `device` for what follows the end of `copy` and appends it. Fails with why the device
/// could not be asked, or the copy not written.
async fn fetch(
    daemon: &Daemon,
    workforce: WorkforceId,
    session: SessionId,
    device: DeviceKey,
    copy: &std::path::Path,
) -> Result<Fetched, String> {
    let from = std::fs::metadata(copy).map_or(0, |metadata| metadata.len());
    let node = daemon
        .node()
        .await
        .map_err(|error| wf_core::error_chain(&error))?;
    let answer = node
        .query_transcript(wf_net::endpoint_id(device), workforce, session, from)
        .await
        .map_err(|error| wf_core::error_chain(&error))?;
    let (bytes, fetched) = match answer {
        TranscriptAnswer::Part(bytes) => {
            let full = bytes.len() >= wf_net::TRANSCRIPT_PART_LEN;
            (bytes, Fetched::Part { full })
        }
        TranscriptAnswer::Last(bytes) => (bytes, Fetched::Last),
        TranscriptAnswer::NotHere => return Ok(Fetched::NotHere),
    };
    append(copy, &bytes).map_err(|error| {
        format!(
            "could not write the copy at {}: {}",
            copy.display(),
            wf_core::error_chain(&error)
        )
    })?;
    Ok(fetched)
}

fn append(copy: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(directory) = copy.parent() {
        std::fs::create_dir_all(directory)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(copy)?;
    file.write_all(bytes)
}

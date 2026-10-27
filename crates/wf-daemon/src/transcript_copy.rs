//! The transcripts of sessions on other devices: the device of a session answers for its
//! transcript over `wf/query/1`.

use wf_core::id::{SessionId, WorkforceId};
use wf_net::TranscriptAnswer;

use crate::{Daemon, StorageClosed};

#[derive(Debug, thiserror::Error)]
pub enum TranscriptError {
    #[error("workforce {0} is not loaded")]
    NotLoaded(WorkforceId),
    #[error(transparent)]
    Storage(#[from] StorageClosed),
    #[error("could not read the transcript")]
    Read(#[source] std::io::Error),
    #[error("reading the transcript was cut short")]
    Task(#[source] tokio::task::JoinError),
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
}

//! The answers of this device to `wf/query/1`: the work and the transcripts of its sessions.

use std::sync::{Arc, Weak};

use n0_future::boxed::BoxFuture;
use wf_core::id::{ItemId, SessionId, WorkforceId};
use wf_core::session::CommitId;
use wf_net::{DiffAnswer, SourceError, TranscriptAnswer};

use crate::Daemon;

#[derive(Debug, thiserror::Error)]
#[error("the daemon is shutting down")]
struct Gone;

/// Answers `wf/query/1` from the agent repositories and the transcripts of this machine.
pub(crate) struct QueryResponder(Weak<Daemon>);

impl QueryResponder {
    pub(crate) fn new(daemon: &Arc<Daemon>) -> Arc<Self> {
        Arc::new(Self(Arc::downgrade(daemon)))
    }
}

impl wf_net::QuerySource for QueryResponder {
    fn diff(
        &self,
        workforce: WorkforceId,
        item: ItemId,
        since: Option<CommitId>,
    ) -> BoxFuture<Result<DiffAnswer, SourceError>> {
        let daemon = self.0.clone();
        Box::pin(async move {
            let daemon = daemon.upgrade().ok_or(Gone)?;
            Ok(daemon.diff_here(workforce, item, since).await?)
        })
    }

    fn transcript(
        &self,
        workforce: WorkforceId,
        session: SessionId,
        from: u64,
        limit: usize,
    ) -> BoxFuture<Result<TranscriptAnswer, SourceError>> {
        let daemon = self.0.clone();
        Box::pin(async move {
            let daemon = daemon.upgrade().ok_or(Gone)?;
            Ok(daemon
                .transcript_here(workforce, session, from, limit)
                .await?)
        })
    }
}

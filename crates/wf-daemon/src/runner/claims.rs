//! Claims across devices, as the log has them after a sync: a session of this device whose claim
//! an earlier one holds ends `Outclaimed`.

use std::sync::Arc;

use wf_core::id::{HashId, WorkforceId};
use wf_core::lifecycle::{Interruption, SessionEnd};

use crate::{Daemon, Loaded};

impl Daemon {
    /// Ends the sessions of this device that lost their claim to an earlier one, at once and
    /// keeping their commits: the end names the head of a work session's branch.
    pub(super) async fn end_lost_claims(
        self: &Arc<Self>,
        workforce: WorkforceId,
        loaded: &Arc<Loaded>,
    ) {
        let device = self.device();
        let Ok(lost) = self
            .read(loaded, move |state| state.outclaimed(device))
            .await
        else {
            return;
        };
        for session in lost {
            let ended = self
                .finish_session(
                    workforce,
                    loaded,
                    session,
                    SessionEnd::Interruption(Interruption::Outclaimed),
                )
                .await;
            match ended {
                Ok(_) => tracing::info!(
                    "session {}: an earlier claim holds its item",
                    session.short()
                ),
                Err(error) => tracing::warn!(
                    "could not end session {}, which lost its claim: {}",
                    session.short(),
                    wf_core::error_chain(&error)
                ),
            }
        }
    }
}

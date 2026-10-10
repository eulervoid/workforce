//! `wf/join/1`: a new device redeems an invite with a member that serves the workforce.
//!
//! The connection authenticates the joiner's device key. The member checks the proof, reserves
//! the invite so a second device cannot use it while this join is in progress, and streams the
//! log with the sync protocol. The joiner then writes its `DeviceBound` and `DeviceProfile` and
//! pushes them with the sync protocol again, on the same stream.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use iroh::Signature;
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};
use serde::{Deserialize, Serialize};
use wf_core::id::{EventId, WorkforceId};
use wf_core::invite::JoinRefusal;

use crate::frame::{read_frame, write_frame};
use crate::log::SyncLog;
use crate::node::{Registry, device_key};
use crate::sync::{initiate, read_open, respond};

pub const JOIN_ALPN: &[u8] = b"wf/join/1";

const FAILED: u32 = 500;

#[derive(Serialize, Deserialize)]
pub(crate) struct JoinRequest {
    pub workforce: WorkforceId,
    pub invite: EventId,
    pub proof: Signature,
}

#[derive(Serialize, Deserialize)]
pub(crate) enum JoinReply {
    Accepted,
    Refused(JoinRefusal),
}

/// Invites that have a join in progress.
#[derive(Clone, Default)]
pub(crate) struct Reservations(Arc<Mutex<BTreeSet<EventId>>>);

pub(crate) struct Reservation {
    invite: EventId,
    reservations: Reservations,
}

impl Reservations {
    fn reserve(&self, invite: EventId) -> Option<Reservation> {
        let mut held = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        held.insert(invite).then(|| Reservation {
            invite,
            reservations: self.clone(),
        })
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.reservations
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.invite);
    }
}

pub(crate) struct JoinHandler<L> {
    pub(crate) registry: Registry<L>,
    pub(crate) reservations: Reservations,
}

impl<L> std::fmt::Debug for JoinHandler<L> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("JoinHandler")
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

impl<L: SyncLog + Clone + 'static> JoinHandler<L> {
    async fn decide(
        &self,
        request: &JoinRequest,
        joiner: wf_core::identity::DeviceKey,
    ) -> Result<(L, Reservation), JoinRefusal> {
        let log = self
            .registry
            .get(&request.workforce)
            .ok_or(JoinRefusal::NotServed)?;
        log.check_join(request.invite, joiner, request.proof, now_ms())
            .await
            .map_err(|_| JoinRefusal::NotServed)??;
        let reservation = self
            .reservations
            .reserve(request.invite)
            .ok_or(JoinRefusal::Reserved)?;
        Ok((log, reservation))
    }
}

impl<L: SyncLog + Clone + 'static> ProtocolHandler for JoinHandler<L> {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let joiner = device_key(connection.remote_id());
        let (mut send, mut recv) = connection
            .accept_bi()
            .await
            .map_err(AcceptError::from_err)?;
        let request: JoinRequest = read_frame(&mut recv).await.map_err(AcceptError::from_err)?;
        let (log, _reservation) = match self.decide(&request, joiner).await {
            Ok(accepted) => accepted,
            Err(refusal) => {
                write_frame(&mut send, &JoinReply::Refused(refusal))
                    .await
                    .map_err(AcceptError::from_err)?;
                let _ = send.finish();
                connection.closed().await;
                return Ok(());
            }
        };
        write_frame(&mut send, &JoinReply::Accepted)
            .await
            .map_err(AcceptError::from_err)?;
        // The log goes out first, then the joiner's own events come in.
        for _ in 0..2 {
            let result = async {
                read_open(&mut recv).await?;
                respond(tokio::io::join(&mut recv, &mut send), &log).await
            }
            .await;
            if let Err(error) = result {
                connection.close(FAILED.into(), b"join failed");
                return Err(AcceptError::from_err(error));
            }
        }
        let _ = send.finish();
        connection.closed().await;
        Ok(())
    }
}

/// The joiner's half, on a stream the member accepted.
pub(crate) async fn run_join<S, L, E>(
    stream: &mut S,
    request: JoinRequest,
    log: &L,
    author: impl FnOnce() -> Result<(), E>,
) -> Result<(), JoinFailure<E>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    L: SyncLog,
{
    let workforce = request.workforce;
    write_frame(stream, &request)
        .await
        .map_err(|error| JoinFailure::Net(error.to_string()))?;
    match read_frame(stream)
        .await
        .map_err(|error| JoinFailure::Net(error.to_string()))?
    {
        JoinReply::Accepted => {}
        JoinReply::Refused(refusal) => return Err(JoinFailure::Refused(refusal)),
    }
    initiate(&mut *stream, workforce, log)
        .await
        .map_err(|error| JoinFailure::Net(error.to_string()))?;
    author().map_err(JoinFailure::Author)?;
    initiate(&mut *stream, workforce, log)
        .await
        .map_err(|error| JoinFailure::Net(error.to_string()))?;
    Ok(())
}

pub(crate) enum JoinFailure<E> {
    Net(String),
    Refused(JoinRefusal),
    Author(E),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_invite_is_reserved_until_the_join_ends() {
        let reservations = Reservations::default();
        let invite = EventId::of(b"invite");
        let held = reservations.reserve(invite).expect("free");
        assert!(reservations.reserve(invite).is_none());
        assert!(reservations.reserve(EventId::of(b"other")).is_some());
        drop(held);
        assert!(reservations.reserve(invite).is_some());
    }
}

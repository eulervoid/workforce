//! `wf/join/1`: a new device redeems an invite with a member that serves the workforce.
//!
//! The connection authenticates the joiner's device key. The member checks the proof, reserves
//! the invite so a second device cannot use it while this join is in progress, and streams the
//! log with the sync protocol. The joiner then writes its `DeviceBound` and `DeviceProfile` and
//! pushes them with the sync protocol again, on the same stream.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use iroh::Signature;
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};
use serde::{Deserialize, Serialize};
use wf_core::id::{EventId, WorkforceId};
use wf_core::identity::DeviceKey;
use wf_core::invite::JoinRefusal;

use crate::frame::{read_frame, write_frame};
use crate::log::SyncLog;
use crate::node::{FAILED, Registry, SourceError, device_key};
use crate::sync::{IDLE_TIMEOUT, initiate, read_open, respond};

pub const JOIN_ALPN: &[u8] = b"wf/join/1";

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

/// The device each invite has been served to. The first device that is let in is the only one
/// that can use the invite, even if its join does not finish and it has to ask again; the invite
/// is then good until it is redeemed or expires. A restart forgets it.
#[derive(Clone, Default)]
pub(crate) struct Reservations(Arc<Mutex<BTreeMap<EventId, DeviceKey>>>);

impl Reservations {
    /// Whether `device` may use `invite`: nobody else has been served with it.
    fn reserve(&self, invite: EventId, device: DeviceKey) -> bool {
        let mut held = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *held.entry(invite).or_insert(device) == device
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
    async fn decide(&self, request: &JoinRequest, joiner: DeviceKey) -> Result<L, JoinRefusal> {
        let log = self
            .registry
            .get(&request.workforce)
            .ok_or(JoinRefusal::NotServed)?;
        log.check_join(request.invite, joiner, request.proof, now_ms())
            .await
            .map_err(|_| JoinRefusal::NotServed)??;
        if !self.reservations.reserve(request.invite, joiner) {
            return Err(JoinRefusal::Reserved);
        }
        Ok(log)
    }
}

impl<L: SyncLog + Clone + 'static> ProtocolHandler for JoinHandler<L> {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let joiner = device_key(connection.remote_id());
        let (mut send, mut recv) = connection
            .accept_bi()
            .await
            .map_err(AcceptError::from_err)?;
        let request: JoinRequest = tokio::time::timeout(IDLE_TIMEOUT, read_frame(&mut recv))
            .await
            .map_err(AcceptError::from_err)?
            .map_err(AcceptError::from_err)?;
        let log = match self.decide(&request, joiner).await {
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
pub(crate) async fn run_join<S, L, E, Fut>(
    stream: &mut S,
    request: JoinRequest,
    log: &L,
    author: impl FnOnce() -> Fut,
) -> Result<(), JoinFailure<E>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    L: SyncLog,
    Fut: std::future::Future<Output = Result<(), E>>,
{
    let workforce = request.workforce;
    write_frame(stream, &request)
        .await
        .map_err(|error| JoinFailure::Net(error.into()))?;
    match read_frame(stream)
        .await
        .map_err(|error| JoinFailure::Net(error.into()))?
    {
        JoinReply::Accepted => {}
        JoinReply::Refused(refusal) => return Err(JoinFailure::Refused(refusal)),
    }
    initiate(&mut *stream, workforce, log)
        .await
        .map_err(|error| JoinFailure::Net(error.into()))?;
    author().await.map_err(JoinFailure::Author)?;
    initiate(&mut *stream, workforce, log)
        .await
        .map_err(|error| JoinFailure::Net(error.into()))?;
    Ok(())
}

pub(crate) enum JoinFailure<E> {
    Net(SourceError),
    Refused(JoinRefusal),
    Author(E),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_invite_is_served_to_the_first_device_only() {
        let reservations = Reservations::default();
        let invite = EventId::of(b"invite");
        let device = |seed: u8| {
            wf_core::identity::DeviceSigner::from_bytes(&[seed; 32])
                .unwrap()
                .key()
        };
        let (first, second) = (device(1), device(2));
        assert!(reservations.reserve(invite, first));
        assert!(reservations.reserve(invite, first), "it may ask again");
        assert!(!reservations.reserve(invite, second));
        assert!(reservations.reserve(EventId::of(b"other"), second));
    }
}

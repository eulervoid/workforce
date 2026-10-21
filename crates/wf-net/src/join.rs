//! `wf/join/2`: a new device redeems an invite with a member that serves the workforce.
//!
//! The connection authenticates the joiner's device key. The member checks the proof, reserves
//! the invite so a second device cannot use it while this join is in progress, and streams the
//! log with the sync protocol. The joiner then writes its `DeviceBound` and `DeviceProfile` and
//! pushes them with the sync protocol again, on the same stream.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iroh::Signature;
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};
use serde::{Deserialize, Serialize};
use wf_core::id::{EventId, WorkforceId};
use wf_core::identity::DeviceKey;
use wf_core::invite::JoinRefusal;

use crate::frame::{read_frame_within, write_frame_within};
use crate::log::SyncLog;
use crate::node::{FAILED, Registry, SourceError, device_key, linger};
use crate::sync::{IDLE_TIMEOUT, SyncError, initiate_within, read_open, respond};

/// The request is about 150 bytes and comes from a device that is not a member yet.
const REQUEST_FRAME_LEN: u32 = 1024;
const REPLY_FRAME_LEN: u32 = 256;

pub const JOIN_ALPN: &[u8] = b"wf/join/2";

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
/// that can use the invite, even if its join does not finish and it has to ask again. The
/// reservation ends when that join finishes or the log refuses the invite as redeemed or expired,
/// since the log then decides who may use it. A restart forgets it.
#[derive(Clone, Default)]
pub(crate) struct Reservations(Arc<Mutex<BTreeMap<EventId, DeviceKey>>>);

impl Reservations {
    /// Whether `device` may use `invite`: nobody else has been served with it.
    fn reserve(&self, invite: EventId, device: DeviceKey) -> bool {
        *self.held().entry(invite).or_insert(device) == device
    }

    fn release(&self, invite: EventId) {
        self.held().remove(&invite);
    }

    fn held(&self) -> std::sync::MutexGuard<'_, BTreeMap<EventId, DeviceKey>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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

impl<L: SyncLog + Clone + 'static> JoinHandler<L> {
    async fn decide(&self, request: &JoinRequest, joiner: DeviceKey) -> Result<L, JoinRefusal> {
        let log = self
            .registry
            .get(&request.workforce)
            .ok_or(JoinRefusal::NotServed)?;
        let checked = log
            .check_join(request.invite, joiner, request.proof)
            .await
            .map_err(|_| JoinRefusal::Unreadable)?;
        if let Err(JoinRefusal::Redeemed | JoinRefusal::Expired) = checked {
            self.reservations.release(request.invite);
        }
        checked?;
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
        let request: JoinRequest = read_frame_within(&mut recv, REQUEST_FRAME_LEN, IDLE_TIMEOUT)
            .await
            .map_err(AcceptError::from_err)?;
        let log = match self.decide(&request, joiner).await {
            Ok(accepted) => accepted,
            Err(refusal) => {
                write_frame_within(&mut send, &JoinReply::Refused(refusal), IDLE_TIMEOUT)
                    .await
                    .map_err(AcceptError::from_err)?;
                let _ = send.finish();
                linger(&connection).await;
                return Ok(());
            }
        };
        write_frame_within(&mut send, &JoinReply::Accepted, IDLE_TIMEOUT)
            .await
            .map_err(AcceptError::from_err)?;
        // The log goes out first, then the joiner's own events come in.
        for _ in 0..2 {
            let result = async {
                let opened = read_open(&mut recv, IDLE_TIMEOUT).await?;
                if opened != request.workforce {
                    return Err(SyncError::OtherWorkforce);
                }
                respond(tokio::io::join(&mut recv, &mut send), &log).await
            }
            .await;
            if let Err(error) = result {
                connection.close(FAILED.into(), b"join failed");
                return Err(AcceptError::from_err(error));
            }
        }
        // The joiner's events redeemed the invite.
        self.reservations.release(request.invite);
        let _ = send.finish();
        linger(&connection).await;
        Ok(())
    }
}

/// The joiner's half, on a stream the member accepted. The member is given up on when it is
/// silent for `idle`.
pub(crate) async fn run_join<S, L, E, Fut>(
    stream: &mut S,
    request: JoinRequest,
    log: &L,
    idle: Duration,
    author: impl FnOnce() -> Fut,
) -> Result<(), JoinFailure<E>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    L: SyncLog,
    Fut: std::future::Future<Output = Result<(), E>>,
{
    let workforce = request.workforce;
    write_frame_within(stream, &request, idle)
        .await
        .map_err(|error| JoinFailure::Net(error.into()))?;
    match read_frame_within(stream, REPLY_FRAME_LEN, idle)
        .await
        .map_err(|error| JoinFailure::Net(error.into()))?
    {
        JoinReply::Accepted => {}
        JoinReply::Refused(refusal) => return Err(JoinFailure::Refused(refusal)),
    }
    initiate_within(&mut *stream, workforce, log, idle)
        .await
        .map_err(|error| JoinFailure::Net(error.into()))?;
    author().await.map_err(JoinFailure::Author)?;
    initiate_within(&mut *stream, workforce, log, idle)
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
    use crate::frame::FrameError;
    use crate::log::LogError;
    use wf_core::event::Event;
    use wf_core::identity::Signature;
    use wf_core::sync::{BatchReceived, Versions};

    /// A log for runs that end before the log is asked for anything.
    struct UntouchedLog;

    impl SyncLog for UntouchedLog {
        async fn versions(&self) -> Result<Versions, LogError> {
            unimplemented!()
        }
        async fn events_after(&self, _: &Versions) -> Result<Vec<Event>, LogError> {
            unimplemented!()
        }
        async fn events_by_id(&self, _: &[EventId]) -> Result<Vec<Event>, LogError> {
            unimplemented!()
        }
        async fn receive(&self, _: Vec<Event>) -> Result<BatchReceived, LogError> {
            unimplemented!()
        }
        async fn is_bound(&self, _: DeviceKey) -> Result<bool, LogError> {
            unimplemented!()
        }
        async fn check_join(
            &self,
            _: EventId,
            _: DeviceKey,
            _: Signature,
        ) -> Result<Result<(), JoinRefusal>, LogError> {
            unimplemented!()
        }
        async fn fill_history(&self) -> Result<(), LogError> {
            unimplemented!()
        }
        async fn missing_parents(&self) -> Result<Vec<EventId>, LogError> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn a_member_that_never_replies_to_the_request_is_given_up_on() {
        let request = JoinRequest {
            workforce: WorkforceId::from(EventId::of(b"workforce")),
            invite: EventId::of(b"invite"),
            proof: Signature::from_bytes(&[0; 64]),
        };
        let (mut joiner, _silent_member) = tokio::io::duplex(8192);
        let result = run_join(
            &mut joiner,
            request,
            &UntouchedLog,
            Duration::from_millis(100),
            || std::future::ready(Ok::<(), std::convert::Infallible>(())),
        )
        .await;
        assert!(matches!(
            result,
            Err(JoinFailure::Net(error)) if matches!(
                error.downcast_ref::<FrameError>(),
                Some(FrameError::TimedOut)
            )
        ));
    }

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
        reservations.release(invite);
        assert!(
            reservations.reserve(invite, second),
            "a released invite is free"
        );
    }
}

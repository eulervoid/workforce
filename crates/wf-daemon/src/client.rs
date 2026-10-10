//! The CLI's side of the socket. Blocking, because the CLI is.

use std::io::ErrorKind;
use std::os::unix::net::UnixStream;
use std::path::Path;

use wf_core::event::{Event, EventBody};
use wf_core::id::{EventId, WorkforceId};
use wf_core::identity::{Actor, DeviceKey};
use wf_core::state::State;

use crate::peers::PeerSync;
use crate::proto::{
    Call, Hello, HelloReply, PROTOCOL_VERSION, ProtoError, Request, Response, read_message,
    write_message,
};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error(transparent)]
    Proto(#[from] ProtoError),
    #[error("the daemon refused: {0}")]
    Failed(String),
    #[error("the daemon answered with the wrong kind of message")]
    UnexpectedResponse,
}

pub struct DaemonClient {
    stream: UnixStream,
}

impl DaemonClient {
    /// `Ok(None)` when no daemon is listening.
    pub fn connect(socket: &Path) -> Result<Option<Self>, ClientError> {
        let stream = match UnixStream::connect(socket) {
            Ok(stream) => stream,
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::NotFound | ErrorKind::ConnectionRefused
                ) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(ProtoError::from(error).into()),
        };
        let mut client = Self { stream };
        write_message(
            &mut client.stream,
            &Hello {
                version: PROTOCOL_VERSION,
            },
        )?;
        match read_message(&mut client.stream)? {
            HelloReply::Ready { .. } => Ok(Some(client)),
            HelloReply::Refused { version } => Err(ProtoError::VersionMismatch {
                daemon: version,
                client: PROTOCOL_VERSION,
            }
            .into()),
        }
    }

    fn call(&mut self, workforce: WorkforceId, call: Call) -> Result<Response, ClientError> {
        write_message(&mut self.stream, &Request { workforce, call })?;
        match read_message(&mut self.stream)? {
            Response::Failed(message) => Err(ClientError::Failed(message)),
            response => Ok(response),
        }
    }

    pub fn state(&mut self, workforce: WorkforceId) -> Result<State, ClientError> {
        match self.call(workforce, Call::State)? {
            Response::State(bytes) => {
                wf_core::encoding::decode(&bytes).map_err(|error| ProtoError::from(error).into())
            }
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub fn event(
        &mut self,
        workforce: WorkforceId,
        id: EventId,
    ) -> Result<Option<Event>, ClientError> {
        match self.call(workforce, Call::Event(id))? {
            Response::Event(event) => Ok(event),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub fn events(&mut self, workforce: WorkforceId) -> Result<Vec<Event>, ClientError> {
        match self.call(workforce, Call::Events)? {
            Response::Events(events) => Ok(events),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub fn author(
        &mut self,
        workforce: WorkforceId,
        actor: Actor,
        body: EventBody,
    ) -> Result<Event, ClientError> {
        match self.call(
            workforce,
            Call::Author {
                actor,
                body: Box::new(body),
            },
        )? {
            Response::Authored(event) => Ok(event),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub fn sync_now(&mut self, workforce: WorkforceId) -> Result<Vec<PeerSync>, ClientError> {
        match self.call(workforce, Call::SyncNow)? {
            Response::Synced(peers) => Ok(peers),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub fn sync_with(
        &mut self,
        workforce: WorkforceId,
        devices: Vec<DeviceKey>,
    ) -> Result<Vec<PeerSync>, ClientError> {
        match self.call(workforce, Call::SyncWith(devices))? {
            Response::Synced(peers) => Ok(peers),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub fn last_seen(
        &mut self,
        workforce: WorkforceId,
    ) -> Result<Vec<(DeviceKey, u64)>, ClientError> {
        match self.call(workforce, Call::LastSeen)? {
            Response::LastSeen(seen) => Ok(seen),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }
}

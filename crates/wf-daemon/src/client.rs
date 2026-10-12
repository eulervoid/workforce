//! The CLI's side of the socket. Blocking, because the CLI is.

use std::io::ErrorKind;
use std::os::unix::net::UnixStream;
use std::path::Path;

use wf_core::event::{DeviceProfile, Event, EventBody};
use wf_core::id::{EventId, ItemId, SessionId, WorkforceId};
use wf_core::identity::{Actor, DeviceKey};
use wf_core::invite::InviteCode;
use wf_core::name::AgentName;
use wf_core::state::State;

use crate::peers::PeerSync;
use crate::proto::{
    Answer, Call, Finish, HeldPermission, Hello, HelloReply, PROTOCOL_VERSION, ProtoError, Request,
    Response, read_message, write_message,
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
            // A path longer than a socket address holds cannot be listened on either.
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::NotFound | ErrorKind::ConnectionRefused | ErrorKind::InvalidInput
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

    /// Every event, in the order the daemon applied them.
    pub fn events(&mut self, workforce: WorkforceId) -> Result<Vec<Event>, ClientError> {
        self.events_since(workforce, 0)
    }

    /// The events from position `from` of the log on, in the order the daemon applied them.
    pub fn events_since(
        &mut self,
        workforce: WorkforceId,
        from: u64,
    ) -> Result<Vec<Event>, ClientError> {
        let mut events = Vec::new();
        loop {
            let position = from + events.len() as u64;
            match self.call(workforce, Call::Events { from: position })? {
                Response::Events(page) if page.is_empty() => return Ok(events),
                Response::Events(page) => events.extend(page),
                _ => return Err(ClientError::UnexpectedResponse),
            }
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

    /// Has the daemon found the fork of `parent` that `genesis` starts.
    pub fn fork(&mut self, parent: WorkforceId, genesis: Event) -> Result<(), ClientError> {
        match self.call(parent, Call::Fork(Box::new(genesis)))? {
            Response::Forked => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Has the daemon join the workforce of `code` as this machine's device.
    pub fn join(
        &mut self,
        workforce: WorkforceId,
        code: &InviteCode,
        profile: DeviceProfile,
    ) -> Result<(), ClientError> {
        let call = Call::Join {
            code: code.to_string(),
            profile,
        };
        match self.call(workforce, call)? {
            Response::Joined => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Has the daemon take in `workforce` from `devices`.
    pub fn follow(
        &mut self,
        workforce: WorkforceId,
        devices: Vec<DeviceKey>,
    ) -> Result<Vec<PeerSync>, ClientError> {
        match self.call(workforce, Call::Follow(devices))? {
            Response::Synced(peers) => Ok(peers),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Has the daemon start a work session of `agent` on `item`.
    pub fn start_session(
        &mut self,
        workforce: WorkforceId,
        agent: AgentName,
        item: ItemId,
        repository: &Path,
    ) -> Result<SessionId, ClientError> {
        let call = Call::StartSession {
            agent,
            item,
            repository: repository.to_path_buf(),
        };
        match self.call(workforce, call)? {
            Response::SessionStarted(session) => Ok(session),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Has the daemon end a session of this machine.
    pub fn end_session(
        &mut self,
        workforce: WorkforceId,
        session: SessionId,
        finish: Finish,
    ) -> Result<(), ClientError> {
        match self.call(workforce, Call::EndSession { session, finish })? {
            Response::SessionEnded => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// The permission requests that wait for the principal.
    pub fn permissions(
        &mut self,
        workforce: WorkforceId,
    ) -> Result<Vec<HeldPermission>, ClientError> {
        match self.call(workforce, Call::Permissions)? {
            Response::Permissions(held) => Ok(held),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Answers the oldest request of a session, and says which it was.
    pub fn answer_permission(
        &mut self,
        workforce: WorkforceId,
        session: SessionId,
        answer: Answer,
    ) -> Result<HeldPermission, ClientError> {
        match self.call(workforce, Call::AnswerPermission { session, answer })? {
            Response::PermissionAnswered(held) => Ok(held),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_daemon_listens_on_a_socket_path_too_long_for_one() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("d".repeat(200)).join("daemon.sock");
        assert!(matches!(DaemonClient::connect(&socket), Ok(None)));
    }
}

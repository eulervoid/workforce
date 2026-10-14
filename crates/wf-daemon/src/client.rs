//! The CLI's side of the socket. Blocking, because the CLI is.

use std::io::ErrorKind;
use std::os::unix::net::UnixStream;
use std::path::Path;

use serde::de::DeserializeOwned;
use wf_core::event::{DeviceProfile, Event, EventBody};
use wf_core::id::{EventId, ItemId, SessionId, WorkforceId};
use wf_core::identity::{Actor, DeviceKey};
use wf_core::invite::InviteCode;
use wf_core::lifecycle::Finish;
use wf_core::name::AgentName;
use wf_core::state::State;
use wf_harness::Permission;

use crate::peers::PeerSync;
use crate::proto::{
    Call, HeldPermission, Hello, HelloReply, PROTOCOL_VERSION, ProtoError, Reply, Request,
    read_message, write_message,
};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error(transparent)]
    Proto(#[from] ProtoError),
    #[error("the daemon refused: {0}")]
    Failed(String),
}

pub struct DaemonClient {
    stream: UnixStream,
    device: DeviceKey,
}

impl DaemonClient {
    /// `Ok(None)` when no daemon is listening.
    pub fn connect(socket: &Path) -> Result<Option<Self>, ClientError> {
        let mut stream = match UnixStream::connect(socket) {
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
        write_message(
            &mut stream,
            &Hello {
                version: PROTOCOL_VERSION,
            },
        )?;
        match read_message(&mut stream)? {
            HelloReply::Ready { device, .. } => Ok(Some(Self { stream, device })),
            HelloReply::Refused { version } => Err(ProtoError::VersionMismatch {
                daemon: version,
                client: PROTOCOL_VERSION,
            }
            .into()),
        }
    }

    /// The device the daemon signs as: this machine's.
    pub fn device(&self) -> DeviceKey {
        self.device
    }

    /// Makes `call` and reads its [`Reply`] as the type the call is answered with.
    fn call<T: DeserializeOwned>(
        &mut self,
        workforce: WorkforceId,
        call: Call,
    ) -> Result<T, ClientError> {
        write_message(&mut self.stream, &Request { workforce, call })?;
        let reply: Reply<T> = read_message(&mut self.stream)?;
        reply.map_err(ClientError::Failed)
    }

    pub fn state(&mut self, workforce: WorkforceId) -> Result<State, ClientError> {
        let bytes: Vec<u8> = self.call(workforce, Call::State)?;
        wf_core::encoding::decode(&bytes).map_err(|error| ProtoError::from(error).into())
    }

    pub fn event(
        &mut self,
        workforce: WorkforceId,
        id: EventId,
    ) -> Result<Option<Event>, ClientError> {
        self.call(workforce, Call::Event(id))
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
            let page: Vec<Event> = self.call(workforce, Call::Events { from: position })?;
            if page.is_empty() {
                return Ok(events);
            }
            events.extend(page);
        }
    }

    pub fn author(
        &mut self,
        workforce: WorkforceId,
        actor: Actor,
        body: EventBody,
    ) -> Result<Event, ClientError> {
        self.call(
            workforce,
            Call::Author {
                actor,
                body: Box::new(body),
            },
        )
    }

    pub fn sync_now(&mut self, workforce: WorkforceId) -> Result<Vec<PeerSync>, ClientError> {
        self.call(workforce, Call::SyncNow)
    }

    /// Has the daemon found the fork of `parent` that `genesis` starts.
    pub fn fork(&mut self, parent: WorkforceId, genesis: Event) -> Result<(), ClientError> {
        self.call(parent, Call::Fork(Box::new(genesis)))
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
        self.call(workforce, call)
    }

    /// Has the daemon take in `workforce` from `devices`.
    pub fn follow(
        &mut self,
        workforce: WorkforceId,
        devices: Vec<DeviceKey>,
    ) -> Result<Vec<PeerSync>, ClientError> {
        self.call(workforce, Call::Follow(devices))
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
        self.call(workforce, call)
    }

    /// Has the daemon end a session of this machine.
    pub fn end_session(
        &mut self,
        workforce: WorkforceId,
        session: SessionId,
        finish: Finish,
    ) -> Result<(), ClientError> {
        self.call(workforce, Call::EndSession { session, finish })
    }

    /// Has the daemon stop a session of this machine now.
    pub fn stop_session(
        &mut self,
        workforce: WorkforceId,
        session: SessionId,
    ) -> Result<(), ClientError> {
        self.call(workforce, Call::StopSession(session))
    }

    /// The permission requests that wait for the principal.
    pub fn permissions(
        &mut self,
        workforce: WorkforceId,
    ) -> Result<Vec<HeldPermission>, ClientError> {
        self.call(workforce, Call::Permissions)
    }

    /// Answers the oldest request of a session, and says which it was.
    pub fn answer_permission(
        &mut self,
        workforce: WorkforceId,
        session: SessionId,
        answer: Permission,
    ) -> Result<HeldPermission, ClientError> {
        self.call(workforce, Call::AnswerPermission { session, answer })
    }

    pub fn last_seen(
        &mut self,
        workforce: WorkforceId,
    ) -> Result<Vec<(DeviceKey, u64)>, ClientError> {
        self.call(workforce, Call::LastSeen)
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

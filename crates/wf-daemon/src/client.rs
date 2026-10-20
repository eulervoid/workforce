//! The CLI's side of the socket. Blocking, because the CLI is.

use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use serde::de::DeserializeOwned;
use wf_core::event::{DeviceProfile, Event, EventBody};
use wf_core::id::{EventId, ItemId, SessionId, WorkforceId};
use wf_core::identity::{AgentId, DeviceKey};
use wf_core::invite::InviteCode;
use wf_core::lifecycle::Finish;
use wf_core::name::{AgentName, ReviewStepName};
use wf_core::state::State;
use wf_harness::Permission;

use crate::caller::{Caller, Token};
use crate::peers::PeerSync;
use crate::proto::{
    Call, Credential, EndAnswer, HeldPermission, Hello, HelloReply, PROTOCOL_VERSION, ProtoError,
    Reply, Request, read_message, write_message,
};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error(transparent)]
    Proto(#[from] ProtoError),
    #[error("the daemon refused: {0}")]
    Failed(String),
    #[error(
        "the daemon does not know whom this connection acts for: a session presents its token, \
         the principal the daemon's principal token"
    )]
    Unrecognized,
}

pub struct DaemonClient {
    stream: UnixStream,
    pid: u32,
    device: DeviceKey,
    caller: Caller,
}

impl DaemonClient {
    /// Connects as whom `token` makes the connection act for. `Ok(None)` when no daemon is
    /// listening.
    pub fn connect(socket: &Path, token: Option<Token>) -> Result<Option<Self>, ClientError> {
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
        match handshake(&mut stream, token)? {
            HelloReply::Ready {
                pid,
                device,
                caller,
                ..
            } => Ok(Some(Self {
                stream,
                pid,
                device,
                caller,
            })),
            HelloReply::Unrecognized => Err(ClientError::Unrecognized),
            HelloReply::Refused { version } => Err(ProtoError::VersionMismatch {
                daemon: version,
                client: PROTOCOL_VERSION,
            }
            .into()),
        }
    }

    /// The process id of the daemon, as it said itself over the socket.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// The device the daemon signs as: this machine's.
    pub fn device(&self) -> DeviceKey {
        self.device
    }

    /// Whom the daemon acts for on this connection.
    pub fn caller(&self) -> Caller {
        self.caller
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

    /// The events that can show in the activity of `item`, in the order the daemon applied them.
    pub fn item_events(
        &mut self,
        workforce: WorkforceId,
        item: ItemId,
    ) -> Result<Vec<Event>, ClientError> {
        self.call(workforce, Call::ItemEvents(item))
    }

    /// The events that defined or removed `agent`, in the order the daemon applied them.
    pub fn agent_events(
        &mut self,
        workforce: WorkforceId,
        agent: AgentId,
    ) -> Result<Vec<Event>, ClientError> {
        self.call(workforce, Call::AgentEvents(agent))
    }

    /// Has the daemon author `body` as whom the connection acts for.
    pub fn author(
        &mut self,
        workforce: WorkforceId,
        body: EventBody,
    ) -> Result<Event, ClientError> {
        let body = Box::new(body);
        self.call(workforce, Call::Author { body })
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

    /// Has the daemon start a session of `agent` on `item`: a review of `step` when the item
    /// awaits one, otherwise work.
    pub fn start_session(
        &mut self,
        workforce: WorkforceId,
        agent: AgentName,
        item: ItemId,
        step: Option<ReviewStepName>,
    ) -> Result<SessionId, ClientError> {
        let call = Call::StartSession { agent, item, step };
        self.call(workforce, call)
    }

    /// Has the daemon end the session the connection acts for.
    pub fn end_session(
        &mut self,
        workforce: WorkforceId,
        finish: Finish,
    ) -> Result<EndAnswer, ClientError> {
        self.call(workforce, Call::EndSession { finish })
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

    /// What the caches in the worktrees took when the daemon last checked them against its budget;
    /// `None` when the machine config sets none.
    pub fn cache_usage(
        &mut self,
        workforce: WorkforceId,
    ) -> Result<Option<crate::caches::CacheUsage>, ClientError> {
        self.call(workforce, Call::CacheUsage)
    }

    /// The work of an item as the device of its live work session shows it. `since` is the head
    /// of the answer the caller last had.
    pub fn live_diff(
        &mut self,
        workforce: WorkforceId,
        item: ItemId,
        since: Option<wf_core::session::CommitId>,
    ) -> Result<crate::proto::LiveDiff, ClientError> {
        self.call(workforce, Call::LiveDiff { item, since })
    }

    /// The commit the target branch is at.
    pub fn target_commit(
        &mut self,
        workforce: WorkforceId,
    ) -> Result<wf_core::session::CommitId, ClientError> {
        self.call(workforce, Call::TargetCommit)
    }

    /// The commits the log names that this device has not got yet.
    pub fn awaited_commits(
        &mut self,
        workforce: WorkforceId,
    ) -> Result<Vec<wf_core::commits::NamedCommit>, ClientError> {
        self.call(workforce, Call::AwaitedCommits)
    }

    /// What is wrong with the machine the daemon runs on.
    pub fn machine_faults(
        &mut self,
        workforce: WorkforceId,
    ) -> Result<crate::MachineFaults, ClientError> {
        self.call(workforce, Call::MachineFaults)
    }

    /// Answers the oldest request of a session, if it is the one held since `since_ms`, and says
    /// which it was.
    pub fn answer_permission(
        &mut self,
        workforce: WorkforceId,
        session: SessionId,
        since_ms: u64,
        answer: Permission,
    ) -> Result<HeldPermission, ClientError> {
        self.call(
            workforce,
            Call::AnswerPermission {
                session,
                since_ms,
                answer,
            },
        )
    }

    pub fn last_seen(
        &mut self,
        workforce: WorkforceId,
    ) -> Result<Vec<(DeviceKey, u64)>, ClientError> {
        self.call(workforce, Call::LastSeen)
    }
}

/// Presents `token` and reads the daemon's answer. A daemon of another version answers the Hello
/// and closes without reading on, so writing the Credential can fail while the refusal already
/// waits to be read: a failed write only counts when there is no answer.
fn handshake<S: Read + Write>(
    stream: &mut S,
    token: Option<Token>,
) -> Result<HelloReply, ProtoError> {
    let written = write_message(
        stream,
        &Hello {
            version: PROTOCOL_VERSION,
        },
    )
    .and_then(|()| write_message(stream, &Credential { token }));
    match (read_message(stream), written) {
        (Ok(reply), _) => Ok(reply),
        (Err(_), Err(error)) | (Err(error), Ok(())) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    /// A daemon of another version: it has refused the Hello and closed, so a write after the
    /// Hello fails as on a closed socket, while its refusal waits to be read.
    struct ClosedAfterHello {
        refusal: Cursor<Vec<u8>>,
        hello_written: bool,
    }

    impl Read for ClosedAfterHello {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            self.refusal.read(buffer)
        }
    }

    impl Write for ClosedAfterHello {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.hello_written {
                return Err(ErrorKind::BrokenPipe.into());
            }
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.hello_written = true;
            Ok(())
        }
    }

    #[test]
    fn a_daemon_that_refuses_and_closes_before_the_credential_is_still_a_refusal() {
        let mut refusal = Vec::new();
        write_message(
            &mut refusal,
            &HelloReply::Refused {
                version: PROTOCOL_VERSION + 1,
            },
        )
        .unwrap();
        let mut daemon = ClosedAfterHello {
            refusal: Cursor::new(refusal),
            hello_written: false,
        };
        assert!(matches!(
            handshake(&mut daemon, None),
            Ok(HelloReply::Refused { .. })
        ));
    }

    #[test]
    fn no_daemon_listens_on_a_socket_path_too_long_for_one() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("d".repeat(200)).join("daemon.sock");
        assert!(matches!(DaemonClient::connect(&socket, None), Ok(None)));
    }
}

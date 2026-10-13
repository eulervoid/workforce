//! Answers the CLI on the unix socket.

use std::sync::Arc;

use tokio::net::UnixStream;
use wf_core::id::WorkforceId;
use wf_core::invite::{InviteCode, InviteCodeError};
use wf_core::lifecycle::{Interruption, SessionEnd};
use wf_net::frame::{FrameError, read_frame, write_frame};

use crate::peers::{peers_of, sync_all};
use crate::proto::{Call, EVENTS_PER_PAGE, Hello, HelloReply, PROTOCOL_VERSION, Request, Response};
use crate::{Daemon, Loaded, now_ms};

pub(crate) async fn handle(daemon: Arc<Daemon>, mut stream: UnixStream) {
    let _ = converse(&daemon, &mut stream).await;
}

async fn converse(daemon: &Arc<Daemon>, stream: &mut UnixStream) -> Result<(), FrameError> {
    let hello: Hello = read_frame(stream).await?;
    if hello.version != PROTOCOL_VERSION {
        return write_frame(
            stream,
            &HelloReply::Refused {
                version: PROTOCOL_VERSION,
            },
        )
        .await;
    }
    write_frame(
        stream,
        &HelloReply::Ready {
            version: PROTOCOL_VERSION,
        },
    )
    .await?;
    loop {
        let request: Request = match read_frame(stream).await {
            Ok(request) => request,
            Err(FrameError::Closed) => return Ok(()),
            Err(error) => return Err(error),
        };
        let response = answer(daemon, request).await;
        match write_frame(stream, &response).await {
            Err(FrameError::TooLarge(size)) => {
                // Nothing of the answer was written, so the connection is still in step.
                let message = format!(
                    "the answer is {size} bytes, more than fits in one message on the socket"
                );
                write_frame(stream, &Response::Failed(message)).await?;
            }
            other => other?,
        }
    }
}

async fn answer(daemon: &Arc<Daemon>, request: Request) -> Response {
    let opened = match request.call {
        Call::Follow(_) | Call::Join { .. } => daemon.follow(request.workforce),
        Call::State
        | Call::Event(_)
        | Call::Events { .. }
        | Call::Author { .. }
        | Call::SyncNow
        | Call::Fork(_)
        | Call::StartSession { .. }
        | Call::EndSession { .. }
        | Call::StopSession(_)
        | Call::Permissions
        | Call::AnswerPermission { .. }
        | Call::LastSeen => daemon.workforce(request.workforce),
    };
    let loaded = match opened {
        Ok(loaded) => loaded,
        Err(error) => return Response::Failed(wf_core::report::report(&error)),
    };
    match call(daemon, request.workforce, &loaded, request.call).await {
        Ok(response) => response,
        Err(message) => Response::Failed(message),
    }
}

async fn call(
    daemon: &Arc<Daemon>,
    workforce: WorkforceId,
    loaded: &Arc<Loaded>,
    call: Call,
) -> Result<Response, String> {
    let storage = &loaded.storage;
    let closed = |error: crate::StorageClosed| wf_core::report::report(&error);
    match call {
        Call::State => {
            let bytes = storage
                .with(|replica| wf_core::encoding::encode(replica.state()))
                .await
                .map_err(closed)?
                .map_err(|error| wf_core::report::report(&error))?;
            Ok(Response::State(bytes))
        }
        Call::Event(id) => {
            let event = storage
                .with(move |replica| wf_store::EventStore::event(replica.store(), &id))
                .await
                .map_err(closed)?
                .map_err(|error| wf_core::report::report(&error))?;
            Ok(Response::Event(event))
        }
        Call::Events { from } => {
            let events = storage
                .with(move |replica| {
                    wf_store::EventStore::log_page(replica.store(), from, EVENTS_PER_PAGE)
                })
                .await
                .map_err(closed)?
                .map_err(|error| wf_core::report::report(&error))?;
            Ok(Response::Events(events))
        }
        Call::Author { actor, body } => {
            let signer = daemon.signer().clone();
            let now_ms = now_ms();
            let event = storage
                .with(move |replica| replica.author(&signer, actor, now_ms, *body))
                .await
                .map_err(closed)?
                .map_err(|error| wf_core::report::report(&error))?;
            Ok(Response::Authored(event))
        }
        Call::SyncNow => {
            let own = daemon.device();
            let peers = storage
                .with(move |replica| {
                    peers_of(replica.state(), own)
                        .into_iter()
                        .map(|device| (device, device.fmt_short()))
                        .collect::<Vec<_>>()
                })
                .await
                .map_err(closed)?;
            if peers.is_empty() {
                return Ok(Response::Synced(Vec::new()));
            }
            let node = daemon
                .node()
                .await
                .map_err(|error| wf_core::report::report(&error))?;
            let results = sync_all(&node, workforce, peers, Some(daemon.presence())).await;
            Ok(Response::Synced(results))
        }
        Call::Fork(genesis) => {
            daemon
                .create_fork(loaded, *genesis)
                .await
                .map_err(|error| wf_core::report::report(&error))?;
            Ok(Response::Forked)
        }
        Call::Join { code, profile } => {
            let code: InviteCode = code
                .parse()
                .map_err(|error: InviteCodeError| wf_core::report::report(&error))?;
            if code.workforce != workforce {
                return Err("the invite is for another workforce".to_string());
            }
            daemon
                .join(loaded, &code, profile)
                .await
                .map_err(|error| wf_core::report::report(&error))?;
            Ok(Response::Joined)
        }
        Call::Follow(devices) => {
            let peers = devices
                .into_iter()
                .map(|device| (device, device.fmt_short()))
                .collect();
            let node = daemon
                .node()
                .await
                .map_err(|error| wf_core::report::report(&error))?;
            let results = sync_all(&node, workforce, peers, Some(daemon.presence())).await;
            Ok(Response::Synced(results))
        }
        Call::LastSeen => Ok(Response::LastSeen(daemon.presence().snapshot())),
        Call::StartSession {
            agent,
            item,
            repository,
        } => {
            let session = daemon
                .start_session(workforce, loaded, agent, item, repository)
                .await
                .map_err(|error| wf_core::report::report(&error))?;
            Ok(Response::SessionStarted(session))
        }
        Call::Permissions => Ok(Response::Permissions(daemon.held.list(workforce))),
        Call::AnswerPermission { session, answer } => daemon
            .held
            .answer(session, answer)
            .map(Response::PermissionAnswered)
            .ok_or_else(|| "that session is not waiting for an answer".to_string()),
        Call::EndSession { session, finish } => {
            daemon
                .finish_session(workforce, loaded, session, SessionEnd::Finish(finish))
                .await
                .map_err(|error| wf_core::report::report(&error))?;
            Ok(Response::SessionEnded)
        }
        Call::StopSession(session) => {
            daemon
                .finish_session(
                    workforce,
                    loaded,
                    session,
                    SessionEnd::Interruption(Interruption::Stop),
                )
                .await
                .map_err(|error| wf_core::report::report(&error))?;
            Ok(Response::SessionEnded)
        }
    }
}

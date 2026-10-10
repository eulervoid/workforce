//! Answers the CLI on the unix socket.

use std::sync::Arc;

use tokio::net::UnixStream;
use wf_core::id::WorkforceId;
use wf_net::frame::{FrameError, read_frame, write_frame};

use crate::peers::{peers_of, sync_all};
use crate::proto::{Call, Hello, HelloReply, PROTOCOL_VERSION, Request, Response};
use crate::{Daemon, Loaded};

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
        write_frame(stream, &response).await?;
    }
}

async fn answer(daemon: &Arc<Daemon>, request: Request) -> Response {
    let loaded = match daemon.workforce(request.workforce) {
        Ok(loaded) => loaded,
        Err(error) => return Response::Failed(error.to_string()),
    };
    match call(daemon, request.workforce, &loaded, request.call).await {
        Ok(response) => response,
        Err(message) => Response::Failed(message),
    }
}

async fn call(
    daemon: &Arc<Daemon>,
    workforce: WorkforceId,
    loaded: &Loaded,
    call: Call,
) -> Result<Response, String> {
    let storage = &loaded.storage;
    let closed = |error: crate::StorageClosed| error.to_string();
    match call {
        Call::State => {
            let bytes = storage
                .with(|replica| wf_core::encoding::encode(replica.state()))
                .await
                .map_err(closed)?
                .map_err(|error| error.to_string())?;
            Ok(Response::State(bytes))
        }
        Call::Event(id) => {
            let event = storage
                .with(move |replica| wf_store::EventStore::event(replica.store(), &id))
                .await
                .map_err(closed)?
                .map_err(|error| error.to_string())?;
            Ok(Response::Event(event))
        }
        Call::Events => {
            let events = storage
                .with(|replica| wf_store::EventStore::events(replica.store()))
                .await
                .map_err(closed)?
                .map_err(|error| error.to_string())?;
            Ok(Response::Events(events))
        }
        Call::Author { actor, body } => {
            let signer = daemon.signer().clone();
            let now_ms = now_ms();
            let event = storage
                .with(move |replica| replica.author(&signer, actor, now_ms, *body))
                .await
                .map_err(closed)?
                .map_err(|error| error.to_string())?;
            loaded.nudge.notify_one();
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
            let results = sync_all(daemon.node(), workforce, peers, Some(daemon.presence())).await;
            Ok(Response::Synced(results))
        }
        Call::LastSeen => Ok(Response::LastSeen(daemon.presence().snapshot())),
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

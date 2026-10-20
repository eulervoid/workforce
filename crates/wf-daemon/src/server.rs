//! Answers the CLI on the unix socket, for whom each connection acts ([`crate::caller`]).

use std::sync::Arc;

use serde::Serialize;
use tokio::net::UnixStream;
use wf_core::event::Event;
use wf_core::id::{HashId, WorkforceId};
use wf_core::identity::{Actor, DeviceKey};
use wf_core::invite::{InviteCode, InviteCodeError};
use wf_core::lifecycle::{Interruption, SessionEnd};
use wf_core::session::SessionRole;
use wf_core::state::State;
use wf_net::frame::{FrameError, read_frame, write_encoded_frame, write_frame};
use wf_store::{RedbStore, Replica};

use crate::caller::Caller;
use crate::peers::{PeerSync, peers_of, sync_all};
use crate::proto::{
    Call, Credential, EVENTS_PER_PAGE, Hello, HelloReply, PROTOCOL_VERSION, Reply, Request,
};
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
    let credential: Credential = read_frame(stream).await?;
    let Some(caller) = credential
        .token
        .and_then(|token| daemon.callers.recognize(&token))
    else {
        return write_frame(stream, &HelloReply::Unrecognized).await;
    };
    write_frame(
        stream,
        &HelloReply::Ready {
            version: PROTOCOL_VERSION,
            pid: std::process::id(),
            device: daemon.device(),
            caller,
        },
    )
    .await?;
    loop {
        let request: Request = match read_frame(stream).await {
            Ok(request) => request,
            Err(FrameError::Closed) => return Ok(()),
            Err(error) => return Err(error),
        };
        let reply = answer(daemon, caller, request).await;
        match write_encoded_frame(stream, &reply).await {
            Err(FrameError::TooLarge(size)) => {
                // Nothing of the answer was written, so the connection is still in step.
                let message = format!(
                    "the answer is {size} bytes, more than fits in one message on the socket"
                );
                write_frame(stream, &failed(message)).await?;
            }
            other => other?,
        }
    }
}

/// A failed [`Reply`], which reads the same whatever the call was answered with.
fn failed(message: String) -> Reply<()> {
    Err(message)
}

/// Encodes what a call is answered with.
fn reply<T: Serialize>(value: T) -> Result<Vec<u8>, String> {
    wf_core::encoding::encode(&Reply::Ok(value)).map_err(|error| wf_core::error_chain(&error))
}

/// The encoded [`Reply`] to a request.
async fn answer(daemon: &Arc<Daemon>, caller: Caller, request: Request) -> Vec<u8> {
    if let Err(message) = permit(caller, request.workforce, &request.call) {
        return encode_failed(message);
    }
    let opened = match request.call {
        Call::Join { ref code, .. } => match check_invite(code, request.workforce) {
            Ok(_) => daemon.follow(request.workforce).await,
            Err(message) => return encode_failed(message),
        },
        Call::Follow(_) => daemon.follow(request.workforce).await,
        Call::State
        | Call::Event(_)
        | Call::Events { .. }
        | Call::ItemEvents(_)
        | Call::AgentEvents(_)
        | Call::Author { .. }
        | Call::SyncNow
        | Call::Fork(_)
        | Call::StartSession { .. }
        | Call::EndSession { .. }
        | Call::StopSession(_)
        | Call::Permissions
        | Call::AnswerPermission { .. }
        | Call::LastSeen
        | Call::MachineFaults
        | Call::CacheUsage
        | Call::LiveDiff { .. }
        | Call::TargetCommit => daemon.workforce(request.workforce).await,
    };
    let loaded = match opened {
        Ok(loaded) => loaded,
        Err(error) => return encode_failed(wf_core::error_chain(&error)),
    };
    match call(daemon, caller, request.workforce, &loaded, request.call).await {
        Ok(reply) => reply,
        Err(message) => encode_failed(message),
    }
}

/// The invite `code` names, if it is a valid one for `workforce`. Checked before the workforce
/// is created, so a bad join leaves nothing behind.
fn check_invite(code: &str, workforce: WorkforceId) -> Result<InviteCode, String> {
    let code: InviteCode = code
        .parse()
        .map_err(|error: InviteCodeError| wf_core::error_chain(&error))?;
    if code.workforce != workforce {
        return Err("the invite is for another workforce".to_string());
    }
    Ok(code)
}

/// Refuses what a session may not do, before anything is opened for it. A session acts in its
/// own workforce, and the calls that act for the principal are the principal's.
fn permit(caller: Caller, workforce: WorkforceId, call: &Call) -> Result<(), String> {
    let Caller::Session {
        workforce: own,
        session,
    } = caller
    else {
        return Ok(());
    };
    if own != workforce {
        return Err(format!(
            "session {session} acts in workforce {own} only, not in {workforce}"
        ));
    }
    let principals = |what: &str| Err(format!("only the principal can {what}, not a session"));
    match call {
        Call::State
        | Call::Event(_)
        | Call::Events { .. }
        | Call::ItemEvents(_)
        | Call::AgentEvents(_)
        | Call::Author { .. }
        | Call::SyncNow
        | Call::LastSeen
        | Call::MachineFaults
        | Call::CacheUsage
        | Call::Permissions
        | Call::TargetCommit
        | Call::EndSession { .. } => Ok(()),
        Call::Fork(_) => principals("found a fork"),
        Call::Join { .. } => principals("join a workforce"),
        Call::Follow(_) => principals("follow a workforce"),
        Call::StartSession { .. } => principals("start sessions"),
        Call::StopSession(_) => principals("stop sessions"),
        Call::AnswerPermission { .. } => principals("answer permission requests"),
        Call::LiveDiff { .. } => principals("ask for a live diff"),
    }
}

/// Who writes what a connection of `caller` authors through the daemon of `device`.
fn actor(state: &State, device: DeviceKey, caller: Caller) -> Result<Actor, String> {
    match caller {
        Caller::Principal => state
            .principal_of(&device)
            .map(Actor::Principal)
            .ok_or_else(|| {
                "this machine's device is not bound to a member of the workforce".to_string()
            }),
        Caller::Session { session, .. } => state
            .session(session)
            .map(wf_core::session::Session::actor)
            .ok_or_else(|| format!("session {session} is not in the workforce")),
    }
}

fn encode_failed(message: String) -> Vec<u8> {
    wf_core::encoding::encode(&failed(message)).expect("a message encodes")
}

/// The events of the log that `keep` accepts, in log order, read a page at a time.
fn filtered_log(
    replica: &Replica<RedbStore>,
    keep: impl Fn(&State, &Event) -> bool,
) -> Result<Vec<Event>, wf_store::StoreError> {
    let mut kept = Vec::new();
    let mut position = 0;
    loop {
        let page = wf_store::EventStore::log_page(replica.store(), position, EVENTS_PER_PAGE)?;
        if page.is_empty() {
            return Ok(kept);
        }
        position += page.len() as u64;
        kept.extend(
            page.into_iter()
                .filter(|event| keep(replica.state(), event)),
        );
    }
}

async fn call(
    daemon: &Arc<Daemon>,
    caller: Caller,
    workforce: WorkforceId,
    loaded: &Arc<Loaded>,
    call: Call,
) -> Result<Vec<u8>, String> {
    let storage = &loaded.storage;
    let closed = |error: crate::StorageClosed| wf_core::error_chain(&error);
    match call {
        Call::State => {
            let bytes = storage
                .with(|replica| wf_core::encoding::encode(replica.state()))
                .await
                .map_err(closed)?
                .map_err(|error| wf_core::error_chain(&error))?;
            reply(bytes)
        }
        Call::Event(id) => {
            let event = storage
                .with(move |replica| wf_store::EventStore::event(replica.store(), &id))
                .await
                .map_err(closed)?
                .map_err(|error| wf_core::error_chain(&error))?;
            reply(event)
        }
        Call::Events { from } => {
            let events = storage
                .with(move |replica| {
                    wf_store::EventStore::log_page(replica.store(), from, EVENTS_PER_PAGE)
                })
                .await
                .map_err(closed)?
                .map_err(|error| wf_core::error_chain(&error))?;
            reply(events)
        }
        Call::ItemEvents(item) => {
            let events = storage
                .with(move |replica| {
                    filtered_log(replica, |state, event| {
                        state.can_be_activity_of(item, event)
                    })
                })
                .await
                .map_err(closed)?
                .map_err(|error| wf_core::error_chain(&error))?;
            reply(events)
        }
        Call::AgentEvents(agent) => {
            let events = storage
                .with(move |replica| {
                    filtered_log(replica, |state, event| {
                        state.agent_change(&agent, event).is_some()
                    })
                })
                .await
                .map_err(closed)?
                .map_err(|error| wf_core::error_chain(&error))?;
            reply(events)
        }
        Call::Author { body } => {
            let body = match caller {
                Caller::Session { session, .. } => {
                    Box::new(daemon.with_head(workforce, loaded, session, *body).await)
                }
                Caller::Principal => body,
            };
            let signer = daemon.signer().clone();
            let now_ms = wf_store::now_ms();
            let event = storage
                .with(move |replica| {
                    let actor = actor(replica.state(), signer.key(), caller)?;
                    replica
                        .author(&signer, actor, now_ms, *body)
                        .map_err(|error| wf_core::error_chain(&error))
                })
                .await
                .map_err(closed)??;
            reply(event)
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
                return reply(Vec::<PeerSync>::new());
            }
            let node = daemon
                .node()
                .await
                .map_err(|error| wf_core::error_chain(&error))?;
            let results = sync_all(&node, workforce, peers, Some(daemon.presence())).await;
            reply(results)
        }
        Call::Fork(genesis) => {
            daemon
                .create_fork(loaded, *genesis)
                .await
                .map_err(|error| wf_core::error_chain(&error))?;
            reply(())
        }
        Call::Join { code, profile } => {
            let code = check_invite(&code, workforce)?;
            daemon
                .join(loaded, &code, profile)
                .await
                .map_err(|error| wf_core::error_chain(&error))?;
            reply(())
        }
        Call::Follow(devices) => {
            let peers = devices
                .into_iter()
                .map(|device| (device, device.fmt_short()))
                .collect();
            let node = daemon
                .node()
                .await
                .map_err(|error| wf_core::error_chain(&error))?;
            let results = sync_all(&node, workforce, peers, Some(daemon.presence())).await;
            reply(results)
        }
        Call::LastSeen => reply(daemon.presence().snapshot()),
        Call::MachineFaults => reply(daemon.faults.current()),
        Call::CacheUsage => reply(daemon.cache_usage_of(workforce)),
        Call::LiveDiff { item, since } => {
            let diff = daemon
                .live_diff(workforce, item, since)
                .await
                .map_err(|error| wf_core::error_chain(&error))?;
            reply(diff)
        }
        Call::TargetCommit => {
            let commit = daemon
                .target_commit(workforce, loaded)
                .await
                .map_err(|error| wf_core::error_chain(&error))?;
            reply(commit)
        }
        Call::StartSession {
            agent,
            item,
            step,
            repository,
        } => {
            let state = daemon
                .snapshot(loaded)
                .await
                .map_err(|error| wf_core::error_chain(&error))?;
            let role = match state.review_to_start(item, step.as_ref()) {
                Some(role) => role.map_err(|error| error.to_string())?,
                None if step.is_some() => {
                    return Err(format!(
                        "#{} awaits no review, so there is no step to review",
                        item.short()
                    ));
                }
                None => SessionRole::Work,
            };
            let session = daemon
                .start_session(workforce, loaded, agent, item, role, repository)
                .await
                .map_err(|error| wf_core::error_chain(&error))?;
            reply(session)
        }
        Call::Permissions => reply(daemon.held.list(workforce)),
        Call::AnswerPermission {
            session,
            since_ms,
            answer,
        } => daemon
            .held
            .answer(session, since_ms, answer)
            .map_err(|refused| refused.to_string())
            .and_then(reply),
        Call::EndSession { finish } => {
            let Caller::Session { session, .. } = caller else {
                return Err(
                    "only a session ends itself; the principal stops a session instead".to_string(),
                );
            };
            daemon
                .finish_session(workforce, loaded, session, SessionEnd::Finish(finish))
                .await
                .map_err(|error| wf_core::error_chain(&error))
                .and_then(reply)
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
                .map_err(|error| wf_core::error_chain(&error))?;
            reply(())
        }
    }
}

#[cfg(test)]
mod tests {
    use wf_core::id::{EventId, SessionId};
    use wf_core::testing::*;
    use wf_harness::Permission;

    use super::*;

    fn session_of(workforce: WorkforceId, session: SessionId) -> Caller {
        Caller::Session { workforce, session }
    }

    #[test]
    fn a_session_cannot_make_the_principals_calls_or_act_in_another_workforce() {
        let workforce = WorkforceId::from(EventId::of(b"workforce"));
        let session = SessionId::from(EventId::of(b"session"));
        let caller = session_of(workforce, session);
        let principals = [
            Call::AnswerPermission {
                session,
                since_ms: 0,
                answer: Permission::Allow,
            },
            Call::StopSession(session),
            Call::Follow(Vec::new()),
            Call::StartSession {
                agent: wf_core::name::AgentName::new("coder").unwrap(),
                item: wf_core::id::ItemId::from(EventId::of(b"item")),
                step: None,
                repository: "/repository".into(),
            },
        ];
        for call in &principals {
            assert!(permit(caller, workforce, call).is_err());
            assert!(permit(Caller::Principal, workforce, call).is_ok());
        }
        assert!(permit(caller, workforce, &Call::State).is_ok());
        let other = WorkforceId::from(EventId::of(b"other"));
        assert!(permit(caller, other, &Call::State).is_err());
    }

    #[test]
    fn what_a_connection_authors_is_written_by_whom_it_acts_for() {
        let fixture = Fixture::new(1);
        let mut state = fixture.state();
        let item = fixture.apply(&mut state, 0, 2_000, create("task", "work"));
        fixture.apply(&mut state, 0, 3_000, define_agent("coder"));
        let started = fixture.apply(
            &mut state,
            0,
            4_000,
            start_session("coder", item.id().into()),
        );
        let session = SessionId::from(started.id());
        let device = fixture.devices[0].key();

        assert_eq!(
            actor(&state, device, Caller::Principal),
            Ok(fixture.founder_actor())
        );
        let agent = actor(&state, device, session_of(fixture.workforce(), session)).unwrap();
        assert!(matches!(agent, Actor::Agent { session: acting, .. } if acting == session));
        let unknown = SessionId::from(EventId::of(b"unknown"));
        assert!(actor(&state, device, session_of(fixture.workforce(), unknown)).is_err());
    }
}

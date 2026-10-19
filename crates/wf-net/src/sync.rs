//! `wf/sync/2`: bring two replicas of one workforce to the same set of events.
//!
//! 1. Both sides send their version vector.
//! 2. Each sends the events the other lacks, in causal order, all branches of equivocated feeds.
//! 3. Each side then asks for the parents of events it had to park, by id, until nothing is
//!    missing or the peer does not have it.
//!
//! Only one side writes at a time, so neither end can block the other on a full buffer. The
//! initiator goes first at every step.

use std::collections::BTreeSet;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};
use wf_core::event::Event;
use wf_core::id::{EventId, WorkforceId};
use wf_core::sync::Versions;

use crate::frame::{FrameError, read_frame, write_frame};
use crate::log::{LogError, SyncLog};

pub const SYNC_ALPN: &[u8] = b"wf/sync/2";

/// A peer that says nothing for this long has stopped answering: the session ends.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

const EVENTS_PER_FRAME: usize = 256;
const IDS_PER_WANT: usize = 1024;

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error(transparent)]
    Log(#[from] LogError),
    #[error("the peer sent {got} where {expected} was expected")]
    Unexpected {
        expected: &'static str,
        got: &'static str,
    },
    #[error("the peer asked for {0} events at once")]
    WantTooLarge(usize),
    #[error("the peer stopped answering")]
    TimedOut,
}

#[derive(Serialize, Deserialize)]
enum Message {
    /// First message of the initiator: which workforce to sync.
    Open(WorkforceId),
    Versions(Versions),
    Events(Vec<Event>),
    EndOfEvents,
    /// Ids the sender needs.
    Want(Vec<EventId>),
    EndOfWants,
    Have(Vec<Event>),
}

impl Message {
    fn name(&self) -> &'static str {
        match self {
            Message::Open(_) => "open",
            Message::Versions(_) => "versions",
            Message::Events(_) => "events",
            Message::EndOfEvents => "end of events",
            Message::Want(_) => "want",
            Message::EndOfWants => "end of wants",
            Message::Have(_) => "have",
        }
    }
}

/// What one run of the protocol changed on this side.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub sent: usize,
    pub applied: usize,
    pub rejected: usize,
}

/// Which side of the stream a session is.
#[derive(Clone, Copy)]
enum Role {
    /// Opened the stream, and goes first at every step.
    Initiator,
    Responder,
}

struct Session<'a, S, L> {
    stream: S,
    log: &'a L,
    report: SyncReport,
    idle: Duration,
}

impl<S: AsyncRead + AsyncWrite + Unpin, L: SyncLog> Session<'_, S, L> {
    async fn send(&mut self, message: Message) -> Result<(), SyncError> {
        Ok(write_frame(&mut self.stream, &message).await?)
    }

    async fn receive(&mut self) -> Result<Message, SyncError> {
        tokio::time::timeout(self.idle, read_frame(&mut self.stream))
            .await
            .map_err(|_| SyncError::TimedOut)?
            .map_err(SyncError::from)
    }

    async fn exchange_versions(&mut self, role: Role) -> Result<Versions, SyncError> {
        let mine = self.log.versions().await?;
        if let Role::Initiator = role {
            self.send(Message::Versions(mine.clone())).await?;
        }
        let theirs = match self.receive().await? {
            Message::Versions(versions) => versions,
            other => {
                return Err(SyncError::Unexpected {
                    expected: "versions",
                    got: other.name(),
                });
            }
        };
        if let Role::Responder = role {
            self.send(Message::Versions(mine)).await?;
        }
        Ok(theirs)
    }

    async fn push_events(&mut self, theirs: &Versions) -> Result<(), SyncError> {
        let events = self.log.events_after(theirs).await?;
        self.report.sent += events.len();
        for batch in events.chunks(EVENTS_PER_FRAME) {
            self.send(Message::Events(batch.to_vec())).await?;
        }
        self.send(Message::EndOfEvents).await
    }

    async fn accept_events(&mut self) -> Result<(), SyncError> {
        loop {
            match self.receive().await? {
                Message::Events(events) => self.apply(events).await?,
                Message::EndOfEvents => return Ok(()),
                other => {
                    return Err(SyncError::Unexpected {
                        expected: "events",
                        got: other.name(),
                    });
                }
            }
        }
    }

    async fn apply(&mut self, events: Vec<Event>) -> Result<(), SyncError> {
        let summary = self.log.receive(events).await?;
        self.report.applied += summary.applied;
        self.report.rejected += summary.rejected;
        Ok(())
    }

    /// Asks for the parents of parked events, each id at most once.
    async fn pull_missing(&mut self) -> Result<(), SyncError> {
        let mut asked = BTreeSet::new();
        loop {
            self.log.fill_history().await?;
            let wanted: Vec<EventId> = self
                .log
                .missing_parents()
                .await?
                .into_iter()
                .filter(|id| !asked.contains(id))
                .take(IDS_PER_WANT)
                .collect();
            if wanted.is_empty() {
                return self.send(Message::EndOfWants).await;
            }
            asked.extend(wanted.iter().copied());
            self.send(Message::Want(wanted)).await?;
            match self.receive().await? {
                Message::Have(events) => self.apply(events).await?,
                other => {
                    return Err(SyncError::Unexpected {
                        expected: "have",
                        got: other.name(),
                    });
                }
            }
        }
    }

    async fn answer_wants(&mut self) -> Result<(), SyncError> {
        loop {
            match self.receive().await? {
                Message::EndOfWants => return Ok(()),
                Message::Want(ids) if ids.len() > IDS_PER_WANT => {
                    return Err(SyncError::WantTooLarge(ids.len()));
                }
                Message::Want(ids) => {
                    let events = self.log.events_by_id(&ids).await?;
                    self.report.sent += events.len();
                    self.send(Message::Have(events)).await?;
                }
                other => {
                    return Err(SyncError::Unexpected {
                        expected: "want",
                        got: other.name(),
                    });
                }
            }
        }
    }
}

/// Reads which workforce the initiator wants to sync. The responder then picks the log and
/// calls [`respond`].
pub async fn read_open<S>(stream: &mut S) -> Result<WorkforceId, SyncError>
where
    S: AsyncRead + Unpin,
{
    let message = tokio::time::timeout(IDLE_TIMEOUT, read_frame(stream))
        .await
        .map_err(|_| SyncError::TimedOut)??;
    match message {
        Message::Open(workforce) => Ok(workforce),
        other => Err(SyncError::Unexpected {
            expected: "open",
            got: other.name(),
        }),
    }
}

/// Runs the protocol as the side that opened the stream.
pub async fn initiate<S, L>(
    stream: S,
    workforce: WorkforceId,
    log: &L,
) -> Result<SyncReport, SyncError>
where
    S: AsyncRead + AsyncWrite + Unpin,
    L: SyncLog,
{
    initiate_within(stream, workforce, log, IDLE_TIMEOUT).await
}

/// Like [`initiate`], giving up when the peer is silent for `idle`.
pub async fn initiate_within<S, L>(
    mut stream: S,
    workforce: WorkforceId,
    log: &L,
    idle: Duration,
) -> Result<SyncReport, SyncError>
where
    S: AsyncRead + AsyncWrite + Unpin,
    L: SyncLog,
{
    write_frame(&mut stream, &Message::Open(workforce)).await?;
    let mut session = Session {
        stream,
        log,
        report: SyncReport::default(),
        idle,
    };
    let theirs = session.exchange_versions(Role::Initiator).await?;
    session.push_events(&theirs).await?;
    session.accept_events().await?;
    session.pull_missing().await?;
    session.answer_wants().await?;
    Ok(session.report)
}

/// Runs the protocol as the side that accepted the stream, after [`read_open`].
pub async fn respond<S, L>(stream: S, log: &L) -> Result<SyncReport, SyncError>
where
    S: AsyncRead + AsyncWrite + Unpin,
    L: SyncLog,
{
    respond_within(stream, log, IDLE_TIMEOUT).await
}

/// Like [`respond`], giving up when the peer is silent for `idle`.
pub async fn respond_within<S, L>(
    stream: S,
    log: &L,
    idle: Duration,
) -> Result<SyncReport, SyncError>
where
    S: AsyncRead + AsyncWrite + Unpin,
    L: SyncLog,
{
    let mut session = Session {
        stream,
        log,
        report: SyncReport::default(),
        idle,
    };
    let theirs = session.exchange_versions(Role::Responder).await?;
    session.accept_events().await?;
    session.push_events(&theirs).await?;
    session.answer_wants().await?;
    session.pull_missing().await?;
    Ok(session.report)
}

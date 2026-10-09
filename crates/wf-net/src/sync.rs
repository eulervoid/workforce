//! `wf/sync/1`: bring two replicas of one workforce to the same set of events.
//!
//! 1. Both sides send their version vector.
//! 2. Each sends the events the other lacks, in causal order, all branches of forked feeds.
//! 3. Each side then asks for the parents of events it had to park, by id, until nothing is
//!    missing or the peer does not have it.
//!
//! Only one side writes at a time, so neither end can block the other on a full buffer. The
//! initiator goes first at every step.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};
use wf_core::event::Event;
use wf_core::id::EventId;
use wf_core::sync::Versions;

use crate::frame::{FrameError, read_frame, write_frame};
use crate::log::{LogError, SyncLog};

pub const SYNC_ALPN: &[u8] = b"wf/sync/1";

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
}

#[derive(Serialize, Deserialize)]
enum Message {
    Versions(Versions),
    Events(Vec<Event>),
    EndOfEvents,
    /// Ids the sender needs. Empty ends the exchange of wants.
    Want(Vec<EventId>),
    Have(Vec<Event>),
}

impl Message {
    fn name(&self) -> &'static str {
        match self {
            Message::Versions(_) => "versions",
            Message::Events(_) => "events",
            Message::EndOfEvents => "end of events",
            Message::Want(_) => "want",
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

struct Session<'a, S, L> {
    stream: S,
    log: &'a L,
    report: SyncReport,
}

impl<S: AsyncRead + AsyncWrite + Unpin, L: SyncLog> Session<'_, S, L> {
    async fn send(&mut self, message: Message) -> Result<(), SyncError> {
        Ok(write_frame(&mut self.stream, &message).await?)
    }

    async fn receive(&mut self) -> Result<Message, SyncError> {
        Ok(read_frame(&mut self.stream).await?)
    }

    async fn exchange_versions(&mut self, send_first: bool) -> Result<Versions, SyncError> {
        let mine = self.log.versions().await?;
        if send_first {
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
        if !send_first {
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
            let wanted: Vec<EventId> = self
                .log
                .missing()
                .await?
                .into_iter()
                .filter(|id| !asked.contains(id))
                .take(IDS_PER_WANT)
                .collect();
            if wanted.is_empty() {
                return self.send(Message::Want(Vec::new())).await;
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
                Message::Want(ids) if ids.is_empty() => return Ok(()),
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

/// Runs the protocol as the side that opened the stream.
pub async fn initiate<S, L>(stream: S, log: &L) -> Result<SyncReport, SyncError>
where
    S: AsyncRead + AsyncWrite + Unpin,
    L: SyncLog,
{
    let mut session = Session {
        stream,
        log,
        report: SyncReport::default(),
    };
    let theirs = session.exchange_versions(true).await?;
    session.push_events(&theirs).await?;
    session.accept_events().await?;
    session.pull_missing().await?;
    session.answer_wants().await?;
    Ok(session.report)
}

/// Runs the protocol as the side that accepted the stream.
pub async fn respond<S, L>(stream: S, log: &L) -> Result<SyncReport, SyncError>
where
    S: AsyncRead + AsyncWrite + Unpin,
    L: SyncLog,
{
    let mut session = Session {
        stream,
        log,
        report: SyncReport::default(),
    };
    let theirs = session.exchange_versions(false).await?;
    session.accept_events().await?;
    session.push_events(&theirs).await?;
    session.answer_wants().await?;
    session.pull_missing().await?;
    Ok(session.report)
}

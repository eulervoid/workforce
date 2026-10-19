//! `wf/sync/3`: bring two replicas of one workforce to the same set of events.
//!
//! 1. Both sides send their version vector.
//! 2. Each sends the events the other lacks, in causal order, all branches of equivocated feeds.
//! 3. Each side then asks for the parents of events it had to park, by id, until nothing is
//!    missing or the peer does not have it. The answer is any number of `Have` frames and an
//!    `EndOfHave`.
//!
//! Only one side writes at a time, so neither end can block the other on a full buffer. The
//! initiator goes first at every step. Frames hold events up to [`BATCH_BYTES`], so no log is too
//! long for one, and a peer that sends or accepts nothing for [`IDLE_TIMEOUT`] ends the session.

use std::collections::BTreeSet;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};
use wf_core::event::Event;
use wf_core::id::{EventId, WorkforceId};
use wf_core::sync::Versions;

use crate::frame::{FrameError, MAX_FRAME_LEN, read_frame_within, write_frame_within};
use crate::log::{LogError, SyncLog};

pub const SYNC_ALPN: &[u8] = b"wf/sync/3";

/// A peer that neither sends nor accepts a byte for this long has stopped answering: the session
/// ends. It restarts with every piece of a frame, so large frames over slow links go through.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

const EVENTS_PER_FRAME: usize = 256;
/// A frame of events is closed once it holds this many encoded bytes. An event is at most
/// `MAX_EVENT_LEN`, so a frame stays far below `MAX_FRAME_LEN`.
const BATCH_BYTES: usize = 8 * 1024 * 1024;
const IDS_PER_WANT: usize = 1024;

/// Largest frame of the messages whose content is small. Sizes are the encoded message with
/// generous room; a peer that sends more is not speaking the protocol.
const OPEN_FRAME_LEN: u32 = 256;
const VERSIONS_FRAME_LEN: u32 = 1024 * 1024;
const WANT_FRAME_LEN: u32 = 64 * 1024;

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
    #[error("the peer opened another workforce than the one it was admitted for")]
    OtherWorkforce,
    #[error("the peer asked for {0} events at once")]
    WantTooLarge(usize),
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
    /// Some of the events asked for; the answer ends with `EndOfHave`.
    Have(Vec<Event>),
    EndOfHave,
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
            Message::EndOfHave => "end of have",
        }
    }

    /// The error for receiving this message where `expected` was due.
    fn unexpected(&self, expected: &'static str) -> SyncError {
        SyncError::Unexpected {
            expected,
            got: self.name(),
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
        Ok(write_frame_within(&mut self.stream, &message, self.idle).await?)
    }

    /// The next message, which may be at most `max_len` bytes encoded.
    async fn receive(&mut self, max_len: u32) -> Result<Message, SyncError> {
        Ok(read_frame_within(&mut self.stream, max_len, self.idle).await?)
    }

    async fn exchange_versions(&mut self, role: Role) -> Result<Versions, SyncError> {
        let mine = self.log.versions().await?;
        if let Role::Initiator = role {
            self.send(Message::Versions(mine.clone())).await?;
        }
        let theirs = match self.receive(VERSIONS_FRAME_LEN).await? {
            Message::Versions(versions) => versions,
            other => {
                return Err(other.unexpected("versions"));
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
        self.send_batches(&events, Message::Events).await?;
        self.send(Message::EndOfEvents).await
    }

    async fn send_batches(
        &mut self,
        events: &[Event],
        message: fn(Vec<Event>) -> Message,
    ) -> Result<(), SyncError> {
        for batch in batches(events, EVENTS_PER_FRAME, BATCH_BYTES) {
            self.send(message(batch.to_vec())).await?;
        }
        Ok(())
    }

    async fn accept_events(&mut self) -> Result<(), SyncError> {
        loop {
            match self.receive(MAX_FRAME_LEN).await? {
                Message::Events(events) => self.apply(events).await?,
                Message::EndOfEvents => return Ok(()),
                other => {
                    return Err(other.unexpected("events"));
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
            loop {
                match self.receive(MAX_FRAME_LEN).await? {
                    Message::Have(events) => self.apply(events).await?,
                    Message::EndOfHave => break,
                    other => {
                        return Err(other.unexpected("have"));
                    }
                }
            }
        }
    }

    async fn answer_wants(&mut self) -> Result<(), SyncError> {
        loop {
            match self.receive(WANT_FRAME_LEN).await? {
                Message::EndOfWants => return Ok(()),
                Message::Want(ids) if ids.len() > IDS_PER_WANT => {
                    return Err(SyncError::WantTooLarge(ids.len()));
                }
                Message::Want(ids) => {
                    let events = self.log.events_by_id(&ids).await?;
                    self.report.sent += events.len();
                    self.send_batches(&events, Message::Have).await?;
                    self.send(Message::EndOfHave).await?;
                }
                other => {
                    return Err(other.unexpected("want"));
                }
            }
        }
    }
}

/// Splits `events` into runs of at most `max_events` and, unless a single event is larger, at most
/// `max_bytes` encoded bytes. A run has at least one event.
fn batches(events: &[Event], max_events: usize, max_bytes: usize) -> Vec<&[Event]> {
    let mut batches = Vec::new();
    let mut start = 0;
    let mut bytes = 0;
    for (index, event) in events.iter().enumerate() {
        let length = event.encode().map_or(0, |encoded| encoded.len());
        if index > start && (index - start == max_events || bytes + length > max_bytes) {
            batches.push(&events[start..index]);
            start = index;
            bytes = 0;
        }
        bytes += length;
    }
    if start < events.len() {
        batches.push(&events[start..]);
    }
    batches
}

/// Reads which workforce the initiator wants to sync, giving up when nothing arrives for `idle`.
/// The responder then picks the log and calls [`respond`].
pub async fn read_open<S>(stream: &mut S, idle: Duration) -> Result<WorkforceId, SyncError>
where
    S: AsyncRead + Unpin,
{
    let message = read_frame_within(stream, OPEN_FRAME_LEN, idle).await?;
    match message {
        Message::Open(workforce) => Ok(workforce),
        Message::Versions(_)
        | Message::Events(_)
        | Message::EndOfEvents
        | Message::Want(_)
        | Message::EndOfWants
        | Message::Have(_)
        | Message::EndOfHave => Err(message.unexpected("open")),
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
    write_frame_within(&mut stream, &Message::Open(workforce), idle).await?;
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

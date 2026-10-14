//! What the CLI and the daemon say to each other over the unix socket.
//!
//! The daemon serves the replica, not commands: the CLI asks for the state and for events, and
//! has the daemon author events, so rendering, drafts and prompts stay in the CLI. Frames are
//! length-prefixed postcard. The first message in each direction carries the protocol version,
//! so a CLI and a daemon from different builds refuse each other cleanly. The client's second
//! message is the token it presents, from which the daemon decides whom the connection acts for
//! ([`crate::caller`]); no request names an actor.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use wf_core::encoding;
use wf_core::event::{DeviceProfile, Event, EventBody};
use wf_core::id::{EventId, ItemId, SessionId, WorkforceId};
use wf_core::identity::DeviceKey;
use wf_core::lifecycle::Finish;
use wf_core::name::AgentName;
use wf_harness::Permission;
use wf_net::frame::MAX_FRAME_LEN;

use crate::caller::{Caller, Token};

pub const PROTOCOL_VERSION: u32 = 12;

/// A frame holds this many events at most, so a long log does not need one huge frame.
pub const EVENTS_PER_PAGE: usize = 512;

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("socket failed")]
    Io(#[from] std::io::Error),
    #[error("message does not decode")]
    Encoding(#[from] encoding::EncodingError),
    #[error("frame of {0} bytes is too large")]
    TooLarge(u64),
    #[error("the daemon speaks protocol {daemon}, this build speaks {client}; restart the daemon")]
    VersionMismatch { daemon: u32, client: u32 },
}

#[derive(Serialize, Deserialize)]
pub struct Hello {
    pub version: u32,
}

/// Sent after [`Hello`]: the token of the session the client runs in, or the principal token,
/// or nothing when it can read neither.
#[derive(Serialize, Deserialize)]
pub struct Credential {
    pub token: Option<Token>,
}

#[derive(Serialize, Deserialize)]
pub enum HelloReply {
    /// `device` is the device the daemon signs as, so that a client knows it without reading
    /// the device key, which agents are not let to. `caller` is whom the connection acts for.
    Ready {
        version: u32,
        device: DeviceKey,
        caller: Caller,
    },
    /// Answered to [`Hello`] of another protocol version, without waiting for the [`Credential`].
    Refused { version: u32 },
    /// The token is none the daemon handed out, or there was none.
    Unrecognized,
}

#[derive(Serialize, Deserialize)]
pub struct Request {
    pub workforce: WorkforceId,
    pub call: Call,
}

/// Each call is answered with a [`Reply`] of the type its documentation names.
///
/// A connection that acts for a session makes calls in the session's workforce only: the reads
/// (`State`, `Event`, `Events`, `LastSeen`, `Permissions`), `Author`, `SyncNow` and `EndSession`. `EndSession` is for sessions only; the other calls are
/// for the principal only.
#[derive(Serialize, Deserialize)]
pub enum Call {
    /// The encoded [`wf_core::state::State`], as `Vec<u8>`.
    State,
    /// `Option<Event>`.
    Event(EventId),
    /// A page of the log: up to [`EVENTS_PER_PAGE`] events in the order the daemon applied them,
    /// starting at position `from`, as `Vec<Event>`. An empty page is the end.
    Events { from: u64 },
    /// Checks the policy, signs with the device key of the daemon and stores the event, written
    /// by whom the connection acts for; answered with the `Event`.
    Author { body: Box<EventBody> },
    /// Syncs with every other bound device now; answered with a `Vec<PeerSync>`.
    SyncNow,
    /// Founds the workforce of this genesis as a fork of the one the request names, from what the
    /// daemon holds of it, and starts serving it; answered with `()`.
    Fork(Box<Event>),
    /// Joins the workforce of an invite code (`wfi1...`) as this machine's device, writing a
    /// profile for it, so the daemon's endpoint is the only one with its key; answered with `()`.
    Join {
        code: String,
        profile: DeviceProfile,
    },
    /// Starts taking in a workforce this machine does not have yet, from these devices, which may
    /// not be bound in it as far as this machine knows: how a device follows a fork it is only
    /// about to learn the members of. The history comes from the parent when it is stored here.
    /// Answered with a `Vec<PeerSync>`.
    Follow(Vec<DeviceKey>),
    /// When each device was last heard from, in milliseconds since the epoch, as
    /// `Vec<(DeviceKey, u64)>`.
    LastSeen,
    /// Starts a work session of `agent` on `item`, in a worktree of the repository at
    /// `repository` on this machine; answered with its `SessionId`.
    StartSession {
        agent: AgentName,
        item: ItemId,
        repository: PathBuf,
    },
    /// Ends the session the connection acts for after the agent's turn, as the agent says, and
    /// applies what follows from it to its item; answered with `()`.
    EndSession { finish: Finish },
    /// Stops a live session of this machine now, for the principal; answered with `()`.
    StopSession(SessionId),
    /// The permission requests that sessions of this machine wait to have answered, as
    /// `Vec<HeldPermission>`.
    Permissions,
    /// Answers the oldest permission request of a session; answered with that
    /// `HeldPermission`.
    AnswerPermission {
        session: SessionId,
        answer: Permission,
    },
}

/// A request of an agent for permission to do something, held until the principal answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldPermission {
    pub session: SessionId,
    /// The tool call that wants permission.
    pub what: String,
    /// Why each decider of the machine passed it on to the principal, as `decider: why`.
    pub passed: Vec<String>,
    /// Milliseconds since the Unix epoch.
    pub since_ms: u64,
}

/// What the daemon answers a [`Call`] with: what the call asks for, or why it failed. Each call
/// says what it is answered with, so the client decodes the reply as the type it expects.
pub type Reply<T> = Result<T, String>;

/// `$XDG_RUNTIME_DIR/workforce/daemon.sock`, falling back to the data directory.
pub fn socket_path(data_dir: &Path, runtime_dir: Option<&Path>) -> PathBuf {
    match runtime_dir {
        Some(runtime) => runtime.join("workforce").join("daemon.sock"),
        None => data_dir.join("daemon.sock"),
    }
}

pub fn write_message<W: Write, T: Serialize>(
    writer: &mut W,
    message: &T,
) -> Result<(), ProtoError> {
    let bytes = encoding::encode(message)?;
    let length = u32::try_from(bytes.len())
        .ok()
        .filter(|length| *length <= MAX_FRAME_LEN)
        .ok_or(ProtoError::TooLarge(bytes.len() as u64))?;
    writer.write_all(&length.to_be_bytes())?;
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}

pub fn read_message<R: Read, T: DeserializeOwned>(reader: &mut R) -> Result<T, ProtoError> {
    let mut length = [0u8; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length);
    if length > MAX_FRAME_LEN {
        return Err(ProtoError::TooLarge(u64::from(length)));
    }
    let mut bytes = vec![0u8; length as usize];
    reader.read_exact(&mut bytes)?;
    Ok(encoding::decode(&bytes)?)
}

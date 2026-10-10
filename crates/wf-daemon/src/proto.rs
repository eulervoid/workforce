//! What the CLI and the daemon say to each other over the unix socket.
//!
//! The daemon serves the replica, not commands: the CLI asks for the state and for events, and
//! has the daemon author events, so rendering, drafts and prompts stay in the CLI. Frames are
//! length-prefixed postcard. The first message in each direction carries the protocol version,
//! so a CLI and a daemon from different builds refuse each other cleanly.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use wf_core::encoding;
use wf_core::event::{Event, EventBody};
use wf_core::id::{EventId, WorkforceId};
use wf_core::identity::{Actor, DeviceKey};
use wf_net::frame::MAX_FRAME_LEN;

use crate::peers::PeerSync;

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("socket failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("message does not decode: {0}")]
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

#[derive(Serialize, Deserialize)]
pub enum HelloReply {
    Ready { version: u32 },
    Refused { version: u32 },
}

#[derive(Serialize, Deserialize)]
pub struct Request {
    pub workforce: WorkforceId,
    pub call: Call,
}

#[derive(Serialize, Deserialize)]
pub enum Call {
    /// The encoded [`wf_core::state::State`].
    State,
    Event(EventId),
    /// Every event, ordered by `(hlc, id)`.
    Events,
    /// Checks the policy, signs with the device key of the daemon and stores the event.
    Author {
        actor: Actor,
        body: Box<EventBody>,
    },
    /// Syncs with every other bound device now.
    SyncNow,
    /// Founds the workforce of this genesis as a fork of the one the request names, from what the
    /// daemon holds of it, and starts serving it.
    Fork(Box<Event>),
    /// Starts taking in a workforce this machine does not have yet, from these devices, which may
    /// not be bound in it as far as this machine knows: how a device follows a fork it is only
    /// about to learn the members of. The history comes from the parent when it is stored here.
    Follow(Vec<DeviceKey>),
    /// When each device was last heard from, in milliseconds since the epoch.
    LastSeen,
}

#[derive(Serialize, Deserialize)]
pub enum Response {
    State(Vec<u8>),
    Event(Option<Event>),
    Events(Vec<Event>),
    Authored(Event),
    Synced(Vec<PeerSync>),
    Forked,
    LastSeen(Vec<(DeviceKey, u64)>),
    Failed(String),
}

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

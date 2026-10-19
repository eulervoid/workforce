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
use wf_core::identity::{AgentId, DeviceKey};
use wf_core::lifecycle::Finish;
use wf_core::name::AgentName;
use wf_harness::Permission;
use wf_net::frame::{FrameError, blocking as frame};

use crate::caller::{Caller, Token};

pub const PROTOCOL_VERSION: u32 = 16;

/// A frame holds this many events at most, so a long log does not need one huge frame.
pub const EVENTS_PER_PAGE: usize = 512;

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("socket failed")]
    Io(#[from] std::io::Error),
    #[error(
        "message does not decode; the daemon may be another build than this one, \
         restart it with `wf daemon restart`"
    )]
    Encoding(#[from] encoding::EncodingError),
    #[error(transparent)]
    Frame(#[from] FrameError),
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
    /// `pid` is the process that answers, so that stopping the daemon never signals a process
    /// the socket did not vouch for.
    Ready {
        version: u32,
        pid: u32,
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
/// (`State`, `Event`, `Events`, `ItemEvents`, `AgentEvents`, `LastSeen`, `Permissions`,
/// `MachineFaults`, `CacheUsage`), `Author`, `SyncNow` and `EndSession`. `EndSession` is for
/// sessions only; the other calls are for the principal only.
#[derive(Serialize, Deserialize)]
pub enum Call {
    /// The encoded [`wf_core::state::State`], as `Vec<u8>`.
    State,
    /// `Option<Event>`.
    Event(EventId),
    /// A page of the log: up to [`EVENTS_PER_PAGE`] events in the order the daemon applied them,
    /// starting at position `from`, as `Vec<Event>`. An empty page is the end.
    Events { from: u64 },
    /// The events of the log that can show in the activity of an item, in the order the daemon
    /// applied them, as `Vec<Event>` ([`wf_core::state::State::can_be_activity_of`]).
    ItemEvents(ItemId),
    /// The events that defined or removed an agent, in the order the daemon applied them, as
    /// `Vec<Event>`.
    AgentEvents(AgentId),
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
    /// Answers the oldest permission request of a session, which must be the one held since
    /// `since_ms` ([`HeldPermission::since_ms`]); answered with that `HeldPermission`.
    AnswerPermission {
        session: SessionId,
        since_ms: u64,
        answer: Permission,
    },
    /// What is wrong with the machine, as [`MachineFaults`](crate::MachineFaults): that the
    /// daemon's writes fail for lack of space, and the harnesses whose sessions cannot run.
    MachineFaults,
    /// What the caches in the worktrees of the workforce took when the daemon last checked them
    /// against the budget of the machine config, as `Option<CacheUsage>`; `None` without a budget.
    CacheUsage,
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
    Ok(frame::write_frame(writer, message)?)
}

pub fn read_message<R: Read, T: DeserializeOwned>(reader: &mut R) -> Result<T, ProtoError> {
    Ok(frame::read_frame(reader)?)
}

#[cfg(test)]
mod tests {
    use wf_core::id::EventId;
    use wf_core::identity::DeviceSigner;
    use wf_core::testing::golden_state;

    use super::*;
    use crate::caches::CacheUsage;
    use crate::faults::MachineFaults;
    use crate::peers::{PeerOutcome, PeerSync};

    /// The CLI decodes replies by the type each call names, and postcard is not self-describing,
    /// so a reply of another shape decodes into garbage or fails obscurely. The bytes below are
    /// the replies of one fixed fixture, one hash per call that carries a structure of its own.
    #[test]
    fn golden_reply_bytes() {
        let device = DeviceSigner::from_bytes(&[2; 32]).unwrap().key();
        let held = HeldPermission {
            session: SessionId::from_bytes([3; 32]),
            what: "bash: cargo test".into(),
            passed: vec!["rules: no rule".into()],
            since_ms: 5_000,
        };
        let peers = vec![
            PeerSync {
                device,
                label: "laptop".into(),
                outcome: PeerOutcome::Synced {
                    sent: 1,
                    received: 2,
                    rejected: 3,
                },
            },
            PeerSync {
                device,
                label: "server".into(),
                outcome: PeerOutcome::Unreachable("timed out".into()),
            },
        ];
        let usage = Some(CacheUsage {
            used_bytes: 10,
            budget_bytes: 20,
        });
        let state = encoding::encode(&golden_state()).unwrap();
        let replies: Vec<(&str, Vec<u8>)> = vec![
            ("State", encoding::encode(&Reply::Ok(state)).unwrap()),
            (
                "Permissions",
                encoding::encode(&Reply::Ok(vec![held])).unwrap(),
            ),
            ("SyncNow", encoding::encode(&Reply::Ok(peers)).unwrap()),
            (
                "MachineFaults",
                encoding::encode(&Reply::Ok(MachineFaults::default())).unwrap(),
            ),
            ("CacheUsage", encoding::encode(&Reply::Ok(usage)).unwrap()),
            (
                "failure",
                encoding::encode(&Reply::<()>::Err("no".into())).unwrap(),
            ),
        ];
        let hashes: Vec<(&str, usize, String)> = replies
            .iter()
            .map(|(call, bytes)| (*call, bytes.len(), EventId::of(bytes).to_string()))
            .collect();
        let pinned = GOLDEN_REPLIES.to_vec();
        let found: Vec<(&str, usize, &str)> = hashes
            .iter()
            .map(|(call, len, hash)| (*call, *len, hash.as_str()))
            .collect();
        assert_eq!(
            (PROTOCOL_VERSION, found),
            (GOLDEN_PROTOCOL_VERSION, pinned),
            "the encoded shape of a daemon reply or of `State` changed. Bump `PROTOCOL_VERSION` \
             and `SNAPSHOT_FORMAT` in wf-store/src/replica.rs, then update \
             `GOLDEN_PROTOCOL_VERSION` and `GOLDEN_REPLIES` here and `GOLDEN_SNAPSHOT` there to \
             what these tests print"
        );
    }

    /// A [`Request`] is decoded by the daemon the way a reply is by the CLI, so a `Call` that
    /// gained, lost or reordered a variant or a field shifts the bytes of every call after it.
    /// One request per call, from fixed keys; the events inside are pinned by their own tests.
    #[test]
    fn golden_request_bytes() {
        use wf_core::event::DeviceProfile;
        use wf_core::identity::PrincipalSigner;
        use wf_core::testing::{Fixture, comment};

        let fixture = Fixture::new(2);
        let state = fixture.state();
        let item = ItemId::from_bytes([1; 32]);
        let device = DeviceSigner::from_bytes(&[2; 32]).unwrap().key();
        let fork = fixture.fork(&state, &PrincipalSigner::generate([9; 32]), 3_000);
        let profile = DeviceProfile {
            name: "laptop".into(),
            hardware: "gpu, os:linux".into(),
            agents: [(AgentName::new("claude").unwrap(), 2)].into(),
            max_sessions: 4,
        };
        let calls: Vec<(&str, Call)> = vec![
            ("State", Call::State),
            ("Event", Call::Event(EventId::from_bytes([4; 32]))),
            ("Events", Call::Events { from: 7 }),
            ("ItemEvents", Call::ItemEvents(item)),
            (
                "AgentEvents",
                Call::AgentEvents(AgentId {
                    principal: PrincipalSigner::generate([9; 32]).key(),
                    name: AgentName::new("claude").unwrap(),
                }),
            ),
            (
                "Author",
                Call::Author {
                    body: Box::new(comment(item, "why")),
                },
            ),
            ("SyncNow", Call::SyncNow),
            ("Fork", Call::Fork(Box::new(fork))),
            (
                "Join",
                Call::Join {
                    code: "wfi1abc".into(),
                    profile,
                },
            ),
            ("Follow", Call::Follow(vec![device])),
            ("LastSeen", Call::LastSeen),
            (
                "StartSession",
                Call::StartSession {
                    agent: AgentName::new("claude").unwrap(),
                    item,
                    repository: "/repo".into(),
                },
            ),
            (
                "EndSession",
                Call::EndSession {
                    finish: Finish::Submit {
                        note: Some("done".into()),
                    },
                },
            ),
            (
                "StopSession",
                Call::StopSession(SessionId::from_bytes([3; 32])),
            ),
            ("Permissions", Call::Permissions),
            (
                "AnswerPermission",
                Call::AnswerPermission {
                    session: SessionId::from_bytes([3; 32]),
                    since_ms: 5_000,
                    answer: Permission::Allow,
                },
            ),
            ("MachineFaults", Call::MachineFaults),
            ("CacheUsage", Call::CacheUsage),
        ];
        let hashes: Vec<(&str, usize, String)> = calls
            .into_iter()
            .map(|(name, call)| {
                let request = Request {
                    workforce: fixture.workforce(),
                    call,
                };
                let bytes = encoding::encode(&request).unwrap();
                (name, bytes.len(), EventId::of(&bytes).to_string())
            })
            .collect();
        let found: Vec<(&str, usize, &str)> = hashes
            .iter()
            .map(|(call, len, hash)| (*call, *len, hash.as_str()))
            .collect();
        assert_eq!(
            (PROTOCOL_VERSION, found),
            (GOLDEN_PROTOCOL_VERSION, GOLDEN_REQUESTS.to_vec()),
            "the encoded shape of a `Request` changed. Bump `PROTOCOL_VERSION`, then update \
             `GOLDEN_PROTOCOL_VERSION` and `GOLDEN_REQUESTS` here to what this test prints"
        );
    }

    const GOLDEN_REQUESTS: [(&str, usize, &str); 18] = [
        (
            "State",
            34,
            "rwod3o6kmzhclbg4vmggbkxubs3qxy4oikqsgxpzgnc2aidnnfva",
        ),
        (
            "Event",
            67,
            "vfmbc5hdevm24ohyxlhvwj4sqzciku3f6ww5nsfkzlxyxzmo4zra",
        ),
        (
            "Events",
            35,
            "szwf5qplrttkeszvqnyusj7tfrzoueuqjnwjd7cevkarhgbguxqq",
        ),
        (
            "ItemEvents",
            67,
            "cqs4u6vupdlgrelbvs555vut6q4fine6lcnbkug2xqben5ph4t3q",
        ),
        (
            "AgentEvents",
            74,
            "7at2zkvfhh5muitehip7wzqhxnofy2bqveep24x4cruyeadn53xa",
        ),
        (
            "Author",
            76,
            "sppriks3gifpm2x37rt7kjgoi7hurgekndouuncq2apu3ldurh2q",
        ),
        (
            "SyncNow",
            34,
            "v5luf6ogxl6t6zx7qbdbax7zaf7lb4c75k6f6kjvz66guaofr7ca",
        ),
        (
            "Fork",
            2150,
            "u4arqufcknq2l5qfnhibcm6e6edezeo4cyzarucab2ioayhr5nia",
        ),
        (
            "Join",
            73,
            "7dvbhhqk2b4kqgu56xvgmfnurqo6kgi7y5eulb3ncyp5ws3hezea",
        ),
        (
            "Follow",
            68,
            "7xuwmrhpsnbxwqiw4npsnym3m3vyh3fzvomekdw6qdgdezkvdnla",
        ),
        (
            "LastSeen",
            34,
            "4yzdxuhm7vmgtv2p6puua5dk5mo7m6y6dlankpwpumr2ldjorqra",
        ),
        (
            "StartSession",
            80,
            "3w56pkagoeq3kfp4dglbqsmaizplarxpyxe7nzra2qimc2isb7wq",
        ),
        (
            "EndSession",
            41,
            "cdl3goyy6actulwynoznk5xtr53zm4cxe2dth2dtwsljge4lkona",
        ),
        (
            "StopSession",
            67,
            "cawkc6w4asw2ngvwkvksmhjrryqbpiiktrlfwc5df5zdf2qjbotq",
        ),
        (
            "Permissions",
            34,
            "3ple4audlypkcdsfdz74z4qb5gowtksipkseo66b7wpeuz4qrciq",
        ),
        (
            "AnswerPermission",
            70,
            "buiv5v6wig2angyzvhla4v76j5rvxbyybu3yyavpgj5jdlh6nnia",
        ),
        (
            "MachineFaults",
            34,
            "yg5ntdbc727c4osihghi3qqxw6stmbov6frj6yngan535l2eem6a",
        ),
        (
            "CacheUsage",
            34,
            "ytabkzyokvnpzluoivm3fn5zmjkp3extlk6yvdx2lufejibufa6a",
        ),
    ];

    const GOLDEN_PROTOCOL_VERSION: u32 = 16;
    const GOLDEN_REPLIES: [(&str, usize, &str); 6] = [
        (
            "State",
            9257,
            "auxjzbopdblpqvaxempnrgr45f4ecnjhyhnyigek6yualjr74eiq",
        ),
        (
            "Permissions",
            70,
            "7qisyjr6gciokcbsilk3l2jk7slnlgqqrum4urcz47hx3c3kj5hq",
        ),
        (
            "SyncNow",
            97,
            "abj77avwgitaa6azyfeh3vz77lq5uvwz37oqwukwng4qpagtqz7a",
        ),
        (
            "MachineFaults",
            3,
            "sfjf74akg5k2rx4tyytllh3og3hqehmf5pgozxw4hdz7dcikcx6a",
        ),
        (
            "CacheUsage",
            4,
            "4jbsfkwgblpmpv53kehzpspv6karstuuedmhfclxlcsqy7rt4whq",
        ),
        (
            "failure",
            4,
            "qegpakx3omc3v743mvdau33u7m5upwr7iolhjkxzg3jk3fgeqfwq",
        ),
    ];
}

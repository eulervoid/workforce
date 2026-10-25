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
use wf_core::name::{AgentName, ReviewStepName};
use wf_core::session::CommitId;
use wf_harness::Permission;
use wf_net::DiffAnswer;
use wf_net::frame::{FrameError, blocking as frame};

use crate::caller::{Caller, Token};

pub const PROTOCOL_VERSION: u32 = 26;

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
/// (`State`, `Event`, `Events`, `ItemEvents`, `AgentEvents`, `Presence`, `Permissions`,
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
    /// When each device was last heard from and when the daemon started, as
    /// [`wf_core::claims::Presence`].
    Presence,
    /// Starts a session of `agent` on `item` by hand: a review
    /// of the named `step` (or of the only one open) when the item awaits a review, and a work
    /// session in the item's agent repository otherwise. Answered with its `SessionId`.
    StartSession {
        agent: AgentName,
        item: ItemId,
        step: Option<ReviewStepName>,
    },
    /// Ends the session the connection acts for after the agent's turn, as the agent says, and
    /// applies what follows from it to its item; answered with an [`EndAnswer`].
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
    /// What the caches in the agent repositories of the workforce took when the daemon last checked them
    /// against the budget of the machine config, as `Option<CacheUsage>`; `None` without a budget.
    CacheUsage,
    /// The work of an item as the device of its live work session shows it, for the principal;
    /// answered with a [`LiveDiff`]. `since` is the head of the last answer, which an unchanged
    /// head is answered with [`DiffAnswer::Unchanged`].
    LiveDiff {
        item: ItemId,
        since: Option<CommitId>,
    },
    /// The commit the target branch is at, for the session to rebase its work onto before it
    /// submits; answered with a `CommitId`.
    TargetCommit,
    /// The commits the log names that this device has not got yet, which what waits for them
    /// (a session, a review, a merge) waits for; answered with a `Vec<AwaitedCommit>`.
    AwaitedCommits,
}

/// What a [`Call::LiveDiff`] came to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LiveDiff {
    /// The device of the session answered, or this one did when the session runs here.
    Answer(DiffAnswer),
    /// The item has no live work session.
    NoSession,
    /// The device of the session could not be asked.
    Unreachable { device: DeviceKey, reason: String },
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

/// What an `EndSession` call came to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EndAnswer {
    /// The session ends after the agent's turn.
    Ended,
    /// A submission is checked first: the daemon runs the checks named here on the merge of the
    /// work, and the session goes on until they are done. Its next prompt tells which failed;
    /// when they pass, it ends as the agent said.
    Checking {
        checks: Vec<wf_core::name::CommandName>,
    },
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
    use wf_core::claims::Presence;
    use wf_core::id::EventId;
    use wf_core::identity::DeviceSigner;
    use wf_core::testing::{golden_forking_state, golden_state};

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
        let live_diffs = vec![
            LiveDiff::Answer(DiffAnswer::Work(wf_net::Work {
                target: "main".into(),
                head: CommitId::Git("0123456789abcdef0123456789abcdef01234567".parse().unwrap()),
                committed: "diff --git a/f b/f\n".into(),
            })),
            LiveDiff::NoSession,
            LiveDiff::Unreachable {
                device,
                reason: "timed out".into(),
            },
        ];
        let state = encoding::encode(&(golden_state(), golden_forking_state())).unwrap();
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
                "Presence",
                encoding::encode(&Reply::Ok(Presence::new(1_000, [(device, 2_000)]))).unwrap(),
            ),
            (
                "LiveDiff",
                encoding::encode(&Reply::Ok(live_diffs)).unwrap(),
            ),
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

    /// Names each `Call` variant for the golden requests. The match has no wildcard, so a new
    /// variant fails to compile here: add its arm, raise `CALL_VARIANTS` and add a request for
    /// it to `golden_request_bytes`.
    fn golden_name(call: &Call) -> &'static str {
        match call {
            Call::State => "State",
            Call::Event(_) => "Event",
            Call::Events { .. } => "Events",
            Call::ItemEvents(_) => "ItemEvents",
            Call::AgentEvents(_) => "AgentEvents",
            Call::Author { .. } => "Author",
            Call::SyncNow => "SyncNow",
            Call::Fork(_) => "Fork",
            Call::Join { .. } => "Join",
            Call::Follow(_) => "Follow",
            Call::Presence => "Presence",
            Call::StartSession { .. } => "StartSession",
            Call::EndSession { .. } => "EndSession",
            Call::StopSession(_) => "StopSession",
            Call::Permissions => "Permissions",
            Call::AnswerPermission { .. } => "AnswerPermission",
            Call::MachineFaults => "MachineFaults",
            Call::CacheUsage => "CacheUsage",
            Call::LiveDiff { .. } => "LiveDiff",
            Call::TargetCommit => "TargetCommit",
            Call::AwaitedCommits => "AwaitedCommits",
        }
    }

    /// The number of arms of [`golden_name`].
    const CALL_VARIANTS: usize = 21;

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
        let calls: Vec<Call> = vec![
            Call::State,
            Call::Event(EventId::from_bytes([4; 32])),
            Call::Events { from: 7 },
            Call::ItemEvents(item),
            Call::AgentEvents(AgentId {
                principal: PrincipalSigner::generate([9; 32]).key(),
                name: AgentName::new("claude").unwrap(),
            }),
            Call::Author {
                body: Box::new(comment(item, "why")),
            },
            Call::SyncNow,
            Call::Fork(Box::new(fork)),
            Call::Join {
                code: "wfi1abc".into(),
                profile,
            },
            Call::Follow(vec![device]),
            Call::Presence,
            Call::StartSession {
                agent: AgentName::new("claude").unwrap(),
                item,
                step: None,
            },
            Call::EndSession {
                finish: Finish::Submit {
                    note: Some("done".into()),
                },
            },
            Call::StopSession(SessionId::from_bytes([3; 32])),
            Call::Permissions,
            Call::AnswerPermission {
                session: SessionId::from_bytes([3; 32]),
                since_ms: 5_000,
                answer: Permission::Allow,
            },
            Call::MachineFaults,
            Call::CacheUsage,
            Call::LiveDiff {
                item,
                since: Some(CommitId::Git(
                    "0123456789abcdef0123456789abcdef01234567".parse().unwrap(),
                )),
            },
            Call::TargetCommit,
            Call::AwaitedCommits,
        ];
        assert_eq!(
            calls.len(),
            CALL_VARIANTS,
            "`Call` has a variant without a request in `golden_request_bytes`"
        );
        let names: std::collections::BTreeSet<_> = calls.iter().map(golden_name).collect();
        assert_eq!(
            names.len(),
            CALL_VARIANTS,
            "two requests are for the same call"
        );
        let hashes: Vec<(&str, usize, String)> = calls
            .into_iter()
            .map(|call| {
                let name = golden_name(&call);
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

    const GOLDEN_REQUESTS: [(&str, usize, &str); 21] = [
        (
            "State",
            34,
            "uun4zkpkslyfndl7i57iknzogv5tnxvsct4dn5f2i7rsge2il37a",
        ),
        (
            "Event",
            67,
            "7uabbxsropvzunejhftkt74rfwlbsd34hmo3vtsuryh4tjwb5dra",
        ),
        (
            "Events",
            35,
            "7d7qvmwvu2bhsxw5amrfbnwanrwx2qrcafd5dazpv5axk2vk5xfa",
        ),
        (
            "ItemEvents",
            67,
            "txuwcouz3fjf652kn6eftw43uzz2d7lfydgxqfsatib3dk5ee3ba",
        ),
        (
            "AgentEvents",
            74,
            "tidcz6k5zekiqlkpevrrpbcyreopk7kyph3wulic4zedryq3mdpq",
        ),
        (
            "Author",
            76,
            "vvitnzxq4ivnkblmb5tp3j5u35f6qe5532h3sgtrfkebxqbs7h4a",
        ),
        (
            "SyncNow",
            34,
            "q3vz6xr3ahof2uz4lzvyd5gub2dfs5qrdvq6jairwjwlun7tgaya",
        ),
        (
            "Fork",
            2114,
            "azcqvjj33zlhqwp6kuswsvikrvncvorarfdipubnhamygmtduntq",
        ),
        (
            "Join",
            73,
            "squn66qkhsamokc2knd5i22bjn7ep6dpclctgmke6yzvhh2rxm5q",
        ),
        (
            "Follow",
            68,
            "ecwm4akw6r4k4bgmtfmavll7yd5gs6qe5sbrsxc25bwc4gttceoq",
        ),
        (
            "Presence",
            34,
            "golkl4gskednp3cut32xd4n6uwakmtcymheyoatichlz2kej6xoa",
        ),
        (
            "StartSession",
            75,
            "ukqjthnmoaxq3qjltbxabki5fmmm75vymii4o5frb3diswpywozq",
        ),
        (
            "EndSession",
            41,
            "zmberfrlin2adwi33t2z27lzwsbd2ucl5xeqqitw6nwkz4c3l64q",
        ),
        (
            "StopSession",
            67,
            "yrti45qpkxfaizfpxxrzahxm6wxoeezz7melzxrkj7xwrcgjs7nq",
        ),
        (
            "Permissions",
            34,
            "2negz735oxc2izfjtgrc65kc23dznrsnbiz26d46y3bvbm7ftgyq",
        ),
        (
            "AnswerPermission",
            70,
            "3z73tbdl6246i7om57ko5wljiw3ymuyacoyuesdrbt53ou5gwv6q",
        ),
        (
            "MachineFaults",
            34,
            "lt5l2kayjva4da3n53albuk3ykr3hl5n2c2nw32xzioghqz7ti6a",
        ),
        (
            "CacheUsage",
            34,
            "7hj3azs6rsep2vg4noxpxwwe6m7hqw7r7f6onf6hxnewsb2isrna",
        ),
        (
            "LiveDiff",
            90,
            "toldep4z7fw5kc3t2j5zq3atdx5mxqrghkbulnwd6kq564bsasga",
        ),
        (
            "TargetCommit",
            34,
            "cmgxhicxtrff6finessdexo5yzwek5jmgkga335q3w2a5idgtp7a",
        ),
        (
            "AwaitedCommits",
            34,
            "5msmqw2c7yzuofh4s4bpco4hbgk6nkb2ld6ybzmawmg25sbtdomq",
        ),
    ];

    const GOLDEN_PROTOCOL_VERSION: u32 = 26;
    const GOLDEN_REPLIES: [(&str, usize, &str); 8] = [
        (
            "State",
            24499,
            "a7a6siegwlfg7lnbdw6wjttic4lslviiuers7mcpqpsam66aegqa",
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
            "Presence",
            39,
            "ihkffobvfi4c55lxe4x6fzf6hoef42zffuv4seleatghl3tyubya",
        ),
        (
            "LiveDiff",
            96,
            "ehkn7afzf5poajq3wkx5kagr5zpy4c5ltaovawkaeypjm4lx2u5a",
        ),
        (
            "failure",
            4,
            "qegpakx3omc3v743mvdau33u7m5upwr7iolhjkxzg3jk3fgeqfwq",
        ),
    ];
}

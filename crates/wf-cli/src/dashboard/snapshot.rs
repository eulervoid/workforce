//! What the dashboard shows at one refresh, taken from the workspace.

use wf_core::event::{Event, EventBody};
use wf_core::graph::Readiness;
use wf_core::id::{CommentId, EventId, HashId, ItemId, SessionId};
use wf_core::identity::{Actor, DeviceKey};
use wf_core::inbox::{InboxKind, Waiting as InboxWaiting};
use wf_core::item::ItemView;
use wf_core::policy::StateCategory;
use wf_core::session::{CommitId, Session, SessionRole};
use wf_core::state::State;
use wf_daemon::proto::HeldPermission;

use crate::{render, view};

/// Everything the dashboard draws besides transcripts, as plain data: the loop takes one at each
/// refresh, and tests build them by hand.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub name: String,
    /// Milliseconds since the Unix epoch, when the snapshot was taken.
    pub now_ms: u64,
    pub counts: Counts,
    /// What waits on the principal, as `wf inbox` lists it.
    pub waiting: Vec<Waiting>,
    /// The live sessions, the newest first.
    pub working: Vec<SessionInfo>,
    /// What `wf status` flags.
    pub flagged: Vec<String>,
    /// The latest event, as `wf watch` prints it.
    pub latest: Option<String>,
    /// What the detail on screen shows.
    pub in_view: Option<InView>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub live_sessions: usize,
    pub open_items: usize,
    pub blocked_items: usize,
    pub devices: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemRef {
    pub id: ItemId,
    pub title: String,
}

/// One thing that waits on the principal, with what it takes to answer it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Waiting {
    /// A session of this machine asks to do something.
    Permission {
        session: SessionId,
        /// When it asked, which tells two requests of a session apart.
        since_ms: u64,
        agent: String,
        item: Option<ItemRef>,
        what: String,
    },
    Blocker {
        item: ItemRef,
        comment: CommentId,
        by: String,
        text: String,
    },
    Review {
        item: ItemRef,
        commit: Option<CommitId>,
    },
    /// A proposal, a merge or a recovery step: answered with commands for now.
    Notice {
        item: ItemRef,
        kind: InboxKind,
        what: String,
    },
}

/// What tells an entry that waits apart from the others, across refreshes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WaitingKey {
    Permission { session: SessionId, since_ms: u64 },
    Blocker(CommentId),
    Item { item: ItemId, kind: InboxKind },
}

impl Waiting {
    pub fn key(&self) -> WaitingKey {
        match self {
            Waiting::Permission {
                session, since_ms, ..
            } => WaitingKey::Permission {
                session: *session,
                since_ms: *since_ms,
            },
            Waiting::Blocker { comment, .. } => WaitingKey::Blocker(*comment),
            Waiting::Review { item, .. } => WaitingKey::Item {
                item: item.id,
                kind: InboxKind::PrincipalReview,
            },
            Waiting::Notice { item, kind, .. } => WaitingKey::Item {
                item: item.id,
                kind: *kind,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionStatus {
    Live,
    /// Live, but its item waits on a dependency or an open blocker.
    Parked,
    Ended,
}

/// Where a session runs, which tells whether its transcript can be read here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Place {
    Here,
    /// On another device, named.
    Elsewhere(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: SessionId,
    pub agent: String,
    /// `work`, `review`, `supervise` or `diagnose`.
    pub role: &'static str,
    pub item: ItemRef,
    pub status: SessionStatus,
    pub started_ms: u64,
    pub place: Place,
}

/// What the detail on screen shows, besides a transcript.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InView {
    /// The item as `wf item show` prints it.
    Item {
        id: ItemId,
        text: String,
    },
    Session(SessionInfo),
}

/// What the dashboard asks the workspace about, besides what it always shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wanted {
    Item(ItemId),
    Session(SessionId),
}

/// What a snapshot is taken from.
pub struct Sources<'a> {
    pub state: &'a State,
    pub events: &'a [Event],
    pub held: &'a [HeldPermission],
    pub own: DeviceKey,
    pub last_seen: &'a crate::devices::LastSeen,
    pub now_ms: u64,
    /// How many columns the text of a detail has.
    pub detail_width: u16,
}

impl Snapshot {
    pub fn take(sources: &Sources, wanted: Option<Wanted>) -> Self {
        let Sources {
            state,
            events,
            held,
            own,
            last_seen,
            now_ms,
            detail_width,
        } = *sources;
        let waiting = waiting(state, events, held);
        let context = view::StatusContext {
            own: Some(own),
            last_seen,
            daemon: true,
            waiting_on_you: waiting.len(),
            refolded: false,
        };
        let latest = events.last().and_then(|event| {
            view::watch_lines(state, events, std::slice::from_ref(event))
                .last()
                .map(render::watch_line)
        });
        let graph = state.dependency_graph();
        let mut live: Vec<&Session> = state.live_sessions().collect();
        live.sort_by_key(|session| std::cmp::Reverse(session.started()));
        Snapshot {
            name: state.name().unwrap_or("workforce").to_owned(),
            now_ms,
            counts: counts(state),
            waiting,
            working: live
                .into_iter()
                .map(|session| session_info(state, &graph, own, session))
                .collect(),
            flagged: view::status(state, context).flagged,
            latest,
            in_view: wanted
                .and_then(|wanted| in_view(state, events, &graph, own, wanted, detail_width)),
        }
    }
}

fn counts(state: &State) -> Counts {
    let policy = &state.policy().expect("a workforce has a policy").policy;
    let graph = state.dependency_graph();
    let mut counts = Counts {
        live_sessions: state.live_sessions().count(),
        devices: state.devices().count(),
        ..Counts::default()
    };
    for item in state.live_items() {
        if item.standing(policy).state_category() == Some(StateCategory::Open) {
            counts.open_items += 1;
        }
        if state.readiness(&graph, item) == Some(Readiness::Blocked) {
            counts.blocked_items += 1;
        }
    }
    counts
}

fn item_ref(state: &State, id: ItemId) -> ItemRef {
    ItemRef {
        id,
        title: state.inbox_title(id),
    }
}

/// `coder` for an agent, `@ada` for a principal.
fn who(state: &State, actor: &Actor) -> String {
    match actor {
        Actor::Agent { agent, .. } => agent.name.to_string(),
        Actor::Principal(_) | Actor::Daemon(_) => view::actor_label(state, actor),
    }
}

fn waiting(state: &State, events: &[Event], held: &[HeldPermission]) -> Vec<Waiting> {
    let mut waiting: Vec<Waiting> = held
        .iter()
        .map(|request| {
            let session = state.session(request.session);
            Waiting::Permission {
                session: request.session,
                since_ms: request.since_ms,
                agent: session.map_or_else(
                    || "an agent".to_owned(),
                    |session| session.agent().name.to_string(),
                ),
                item: session.map(|session| item_ref(state, session.item())),
                what: request.what.clone(),
            }
        })
        .collect();
    for entry in state.inbox() {
        let item = item_ref(state, entry.item);
        waiting.push(match entry.waiting {
            InboxWaiting::Blocker { comment } => {
                let id = EventId::from(comment);
                let (by, text) = events
                    .iter()
                    .rev()
                    .find(|event| event.id() == id)
                    .map(|event| match &event.content.body {
                        EventBody::Commented(blocker) => {
                            (who(state, &event.content.actor), blocker.text.clone())
                        }
                        _ => (who(state, &event.content.actor), String::new()),
                    })
                    .unwrap_or_default();
                Waiting::Blocker {
                    item,
                    comment,
                    by,
                    text,
                }
            }
            InboxWaiting::PrincipalReview { commit } => Waiting::Review { item, commit },
            InboxWaiting::Proposal | InboxWaiting::Merge { .. } | InboxWaiting::Recovery { .. } => {
                Waiting::Notice {
                    kind: entry.waiting.kind(),
                    what: state.what_waits(&entry),
                    item,
                }
            }
        });
    }
    waiting
}

fn role_name(role: SessionRole) -> &'static str {
    match role {
        SessionRole::Work => "work",
        SessionRole::Review { .. } => "review",
        SessionRole::Supervise => "supervise",
        SessionRole::Diagnose => "diagnose",
    }
}

fn session_info(
    state: &State,
    graph: &wf_core::graph::DependencyGraph,
    own: DeviceKey,
    session: &Session,
) -> SessionInfo {
    let status = if !session.is_live() {
        SessionStatus::Ended
    } else if view::is_parked(state, graph, session) {
        SessionStatus::Parked
    } else {
        SessionStatus::Live
    };
    let place = if session.device() == own {
        Place::Here
    } else {
        let name = state
            .devices()
            .find(|info| info.device == session.device())
            .and_then(|info| info.profile.map(|profile| profile.name.clone()))
            .unwrap_or_else(|| session.device().fmt_short());
        Place::Elsewhere(name)
    };
    SessionInfo {
        id: session.id(),
        agent: session.agent().name.to_string(),
        role: role_name(session.role()),
        item: item_ref(state, session.item()),
        status,
        started_ms: session.started().hlc.wall_ms(),
        place,
    }
}

fn in_view(
    state: &State,
    events: &[Event],
    graph: &wf_core::graph::DependencyGraph,
    own: DeviceKey,
    wanted: Wanted,
    width: u16,
) -> Option<InView> {
    match wanted {
        Wanted::Item(id) => {
            let item = state.item(id)?;
            let text = match item {
                ItemView::Live(live) => render::item_detail_for(
                    &view::item_detail(state, live, events),
                    Some(usize::from(width).min(100)),
                ),
                ItemView::Archived(_) => format!("#{} {} (archived)\n", id.short(), item.title()),
            };
            Some(InView::Item { id, text })
        }
        Wanted::Session(id) => {
            let session = state.session(id)?;
            Some(InView::Session(session_info(state, graph, own, session)))
        }
    }
}

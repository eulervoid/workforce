//! What the dashboard shows at one refresh, taken from the workspace.

use std::collections::{HashMap, HashSet};

use wf_core::event::{Event, EventBody};
use wf_core::graph::Readiness;
use wf_core::id::{CommentId, EventId, HashId, ItemId, SessionId};
use wf_core::identity::AgentId;
use wf_core::identity::{Actor, DeviceKey};
use wf_core::inbox::{InboxKind, Waiting as InboxWaiting};
use wf_core::item::{ItemState, ItemView, Priority};
use wf_core::resolve::MentionTarget;
use wf_core::session::{CommitId, EndReason, Session, SessionRole};
use wf_core::state::State;
use wf_core::stats::{AgentStats, ProjectStats};
use wf_daemon::proto::HeldPermission;

use super::diff::{LineComment, PostedComment};
use crate::cli::{ItemGrouping, ItemOrder};
use crate::commands::sorted_views;
use crate::transcript::Format;
use crate::view::Flag;
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
    pub flagged: Vec<Flag>,
    /// What the detail on screen shows.
    pub in_view: Option<InView>,
    /// The live items in groups, when the Items tab is on screen.
    pub items: Vec<ItemGroup>,
    /// The agents, when the Agents tab is on screen.
    pub agents: Vec<AgentInfo>,
    /// How the workforce is doing, when the Agents tab is on screen.
    pub stats: ProjectStats,
    pub links: Links,
}

/// What the ids and names written in the text on screen point at, by how they are written.
#[derive(Clone, Debug, Default)]
pub struct Links {
    /// Every item, by its short id: `k3f9ab`.
    pub items: HashMap<String, ItemId>,
    /// The comments of every live item, by the item and the comment's short id: `k3f9`.
    pub comments: HashSet<(ItemId, String)>,
    /// Every agent, by its label: `@ada/grace`.
    pub agents: HashMap<String, AgentId>,
    /// Every session, by its label: `@ada/grace/s4k2ab`.
    pub sessions: HashMap<String, SessionId>,
}

impl Links {
    fn take(state: &State) -> Self {
        Links {
            items: state
                .items()
                .map(|item| (item.id().short(), item.id()))
                .collect(),
            comments: state
                .live_items()
                .flat_map(|item| {
                    item.comments()
                        .keys()
                        .map(|comment| (item.id(), comment.short()))
                })
                .collect(),
            agents: state
                .agents()
                .map(|(id, _)| {
                    let label = view::mention_label(state, &MentionTarget::Agent(id.clone()));
                    (label, id.clone())
                })
                .collect(),
            sessions: state
                .sessions()
                .map(|session| (state.actor_label(&session.actor()), session.id()))
                .collect(),
        }
    }
}

/// How many sessions of an agent that ended are listed under it.
const ENDED_SESSIONS: usize = 5;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentInfo {
    pub id: AgentId,
    /// `claude-code · opus`
    pub runs_on: String,
    /// How many sessions this machine runs of it at once; `None` when it does not run it.
    pub capacity: Option<u32>,
    pub stats: AgentStats,
    /// Its live sessions, then the latest that ended.
    pub sessions: Vec<SessionInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemGroup {
    pub name: String,
    pub items: Vec<ItemRow>,
}

/// An item in a list, as `wf item list` shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemRow {
    pub id: ItemId,
    pub title: String,
    pub state: ItemState,
    pub priority: Option<Priority>,
    /// `None` unless the item is open.
    pub readiness: Option<Readiness>,
    /// Steps checked off, and all steps.
    pub steps: Option<(usize, usize)>,
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
    Ended {
        reason: EndReason,
        /// Milliseconds since the Unix epoch.
        at_ms: u64,
    },
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
    /// The definition of an agent and how it is doing.
    Agent {
        id: AgentId,
        text: String,
    },
    /// The item as `wf item show` prints it.
    Item {
        id: ItemId,
        text: String,
        /// The session `enter` opens: the live one, else the latest.
        session: Option<SessionId>,
    },
    Session(SessionInfo),
    /// What a diff shows besides the diff, which the loop reads from git: the comments on its
    /// lines in the log.
    Diff {
        id: ItemId,
        comments: Vec<PostedComment>,
    },
    /// A flag that is not about an item, in the notice box of an item.
    Flag {
        flag: Flag,
        text: String,
    },
}

/// What the dashboard asks the workspace about, besides what it always shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Wanted {
    pub detail: Option<Detail>,
    /// The items, listed so.
    pub items: Option<ItemListing>,
    /// The agents and the stats.
    pub agents: bool,
}

/// How the items are listed: `wf item list --group-by --sort`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ItemListing {
    pub grouping: ItemGrouping,
    pub order: ItemOrder,
}

/// What a detail shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Detail {
    Item(ItemId),
    Session(SessionId),
    /// The work of an item against its target, read from git by the loop rather than taken
    /// with the snapshot.
    Diff(ItemId),
    Agent(AgentId),
    /// A flag of `wf status` that is not about an item.
    Flag(Flag),
}

/// What a snapshot or a detail is taken from.
pub struct Sources<'a> {
    pub state: &'a State,
    pub events: &'a [Event],
    pub own: DeviceKey,
    pub now_ms: u64,
    /// How many columns the text of a detail has.
    pub detail_width: u16,
}

impl Snapshot {
    /// Takes all the dashboard shows of the workforce; `held` are the permission requests the
    /// daemon holds, and `last_seen` when it last heard from each device.
    pub fn take(
        sources: &Sources,
        held: &[HeldPermission],
        last_seen: &crate::devices::LastSeen,
        wanted: Wanted,
    ) -> Self {
        let Sources {
            state,
            events,
            own,
            now_ms,
            detail_width: _,
        } = *sources;
        let waiting = waiting(state, events, held);
        let context = view::StatusContext {
            // Only `flagged` is used below, which no machine fault changes; `next` is not shown.
            machine_faults: &wf_daemon::MachineFaults::default(),
            own: Some(own),
            last_seen,
            daemon: true,
            waiting_on_you: waiting.len(),
            refolded: false,
            // Measuring them walks the caches, too slow for every frame.
            caches: None,
        };
        let graph = state.dependency_graph();
        let mut live: Vec<&Session> = state.live_sessions().collect();
        live.sort_by_key(|session| std::cmp::Reverse(session.started()));
        let flagged = view::status(state, context).flagged;
        Snapshot {
            name: state.name().unwrap_or("workforce").to_owned(),
            now_ms,
            counts: counts(state),
            waiting,
            working: live
                .into_iter()
                .map(|session| session_info(state, &graph, own, session))
                .collect(),
            in_view: wanted
                .detail
                .clone()
                .and_then(|wanted| InView::take(sources, &graph, &flagged, wanted)),
            flagged,
            agents: if wanted.agents {
                agents(state, &graph, own)
            } else {
                Vec::new()
            },
            stats: if wanted.agents {
                state.project_stats(now_ms)
            } else {
                ProjectStats::default()
            },
            items: wanted
                .items
                .map(|listing| item_groups(state, &graph, listing))
                .unwrap_or_default(),
            links: Links::take(state),
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
        if item.state(policy) == ItemState::Open {
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
        Actor::Principal(_) | Actor::Daemon(_) => state.actor_label(actor),
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
                    .map(|event| {
                        // A blocker is a comment; anything else has no text to quote.
                        let text = if let EventBody::Commented(blocker) = &event.content.body {
                            blocker.text.clone()
                        } else {
                            String::new()
                        };
                        (who(state, &event.content.actor), text)
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
    let status = if let Some(ended) = session.ended() {
        SessionStatus::Ended {
            reason: ended.reason,
            at_ms: ended.at().wall_ms(),
        }
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

impl InView {
    /// Takes what `wanted` shows; `flagged` is what `wf status` flags.
    pub fn take(
        sources: &Sources,
        graph: &wf_core::graph::DependencyGraph,
        flagged: &[Flag],
        wanted: Detail,
    ) -> Option<InView> {
        let Sources {
            state,
            events,
            own,
            now_ms,
            detail_width: width,
        } = *sources;
        match wanted {
            Detail::Item(id) => {
                let item = state.item(id)?;
                let text = match item {
                    ItemView::Live(live) => render::item_detail_for(
                        &view::item_detail(state, live, events),
                        Some(usize::from(width).min(100)),
                        Format::Styled,
                    ),
                    ItemView::Archived(_) => {
                        format!("#{} {} (archived)\n", id.short(), item.title())
                    }
                };
                let mut sessions: Vec<&Session> = state
                    .sessions()
                    .filter(|session| session.item() == id)
                    .collect();
                sessions.sort_by_key(|session| (session.is_live(), session.started()));
                let session = sessions.last().map(|session| session.id());
                Some(InView::Item { id, text, session })
            }
            Detail::Session(id) => {
                let session = state.session(id)?;
                Some(InView::Session(session_info(state, graph, own, session)))
            }
            Detail::Agent(id) => {
                let detail = view::agent_detail(state, &id, events, Some(own))?;
                let now = jiff::Timestamp::from_millisecond(i64::try_from(now_ms).ok()?).ok()?;
                Some(InView::Agent {
                    text: render::agent_detail_for(
                        &detail,
                        Some(usize::from(width).min(100)),
                        Format::Styled,
                        now,
                    ),
                    id,
                })
            }
            Detail::Diff(id) => {
                let ItemView::Live(item) = state.item(id)? else {
                    return None;
                };
                let comments = events
                    .iter()
                    .filter_map(|event| {
                        let EventBody::Commented(comment) = &event.content.body else {
                            return None;
                        };
                        let lines = comment.lines.clone().filter(|_| comment.item == id)?;
                        let resolved = item
                            .comments()
                            .get(&CommentId::from(event.id()))
                            .is_some_and(|folded| folded.resolved().is_some());
                        Some(PostedComment {
                            by: who(state, &event.content.actor),
                            comment: LineComment {
                                kind: comment.kind,
                                lines,
                                text: comment.text.clone(),
                            },
                            resolved,
                        })
                    })
                    .collect();
                Some(InView::Diff { id, comments })
            }
            // Gone once fixed.
            Detail::Flag(flag) => flagged.contains(&flag).then(|| InView::Flag {
                text: render::flag_detail_for(&flag, Some(usize::from(width).min(100))),
                flag,
            }),
        }
    }
}

/// The agents, each with its live sessions and the latest that ended.
fn agents(
    state: &State,
    graph: &wf_core::graph::DependencyGraph,
    own: DeviceKey,
) -> Vec<AgentInfo> {
    let hosted = state
        .devices()
        .find(|info| info.device == own)
        .and_then(|info| info.profile)
        .map(|profile| profile.agents.clone())
        .unwrap_or_default();
    state
        .agents()
        .map(|(id, definition)| {
            let mut sessions: Vec<&Session> = state
                .sessions()
                .filter(|session| session.agent() == id)
                .collect();
            sessions.sort_by_key(|session| (session.is_live(), session.started()));
            sessions.reverse();
            let live = sessions.iter().filter(|session| session.is_live()).count();
            AgentInfo {
                id: id.clone(),
                runs_on: match &definition.model {
                    Some(model) => format!("{} · {model}", definition.harness),
                    None => definition.harness.to_string(),
                },
                capacity: hosted.get(&id.name).copied(),
                stats: state.agent_stats(id),
                sessions: sessions
                    .into_iter()
                    .take(live + ENDED_SESSIONS)
                    .map(|session| session_info(state, graph, own, session))
                    .collect(),
            }
        })
        .collect()
}

fn item_groups(
    state: &State,
    graph: &wf_core::graph::DependencyGraph,
    listing: ItemListing,
) -> Vec<ItemGroup> {
    let entries: Vec<_> = sorted_views(state, false, listing.order)
        .into_iter()
        .map(|view| (view, view::summary(state, graph, view)))
        .collect();
    view::group_items(listing.grouping, entries)
        .into_iter()
        .map(|group| ItemGroup {
            name: group.group,
            items: group
                .items
                .into_iter()
                .filter_map(|summary| {
                    Some(ItemRow {
                        id: summary.id.parse().ok()?,
                        title: summary.title,
                        state: summary.state,
                        priority: summary.priority,
                        readiness: summary.readiness,
                        steps: summary.steps.map(|steps| (steps.done, steps.total)),
                    })
                })
                .collect(),
        })
        .collect()
}

/// Work the daemon moved from `review` back to `open`, as when approved work conflicts with the
/// target: the principal did not make the move, and may miss it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MovedBack {
    pub item: ItemId,
    /// The first sentence of the comment the move names as its reason.
    pub reason: String,
}

/// The moves back among `events[known..]`, read against the events before them: those the daemon
/// made from `review` to `open` naming a comment that says why.
pub fn moves_back(events: &[Event], known: usize) -> Vec<MovedBack> {
    let mut moves = Vec::new();
    for (index, event) in events.iter().enumerate().skip(known) {
        let EventBody::ItemMoved(moved) = &event.content.body else {
            continue;
        };
        let (Actor::Daemon(_), ItemState::Open, Some(because)) =
            (&event.content.actor, moved.to.state(), moved.because)
        else {
            continue;
        };
        let before = &events[..index];
        let from = before.iter().rev().find_map(|earlier| {
            if let EventBody::ItemMoved(earlier) = &earlier.content.body
                && earlier.item == moved.item
            {
                Some(earlier.to.state())
            } else {
                None
            }
        });
        let reason = before.iter().rev().find_map(|earlier| {
            if let EventBody::Commented(comment) = &earlier.content.body
                && CommentId::from(earlier.id()) == because
            {
                Some(first_sentence(&comment.text))
            } else {
                None
            }
        });
        if from == Some(ItemState::Review)
            && let Some(reason) = reason
        {
            moves.push(MovedBack {
                item: moved.item,
                reason,
            });
        }
    }
    moves
}

/// The first sentence of `text`, without its full stop, to go after a colon: `Merging into main
/// conflicts in a.rs. The next session…` is `merging into main conflicts in a.rs`.
fn first_sentence(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    let sentence = line.split_once(". ").map_or(line, |(first, _)| first);
    let sentence = sentence.trim().trim_end_matches('.');
    // A capital starts the sentence, unless the word is all capitals: `README`.
    let mut characters = sentence.chars();
    match (characters.next(), characters.next()) {
        (Some(first), Some(second)) if second.is_lowercase() => {
            format!("{}{}", first.to_lowercase(), &sentence[first.len_utf8()..])
        }
        _ => sentence.to_owned(),
    }
}

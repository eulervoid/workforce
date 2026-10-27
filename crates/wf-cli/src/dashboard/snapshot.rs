//! What the dashboard shows at one refresh, taken from the workspace.

use std::collections::{BTreeSet, HashMap, HashSet};

use wf_core::event::{Event, EventBody};
use wf_core::graph::{DependencyGraph, Readiness};
use wf_core::id::{CommentId, EventId, HashId, ItemId, SessionId};
use wf_core::identity::AgentId;
use wf_core::identity::{Actor, DeviceKey};
use wf_core::inbox::{InboxKind, PrincipalReviewReason, Stale, Waiting as InboxWaiting};
use wf_core::item::{ItemEntry, ItemState};
use wf_core::name::AgentName;
use wf_core::resolve::MentionTarget;
use wf_core::search::{Query, SearchScope, search};
use wf_core::session::{CommitId, Session};
use wf_core::state::State;
use wf_core::stats::{AgentStats, ProjectStats};
use wf_daemon::proto::HeldPermission;

use super::app::{Request, Scroll};
use super::diff::{LineComment, PostedComment};
use crate::cli::{ItemGrouping, ItemOrder};
use crate::commands::sorted_views;
use crate::transcript::Format;
use crate::view::{Flag, ItemGroup, ItemRefView};
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
    pub waiting: Vec<InboxEntry>,
    /// The live sessions, the newest first.
    pub working: Vec<SessionInfo>,
    /// What `wf status` flags.
    pub flagged: Vec<Flag>,
    /// The other devices, with whether each is there.
    pub devices: Vec<view::DeviceStatus>,
    /// What waits for a commit that has not arrived from another device, one line each.
    pub awaiting_commits: Vec<String>,
    /// How the target branch of the principal's checkout differs from wf's, if it does.
    pub checkout_target: Option<view::CheckoutTarget>,
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
    pub view: view::AgentView,
    /// How many sessions this machine runs of it at once; `None` when it does not run it.
    pub capacity: Option<u32>,
    pub stats: AgentStats,
    /// Its live sessions, then the latest that ended.
    pub sessions: Vec<SessionInfo>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub live_sessions: usize,
    pub open_items: usize,
    pub blocked_items: usize,
    pub devices: usize,
}

/// One thing that waits on the principal, with what it takes to answer it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InboxEntry {
    /// A session of this machine asks to do something.
    Permission {
        session: SessionId,
        /// When it asked, which tells two requests of a session apart.
        since_ms: u64,
        agent: String,
        item: Option<ItemRefView>,
        what: String,
    },
    Blocker {
        item: ItemRefView,
        comment: CommentId,
        by: String,
        text: String,
    },
    Review {
        item: ItemRefView,
        commit: Option<CommitId>,
        /// The branch stopped merging into the target after the review was offered.
        stale: Option<Stale>,
        reason: PrincipalReviewReason,
    },
    /// A proposal, a merge or a recovery step: answered with commands for now.
    Notice {
        item: ItemRefView,
        kind: InboxKind,
        what: String,
    },
    /// Something wrong with the machine: what it is and what the principal does about it.
    MachineFault { title: String, what: String },
    /// A push of the principal that lost a fork of the target, and what to do about it.
    LostPush {
        commit: CommitId,
        title: String,
        what: String,
    },
}

/// What tells an entry that waits apart from the others, across refreshes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InboxKey {
    Permission {
        session: SessionId,
        since_ms: u64,
    },
    Blocker(CommentId),
    Item {
        item: ItemId,
        kind: InboxKind,
    },
    /// The title tells machine faults apart: the machine has one of each.
    MachineFault(String),
    LostPush(CommitId),
}

impl InboxEntry {
    pub fn key(&self) -> InboxKey {
        match self {
            InboxEntry::Permission {
                session, since_ms, ..
            } => InboxKey::Permission {
                session: *session,
                since_ms: *since_ms,
            },
            InboxEntry::Blocker { comment, .. } => InboxKey::Blocker(*comment),
            InboxEntry::Review { item, .. } => InboxKey::Item {
                item: item.id,
                kind: InboxKind::PrincipalReview,
            },
            InboxEntry::Notice { item, kind, .. } => InboxKey::Item {
                item: item.id,
                kind: *kind,
            },
            InboxEntry::MachineFault { title, .. } => InboxKey::MachineFault(title.clone()),
            InboxEntry::LostPush { commit, .. } => InboxKey::LostPush(*commit),
        }
    }
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
    pub view: view::SessionView,
    /// The name of its agent, which the rows show where the view has the label.
    pub agent_name: AgentName,
    pub place: Place,
}

impl std::ops::Deref for SessionInfo {
    type Target = view::SessionView;

    fn deref(&self) -> &view::SessionView {
        &self.view
    }
}

/// What the detail on screen shows, besides a transcript.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InView {
    /// The definition of an agent and how it is doing.
    Agent { id: AgentId, text: String },
    /// The item as `wf item show` prints it.
    Item {
        id: ItemId,
        text: String,
        /// The session `enter` opens: the live one, else the latest.
        session: Option<SessionId>,
    },
    /// Boxed, as a session is several times larger than what the other variants hold.
    Session {
        info: Box<SessionInfo>,
        /// Once it ended, the session its item went on in: the one that took over its claim
        /// when it ended `LeaseExpired` or `Outclaimed` ([`State::taken_over_by`]), else the one
        /// `enter` opens from the item now, when that is another: the live one, else the latest.
        successor: Option<Box<SessionInfo>>,
    },
    /// What a diff shows besides the diff, which the loop reads from git: the comments on its
    /// lines in the log.
    Diff {
        id: ItemId,
        comments: Vec<PostedComment>,
    },
    /// A flag that is not about an item, in the notice box of an item.
    Flag { flag: Flag, text: String },
}

impl InView {
    /// Whether this is what `detail` shows.
    pub fn is_of(&self, detail: &Detail) -> bool {
        match (self, detail) {
            (Self::Item { id, .. }, Detail::Item(wanted)) => id == wanted,
            (Self::Session { info, .. }, Detail::Session(wanted)) => info.view.id == *wanted,
            (Self::Diff { id, .. }, Detail::Diff(wanted)) => id == wanted,
            (Self::Agent { id, .. }, Detail::Agent(wanted)) => id == wanted,
            (Self::Flag { flag, .. }, Detail::Flag(wanted)) => flag == wanted,
            (
                Self::Item { .. }
                | Self::Session { .. }
                | Self::Diff { .. }
                | Self::Agent { .. }
                | Self::Flag { .. },
                _,
            ) => false,
        }
    }
}

/// What the dashboard asks the workspace about, besides what it always shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Wanted {
    pub detail: Option<Detail>,
    /// The list of the tab on screen, when that is one the workspace is asked to read.
    pub listing: Option<Listing>,
}

/// A list read from the workspace for the tab on screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Listing {
    /// The items, listed so.
    Items(ItemListing),
    /// The agents and the stats.
    Agents,
}

/// How the items are listed: `wf item list --group-by --sort`, or `wf item search` when there
/// is something to search for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemListing {
    pub grouping: ItemGrouping,
    pub order: ItemOrder,
    /// Only the items with all of its words are listed.
    pub search: Option<Query>,
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
    /// The dependencies of `state`, built once for everything taken from it.
    pub graph: &'a DependencyGraph,
    /// The events of the detail wanted: for an item or its diff, those that can show in the
    /// activity of the item; for an agent, those that defined or removed it; none for another.
    pub detail_events: &'a [Event],
    /// The text of each comment, while the items are searched; empty otherwise.
    pub comment_texts: &'a HashMap<CommentId, String>,
    /// Looks an event up by its id, without going through the log.
    pub event: &'a dyn Fn(EventId) -> Option<Event>,
    pub own: DeviceKey,
    pub now_ms: u64,
    /// How many columns the text of a detail has.
    pub detail_width: u16,
}

impl Snapshot {
    /// Takes all the dashboard shows of the workforce; `held` are the permission requests the
    /// daemon holds, and `presence` what it knows of which devices are there.
    pub fn take(
        sources: &Sources,
        held: &[HeldPermission],
        machine_faults: &wf_daemon::MachineFaults,
        awaited: &[wf_core::commits::AwaitedCommit],
        presence: Option<&wf_core::claims::Presence>,
        checkout_target: Option<view::CheckoutTarget>,
        wanted: Wanted,
    ) -> Self {
        let Sources {
            state,
            graph,
            detail_events: _,
            comment_texts: _,
            event,
            own,
            now_ms,
            detail_width: _,
        } = *sources;
        let waiting = waiting(state, own, event, held, machine_faults);
        let context = view::StatusContext {
            machine_faults,
            awaited,
            own: Some(own),
            presence,
            now_ms,
            daemon: true,
            waiting_on_you: waiting.len(),
            refolded: false,
            // Measuring them walks the caches, too slow for every frame.
            caches: None,
            checkout_target: checkout_target.clone(),
        };
        let mut live: Vec<&Session> = state.live_sessions().collect();
        live.sort_by_key(|session| std::cmp::Reverse(session.started()));
        let status = view::status(state, graph, context);
        let flagged = status.flagged;
        let devices = status
            .devices
            .into_iter()
            .filter(|device| device.presence != view::DevicePresence::ThisDevice)
            .collect();
        Snapshot {
            name: state.name().unwrap_or("workforce").to_owned(),
            now_ms,
            counts: counts(state, graph),
            waiting,
            working: live
                .into_iter()
                .map(|session| session_info(state, graph, own, session))
                .collect(),
            in_view: wanted
                .detail
                .clone()
                .and_then(|wanted| InView::take(sources, &flagged, wanted)),
            flagged,
            awaiting_commits: view::awaiting_lines(state, awaited),
            devices,
            checkout_target,
            agents: match wanted.listing {
                Some(Listing::Agents) => agents(state, graph, own),
                Some(Listing::Items(_)) | None => Vec::new(),
            },
            stats: match wanted.listing {
                Some(Listing::Agents) => state.project_stats(now_ms),
                Some(Listing::Items(_)) | None => ProjectStats::default(),
            },
            items: match wanted.listing {
                Some(Listing::Items(listing)) => item_groups(sources, listing),
                Some(Listing::Agents) | None => Vec::new(),
            },
            links: Links::take(state),
        }
    }
}

fn counts(state: &State, graph: &DependencyGraph) -> Counts {
    let mut counts = Counts {
        live_sessions: state.live_sessions().count(),
        devices: state.devices().count(),
        ..Counts::default()
    };
    for item in state.live_items() {
        if item.state() == ItemState::Open {
            counts.open_items += 1;
        }
        if state.readiness(graph, item) == Some(Readiness::Blocked) {
            counts.blocked_items += 1;
        }
    }
    counts
}

fn item_ref(state: &State, id: ItemId) -> Option<ItemRefView> {
    state.item(id).map(view::item_ref_view)
}

/// `coder` for an agent, `@ada` for a principal.
fn who(state: &State, actor: &Actor) -> String {
    match actor {
        Actor::Agent { agent, .. } => agent.name.to_string(),
        Actor::Principal(_) | Actor::Daemon(_) => state.actor_label(actor),
    }
}

fn waiting(
    state: &State,
    own: DeviceKey,
    event: &dyn Fn(EventId) -> Option<Event>,
    held: &[HeldPermission],
    machine_faults: &wf_daemon::MachineFaults,
) -> Vec<InboxEntry> {
    let mut waiting: Vec<InboxEntry> = held
        .iter()
        .map(|request| {
            let session = state.session(request.session);
            InboxEntry::Permission {
                session: request.session,
                since_ms: request.since_ms,
                agent: session.map_or_else(
                    || "an agent".to_owned(),
                    |session| session.agent().name.to_string(),
                ),
                item: session.and_then(|session| item_ref(state, session.item())),
                what: request.what.clone(),
            }
        })
        .collect();
    for entry in state.inbox() {
        // An entry of the inbox is about an item of the state.
        let Some(item) = item_ref(state, entry.item) else {
            continue;
        };
        waiting.push(match entry.waiting {
            InboxWaiting::Blocker { comment } => {
                let id = EventId::from(comment);
                let (by, text) = event(id)
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
                InboxEntry::Blocker {
                    item,
                    comment,
                    by,
                    text,
                }
            }
            InboxWaiting::PrincipalReview {
                commit,
                stale,
                reason,
            } => InboxEntry::Review {
                item,
                commit,
                stale,
                reason,
            },
            InboxWaiting::Proposal
            | InboxWaiting::Recovery { .. }
            | InboxWaiting::Mention { .. } => InboxEntry::Notice {
                kind: entry.waiting.kind(),
                what: state.what_waits(&entry),
                item,
            },
        });
    }
    waiting.extend(view::lost_pushes(state, own).iter().map(|push| {
        let view = view::lost_push_view(state, push);
        InboxEntry::LostPush {
            commit: push.commit,
            title: view.title,
            what: view.what,
        }
    }));
    waiting.extend(
        view::machine_fault_views(machine_faults)
            .into_iter()
            .map(|fault| InboxEntry::MachineFault {
                title: fault.title,
                what: fault.what,
            }),
    );
    waiting
}

fn session_info(
    state: &State,
    graph: &DependencyGraph,
    own: DeviceKey,
    session: &Session,
) -> SessionInfo {
    let place = if session.device() == own {
        Place::Here
    } else {
        Place::Elsewhere(view::device_name(state, session.device()))
    };
    SessionInfo {
        view: view::session_view(state, graph, session, None),
        agent_name: session.agent().name.clone(),
        place,
    }
}

impl InView {
    /// Takes what `wanted` shows; `flagged` is what `wf status` flags.
    pub fn take(sources: &Sources, flagged: &[Flag], wanted: Detail) -> Option<InView> {
        let Sources {
            state,
            graph,
            detail_events: events,
            comment_texts: _,
            event: _,
            own,
            now_ms,
            detail_width: width,
        } = *sources;
        match wanted {
            Detail::Item(id) => {
                let item = state.item(id)?;
                let text = match item {
                    ItemEntry::Live(live) => render::item_detail_for(
                        &view::item_detail(state, graph, live, events),
                        Some(usize::from(width).min(100)),
                        Format::Styled,
                    ),
                    ItemEntry::Archived(_) => {
                        format!("#{} {} (archived)\n", id.short(), item.title())
                    }
                };
                let session = item_session(state, id).map(Session::id);
                Some(InView::Item { id, text, session })
            }
            Detail::Session(id) => {
                let session = state.session(id)?;
                let successor = state
                    .taken_over_by(session)
                    .or_else(|| item_session(state, session.item()))
                    .filter(|successor| !session.is_live() && successor.id() != id)
                    .map(|successor| Box::new(session_info(state, graph, own, successor)));
                Some(InView::Session {
                    info: Box::new(session_info(state, graph, own, session)),
                    successor,
                })
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
                let ItemEntry::Live(item) = state.item(id)? else {
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

/// The session of `item` that `enter` opens from it: the live one, else the latest.
fn item_session(state: &State, item: ItemId) -> Option<&Session> {
    state
        .sessions()
        .filter(|session| session.item() == item)
        .max_by_key(|session| (session.is_live(), session.started()))
}

/// The agents, each with its live sessions and the latest that ended.
fn agents(state: &State, graph: &DependencyGraph, own: DeviceKey) -> Vec<AgentInfo> {
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
                view: view::agent_view(state, id, definition),
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

fn item_groups(sources: &Sources, listing: ItemListing) -> Vec<ItemGroup> {
    let Sources {
        state,
        graph,
        comment_texts,
        ..
    } = *sources;
    let found: Option<BTreeSet<ItemId>> = listing.search.map(|query| {
        search(state, &query, comment_texts, SearchScope::Live)
            .iter()
            .map(|hit| hit.item.id())
            .collect()
    });
    let summaries = sorted_views(state, crate::commands::Scope::Live, listing.order)
        .into_iter()
        .filter(|item| {
            found
                .as_ref()
                .is_none_or(|found| found.contains(&item.id()))
        })
        .map(|view| view::summary(state, graph, view))
        .collect();
    view::group_items(listing.grouping, summaries)
}

/// Work the daemon moved from `review` back to `open`, as when approved work conflicts with the
/// target: the principal did not make the move, and may miss it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MovedBack {
    pub item: ItemId,
    /// The first sentence of the comment the move names as its reason.
    pub reason: String,
}

/// The moves back among the `new` events: those the daemon made from `review` to `open` naming a
/// comment that says why. Each is read against the events of its item before it, which
/// `item_events` has in the order they were applied, the move among them.
pub fn moves_back<'a>(
    new: &[Event],
    item_events: &dyn Fn(ItemId) -> &'a [Event],
) -> Vec<MovedBack> {
    let mut moves = Vec::new();
    for event in new {
        let EventBody::ItemMoved(moved) = &event.content.body else {
            continue;
        };
        let (Actor::Daemon(_), ItemState::Open, Some(because)) =
            (&event.content.actor, moved.to, moved.because)
        else {
            continue;
        };
        let events = item_events(moved.item);
        let Some(index) = events.iter().position(|known| known.id() == event.id()) else {
            continue;
        };
        let before = &events[..index];
        let from = before.iter().rev().find_map(|earlier| {
            if let EventBody::ItemMoved(earlier) = &earlier.content.body
                && earlier.item == moved.item
            {
                Some(earlier.to)
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

impl Detail {
    /// Where a detail starts: a transcript at its end, which is what happens now.
    pub(super) fn start(&self) -> Scroll {
        match self {
            Detail::Item(_) | Detail::Agent(_) | Detail::Flag(_) | Detail::Diff(_) => Scroll::At(0),
            Detail::Session(_) => Scroll::END,
        }
    }
}

impl InboxEntry {
    /// The blocker this is, if it is one: its item and its comment.
    pub fn blocker(&self) -> Option<(ItemId, CommentId)> {
        match self {
            InboxEntry::Blocker { item, comment, .. } => Some((item.id, *comment)),
            InboxEntry::Permission { .. }
            | InboxEntry::Review { .. }
            | InboxEntry::Notice { .. }
            | InboxEntry::MachineFault { .. }
            | InboxEntry::LostPush { .. } => None,
        }
    }

    /// The review this is, if it is one: its item, and the commit that was submitted.
    pub fn review(&self) -> Option<(ItemId, Option<CommitId>)> {
        match self {
            InboxEntry::Review { item, commit, .. } => Some((item.id, *commit)),
            InboxEntry::Permission { .. }
            | InboxEntry::Blocker { .. }
            | InboxEntry::Notice { .. }
            | InboxEntry::MachineFault { .. }
            | InboxEntry::LostPush { .. } => None,
        }
    }

    /// The review of `item` this is, if it is one: the commit that was submitted.
    pub fn review_of(&self, item: ItemId) -> Option<Option<CommitId>> {
        self.review()
            .filter(|(reviewed, _)| *reviewed == item)
            .map(|(_, commit)| commit)
    }
}

impl InboxEntry {
    /// The permission request this is, if it is one.
    pub fn request(&self) -> Option<Request> {
        match self {
            InboxEntry::Permission {
                session, since_ms, ..
            } => Some(Request {
                session: *session,
                since_ms: *since_ms,
            }),
            InboxEntry::Blocker { .. }
            | InboxEntry::Review { .. }
            | InboxEntry::Notice { .. }
            | InboxEntry::MachineFault { .. }
            | InboxEntry::LostPush { .. } => None,
        }
    }
}

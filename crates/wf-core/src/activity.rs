//! What happened to an item, in order: its comments, its moves, edits and links, its sessions,
//! reviews and merges, and what other items did that changes it. Pure: the caller loads the
//! events from the log.

use std::collections::{BTreeMap, BTreeSet};

use crate::body::StepId;
use crate::crdt::SetChange;
use crate::event::{Change, CommentKind, Event, EventBody, Link};
use crate::hlc::Hlc;
use crate::id::{CommentId, EventId, HashId, ItemId, MIN_PREFIX_LEN, SessionId};
use crate::identity::Actor;
use crate::item::{ItemState, ItemView};
use crate::refs;
use crate::session::{CommitId, EndReason, SessionRole, Verdict};
use crate::state::State;

/// One thing that happened, by whom and when.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub event: EventId,
    pub at: Hlc,
    /// Who did it. For a session's start and end, the agent of the session.
    pub by: Actor,
    pub activity: Activity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Activity {
    Created,
    Commented {
        comment: CommentId,
        kind: CommentKind,
        /// With the lines of the diff it is about above it, when it is about some.
        text: String,
        resolves: Vec<CommentId>,
        step: Option<StepId>,
    },
    /// A move someone made; moves carried out for a session are part of its start or end.
    Moved {
        to: ItemState,
        because: Option<CommentId>,
    },
    Edited(Change),
    /// A body edit that lost to a concurrent one: its author had not seen the edit that won.
    BodyOverwritten {
        body: String,
    },
    DependsOn(SetChange<ItemId>),
    Supersedes(ItemId),
    Archived,
    Unarchived,
    SessionStarted {
        session: SessionId,
        role: SessionRole,
    },
    SessionEnded {
        session: SessionId,
        reason: EndReason,
        /// Where the end moved the item, unless it stayed where it was.
        moved_to: Option<ItemState>,
        because: Option<CommentId>,
    },
    Reviewed {
        commit: Option<CommitId>,
        verdict: Verdict,
        because: Option<CommentId>,
    },
    Merged {
        commit: CommitId,
    },
    /// A dependency was moved to `done` or `cancelled`.
    DependencyFinished {
        item: ItemId,
        to: ItemState,
    },
    /// Another item started or stopped depending on this one.
    Dependent(SetChange<ItemId>),
    /// Another item supersedes this one.
    SupersededBy(ItemId),
    /// Another item's body or a comment on it refers to this one, or to `comment` on it.
    ReferencedIn {
        item: ItemId,
        comment: Option<CommentId>,
    },
}

impl State {
    /// The activity of `item`, in fold order, from `events`: the log, or as much of it as is at
    /// hand. A comment whose event is missing is left out.
    pub fn activity(&self, item: ItemId, events: &[Event]) -> Vec<Entry> {
        let mut events: Vec<&Event> = events.iter().collect();
        // An id is a hash of the event: compare them only where the clocks tie.
        events.sort_by(|one, other| {
            (one.content.hlc.cmp(&other.content.hlc)).then_with(|| one.id().cmp(&other.id()))
        });
        // When the item was created, where it is known, to tell its creation apart without
        // hashing every other.
        let (depends_on, created_at): (Vec<ItemId>, Option<Hlc>) = match self.item(item) {
            Some(ItemView::Live(live)) => (
                live.depends_on().values().copied().collect(),
                Some(live.created()),
            ),
            Some(ItemView::Archived(_)) | None => (Vec::new(), None),
        };
        let resolver = self.resolver();
        // Every reference to this item has the start of its id: text without it is not parsed.
        let id_start = item.to_base32()[..MIN_PREFIX_LEN].to_owned();
        // What of this item `text` refers to: the item itself (`None`) or its comments.
        let referenced_here = |text: &str| -> BTreeSet<Option<CommentId>> {
            if !text.contains(&id_start) {
                return BTreeSet::new();
            }
            refs::parse(text)
                .iter()
                .filter_map(|token| resolver.reference(token))
                .filter(|reference| reference.item() == item)
                .map(|reference| reference.comment())
                .collect()
        };
        // The last move each session made: its end's, when it is not the start's.
        let mut session_moves: BTreeMap<SessionId, ItemState> = BTreeMap::new();
        for event in &events {
            if let EventBody::ItemMoved(moved) = &event.content.body
                && moved.item == item
                && let Some(session) = moved.by_session
            {
                session_moves.insert(session, moved.to.state());
            }
        }

        let overwritten = overwritten_body_edits(item, &events);

        let mut entries = Vec::new();
        for event in events {
            let content = &event.content;
            let session_actor = |session: SessionId| {
                self.session(session)
                    .map_or(content.actor.clone(), |folded| Actor::Agent {
                        agent: folded.agent().clone(),
                        session,
                    })
            };
            let mut by = content.actor.clone();
            let mut referenced_in = |other: ItemId, referenced: BTreeSet<Option<CommentId>>| {
                entries.extend(referenced.into_iter().map(|comment| Entry {
                    event: event.id(),
                    at: content.hlc,
                    by: content.actor.clone(),
                    activity: Activity::ReferencedIn {
                        item: other,
                        comment,
                    },
                }));
            };
            let activity = match &content.body {
                EventBody::ItemCreated(_)
                    if created_at.is_none_or(|at| at == content.hlc)
                        && ItemId::from(event.id()) == item =>
                {
                    Activity::Created
                }
                EventBody::ItemCreated(created) => {
                    referenced_in(event.id().into(), referenced_here(&created.body));
                    continue;
                }
                EventBody::Commented(comment) if comment.item == item => Activity::Commented {
                    comment: event.id().into(),
                    kind: comment.kind,
                    text: comment.text_with_lines(),
                    resolves: comment.resolves.clone(),
                    step: comment.step,
                },
                EventBody::Commented(comment) => {
                    referenced_in(comment.item, referenced_here(&comment.text));
                    continue;
                }
                EventBody::ItemMoved(moved) if moved.item == item => match moved.by_session {
                    Some(_) => continue,
                    None => Activity::Moved {
                        to: moved.to.state(),
                        because: moved.because,
                    },
                },
                EventBody::ItemMoved(moved) if depends_on.contains(&moved.item) => {
                    match moved.to.state() {
                        finished @ (ItemState::Done | ItemState::Cancelled) => {
                            Activity::DependencyFinished {
                                item: moved.item,
                                to: finished,
                            }
                        }
                        ItemState::Draft
                        | ItemState::Open
                        | ItemState::Active
                        | ItemState::Review => continue,
                    }
                }
                EventBody::ItemEdited(edited) if edited.item == item => {
                    for change in &edited.changes {
                        let activity = match change {
                            Change::Body(body) if overwritten.contains(&event.id()) => {
                                Activity::BodyOverwritten { body: body.clone() }
                            }
                            change => Activity::Edited(change.clone()),
                        };
                        entries.push(Entry {
                            event: event.id(),
                            at: content.hlc,
                            by: by.clone(),
                            activity,
                        });
                    }
                    continue;
                }
                EventBody::ItemEdited(edited) => {
                    let referenced = edited
                        .changes
                        .iter()
                        .flat_map(|change| match change {
                            Change::Body(body) => referenced_here(body),
                            Change::Title(_)
                            | Change::Priority(_)
                            | Change::Lane(_)
                            | Change::Labels(_)
                            | Change::Requires(_)
                            | Change::ReviewLevel(_)
                            | Change::StepDone { .. }
                            | Change::RecoveryLevel(_) => BTreeSet::new(),
                        })
                        .collect();
                    referenced_in(edited.item, referenced);
                    continue;
                }
                EventBody::ItemLinked(linked) if linked.item == item => match &linked.link {
                    Link::DependsOn(change) => Activity::DependsOn(change.clone()),
                    Link::Supersedes(target) => Activity::Supersedes(*target),
                },
                EventBody::ItemLinked(linked) => match &linked.link {
                    Link::DependsOn(SetChange::Add(target)) if *target == item => {
                        Activity::Dependent(SetChange::Add(linked.item))
                    }
                    Link::DependsOn(SetChange::Remove { value, observed }) if *value == item => {
                        Activity::Dependent(SetChange::Remove {
                            value: linked.item,
                            observed: observed.clone(),
                        })
                    }
                    Link::Supersedes(target) if *target == item => {
                        Activity::SupersededBy(linked.item)
                    }
                    Link::DependsOn(_) | Link::Supersedes(_) => continue,
                },
                EventBody::ItemArchived(reference) if reference.item == item => Activity::Archived,
                EventBody::ItemUnarchived(reference) if reference.item == item => {
                    Activity::Unarchived
                }
                EventBody::SessionStarted(started) if started.item == item => {
                    let session = SessionId::from(event.id());
                    by = session_actor(session);
                    Activity::SessionStarted {
                        session,
                        role: started.role,
                    }
                }
                EventBody::SessionEnded(ended)
                    if self
                        .session(ended.session)
                        .is_some_and(|session| session.item() == item) =>
                {
                    by = session_actor(ended.session);
                    let moved_to = session_moves
                        .get(&ended.session)
                        .filter(|to| **to != ItemState::Active)
                        .copied();
                    Activity::SessionEnded {
                        session: ended.session,
                        reason: ended.reason,
                        moved_to,
                        because: ended.because,
                    }
                }
                EventBody::Reviewed(review) if review.item == item => Activity::Reviewed {
                    commit: review.commit,
                    verdict: review.verdict,
                    because: review.because,
                },
                EventBody::Merged(merged) if merged.item == item => Activity::Merged {
                    commit: merged.commit,
                },
                // Events of other items, and events that are not about items.
                EventBody::ItemMoved(_)
                | EventBody::ItemArchived(_)
                | EventBody::ItemUnarchived(_)
                | EventBody::SessionStarted(_)
                | EventBody::SessionEnded(_)
                | EventBody::Reviewed(_)
                | EventBody::Merged(_)
                | EventBody::Genesis(_)
                | EventBody::PolicySet(_)
                | EventBody::BoundPolicySet(_)
                | EventBody::AgentDefined(_)
                | EventBody::AgentRemoved(_)
                | EventBody::InviteCreated(_)
                | EventBody::DeviceBound(_)
                | EventBody::DeviceProfile(_)
                | EventBody::ForkAnnounced(_)
                | EventBody::Unknown { .. } => continue,
            };
            entries.push(Entry {
                event: event.id(),
                at: content.hlc,
                by,
                activity,
            });
        }
        entries
    }
}

impl State {
    /// The item an event is about, if it is about one.
    pub fn item_of(&self, event: &Event) -> Option<ItemId> {
        match &event.content.body {
            EventBody::ItemCreated(_) => Some(event.id().into()),
            EventBody::Commented(comment) => Some(comment.item),
            EventBody::ItemMoved(moved) => Some(moved.item),
            EventBody::ItemEdited(edited) => Some(edited.item),
            EventBody::ItemLinked(linked) => Some(linked.item),
            EventBody::ItemArchived(reference) | EventBody::ItemUnarchived(reference) => {
                Some(reference.item)
            }
            EventBody::SessionStarted(started) => Some(started.item),
            EventBody::SessionEnded(ended) => {
                self.session(ended.session).map(|session| session.item())
            }
            EventBody::Reviewed(review) => Some(review.item),
            EventBody::Merged(merged) => Some(merged.item),
            EventBody::Genesis(_)
            | EventBody::PolicySet(_)
            | EventBody::BoundPolicySet(_)
            | EventBody::AgentDefined(_)
            | EventBody::AgentRemoved(_)
            | EventBody::InviteCreated(_)
            | EventBody::DeviceBound(_)
            | EventBody::DeviceProfile(_)
            | EventBody::ForkAnnounced(_)
            | EventBody::Unknown { .. } => None,
        }
    }
}

/// The body edits of `item` that lost to a concurrent edit: every one that is neither the edit
/// that won, the latest by `(hlc, id)`, nor in its causal past. `events` are in fold order.
fn overwritten_body_edits(item: ItemId, events: &[&Event]) -> BTreeSet<EventId> {
    let edits_body = |event: &&&Event| {
        let EventBody::ItemEdited(edited) = &event.content.body else {
            return false;
        };
        edited.item == item
            && edited
                .changes
                .iter()
                .any(|change| matches!(change, Change::Body(_)))
    };
    let body_edits: Vec<EventId> = events
        .iter()
        .filter(edits_body)
        .map(|event| event.id())
        .collect();
    // With one edit or none, none lost; this spares hashing every event for `by_id`.
    let [.., _, winner] = body_edits[..] else {
        return BTreeSet::new();
    };
    let by_id: BTreeMap<EventId, &Event> =
        events.iter().map(|event| (event.id(), *event)).collect();
    let mut seen = BTreeSet::new();
    let mut waiting = vec![winner];
    while let Some(id) = waiting.pop() {
        let Some(event) = by_id.get(&id) else {
            continue;
        };
        for parent in event.content.prev.iter().chain(&event.content.deps) {
            if seen.insert(*parent) {
                waiting.push(*parent);
            }
        }
    }
    body_edits
        .into_iter()
        .filter(|edit| *edit != winner && !seen.contains(edit))
        .collect()
}

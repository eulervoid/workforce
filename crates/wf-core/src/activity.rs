//! What happened to an item, in order: its comments, its moves, edits and links, its sessions,
//! reviews and merges, and what other items did that changes it. Pure: the caller loads the
//! events from the log.

use std::collections::{BTreeMap, BTreeSet};

use crate::body::StepId;
use crate::crdt::SetChange;
use crate::event::{Change, CommentKind, Event, EventBody, Link};
use crate::hlc::Hlc;
use crate::id::{CommentId, EventId, ItemId, SessionId};
use crate::identity::Actor;
use crate::item::ItemView;
use crate::name::StateName;
use crate::policy::StateCategory;
use crate::refs::{self, Token};
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
        text: String,
        resolves: Vec<CommentId>,
        step: Option<StepId>,
    },
    /// A move someone made; moves carried out for a session are part of its start or end.
    Moved {
        to: StateName,
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
        moved_to: Option<StateName>,
    },
    Reviewed {
        commit: Option<CommitId>,
        verdict: Verdict,
        because: Option<CommentId>,
    },
    Merged {
        commit: CommitId,
    },
    /// A dependency reached its done or cancelled state.
    DependencyFinished {
        item: ItemId,
        to: StateName,
        category: StateCategory,
    },
    /// Another item started or stopped depending on this one.
    Dependent(SetChange<ItemId>),
    /// Another item supersedes this one.
    SupersededBy(ItemId),
    /// Another item's body or a comment on it refers to this one.
    ReferencedIn(ItemId),
}

impl State {
    /// The activity of `item`, in fold order, from `events`: the log, or as much of it as is at
    /// hand. A comment whose event is missing is left out.
    pub fn activity(&self, item: ItemId, events: &[Event]) -> Vec<Entry> {
        let mut events: Vec<&Event> = events.iter().collect();
        events.sort_by_key(|event| (event.content.hlc, event.id()));
        let policy = self.policy().map(|document| &document.policy);
        let category = |of: ItemId, state: &StateName| {
            let ItemView::Live(live) = self.item(of)? else {
                return None;
            };
            policy?
                .type_config(live.item_type())?
                .state_category_of(state)
        };
        let depends_on: Vec<ItemId> = match self.item(item) {
            Some(ItemView::Live(live)) => live.depends_on().values().copied().collect(),
            Some(ItemView::Archived(_)) | None => Vec::new(),
        };
        let resolver = self.resolver();
        let refers_here = |text: &str| {
            refs::parse(text).iter().any(|token| match token {
                Token::ItemRef(prefix) => resolver.item(prefix) == Ok(item),
                Token::Mention(_) => false,
            })
        };
        // The last move each session made: its end's, when it is not the start's.
        let mut session_moves: BTreeMap<SessionId, StateName> = BTreeMap::new();
        for event in &events {
            if let EventBody::ItemMoved(moved) = &event.content.body
                && moved.item == item
                && let Some(session) = moved.by_session
            {
                session_moves.insert(session, moved.to.clone());
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
            let activity = match &content.body {
                EventBody::ItemCreated(_) if ItemId::from(event.id()) == item => Activity::Created,
                EventBody::ItemCreated(created) if refers_here(&created.body) => {
                    Activity::ReferencedIn(event.id().into())
                }
                EventBody::Comment(comment) if comment.item == item => Activity::Commented {
                    comment: event.id().into(),
                    kind: comment.kind,
                    text: comment.text.clone(),
                    resolves: comment.resolves.clone(),
                    step: comment.step,
                },
                EventBody::Comment(comment) if refers_here(&comment.text) => {
                    Activity::ReferencedIn(comment.item)
                }
                EventBody::ItemMoved(moved) if moved.item == item => match moved.by_session {
                    Some(_) => continue,
                    None => Activity::Moved {
                        to: moved.to.clone(),
                        because: moved.because,
                    },
                },
                EventBody::ItemMoved(moved) if depends_on.contains(&moved.item) => {
                    match category(moved.item, &moved.to) {
                        Some(finished @ (StateCategory::Done | StateCategory::Cancelled)) => {
                            Activity::DependencyFinished {
                                item: moved.item,
                                to: moved.to.clone(),
                                category: finished,
                            }
                        }
                        _ => continue,
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
                    let refers = edited.changes.iter().any(|change| match change {
                        Change::Body(body) => refers_here(body),
                        _ => false,
                    });
                    if !refers {
                        continue;
                    }
                    Activity::ReferencedIn(edited.item)
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
                        .filter(|to| category(item, to) != Some(StateCategory::Active))
                        .cloned();
                    Activity::SessionEnded {
                        session: ended.session,
                        reason: ended.reason,
                        moved_to,
                    }
                }
                EventBody::Review(review) if review.item == item => Activity::Reviewed {
                    commit: review.commit,
                    verdict: review.verdict,
                    because: review.because,
                },
                EventBody::Merged(merged) if merged.item == item => Activity::Merged {
                    commit: merged.commit,
                },
                // Events of other items, and events that are not about items.
                EventBody::ItemCreated(_)
                | EventBody::Comment(_)
                | EventBody::ItemMoved(_)
                | EventBody::ItemArchived(_)
                | EventBody::ItemUnarchived(_)
                | EventBody::SessionStarted(_)
                | EventBody::SessionEnded(_)
                | EventBody::Review(_)
                | EventBody::Merged(_)
                | EventBody::Genesis(_)
                | EventBody::PolicySet(_)
                | EventBody::AgentDefined(_)
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
    let Some(winner) = body_edits.last().copied() else {
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

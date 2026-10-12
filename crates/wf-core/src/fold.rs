//! Applying admitted events. Every register and set update here is commutative, so the result is
//! a function of the set of events, not of the order they were applied in.

use std::collections::{BTreeMap, BTreeSet};

use crate::crdt::{Lww, Stamp};
use crate::event::{Change, CommentKind, Event, EventBody, ItemLinked, Link};
use crate::id::{CommentId, EventId, ItemId, SessionId};
use crate::identity::{AgentId, Role};
use crate::item::{Comment, Item, Merge, Resolved, Verdicted};
use crate::name::StateName;
use crate::refs;
use crate::session::{Ended, Session};
use crate::state::{
    AgentDefinition, Announcement, Binding, Feed, FeedHead, ForkState, Invite, Known, Member,
    Neighbours, Origin, PolicyDoc, State, Workforce,
};

impl State {
    /// Applies an event that [`State::receive`] has admitted.
    pub(crate) fn apply(&mut self, event: &Event, id: EventId, origin: Origin) {
        let content = &event.content;
        let stamp = Stamp {
            hlc: content.hlc,
            id,
        };

        self.known.insert(
            id,
            Known {
                hlc: content.hlc,
                author: content.author,
                seq: content.seq,
            },
        );
        let head = FeedHead {
            seq: content.seq,
            id,
        };
        let around = Neighbours {
            has_child: self.extended.contains(&id),
            has_sibling: content.prev.is_some_and(|prev| !self.extended.insert(prev)),
        };
        self.feeds
            .entry(content.author)
            .or_insert_with(Feed::new)
            .record(head, content.prev, around);
        if origin == Origin::History {
            self.take_in_history(event, id);
            return;
        }
        for parent in event.causal_parents() {
            self.heads.remove(&parent);
        }
        self.heads.insert(id);

        if let EventBody::Genesis(signed) = &content.body {
            let genesis = &signed.statement;
            let mut members = BTreeMap::new();
            let mut devices = BTreeMap::new();
            let mut aliases = BTreeMap::new();
            let genesis_binding = |principal| Binding { principal, stamp };
            let seeds = std::iter::once((&genesis.founder, Role::Owner))
                .chain(genesis.members.iter().map(|seed| (seed, Role::Member)));
            for (seed, role) in seeds {
                for previous in &seed.previous {
                    aliases.insert(*previous, seed.principal);
                }
                for device in &seed.devices {
                    devices.insert(*device, genesis_binding(seed.principal));
                }
                members.insert(
                    seed.principal,
                    Member {
                        handle: seed.handle.clone(),
                        role,
                        devices: seed.devices.iter().copied().collect(),
                    },
                );
            }
            let policy = PolicyDoc::parse(&genesis.policy).unwrap_or_else(|_| PolicyDoc::default());
            self.expected = None;
            self.workforce = Some(Workforce {
                id: id.into(),
                name: genesis.name.clone(),
                founder: genesis.founder.principal,
                members,
                devices,
                policy_source: Lww::new(genesis.policy.clone(), stamp),
                policy: Lww::new(policy, stamp),
                agents: BTreeMap::new(),
                items: BTreeMap::new(),
                sessions: BTreeMap::new(),
                invites: BTreeMap::new(),
                profiles: BTreeMap::new(),
                fork: genesis
                    .forked_from
                    .as_ref()
                    .map(|point| ForkState::starting_at(point.workforce, point.heads.clone())),
                aliases,
                announcements: BTreeMap::new(),
            });
            return;
        }

        self.fold_body(event, id, stamp, Origin::Own);
    }

    /// Takes in an event of the history this workforce was forked from. It is only recorded here:
    /// history arrives newest first, so its bodies are folded once all of it has arrived.
    fn take_in_history(&mut self, event: &Event, id: EventId) {
        let known = &self.known;
        let Some(ForkState::TakingIn {
            wanted, admitted, ..
        }) = self
            .workforce
            .as_mut()
            .and_then(|workforce| workforce.fork.as_mut())
        else {
            return;
        };
        wanted.remove(&id);
        wanted.extend(
            event
                .causal_parents()
                .filter(|parent| !known.contains_key(parent)),
        );
        admitted.insert(id, event.clone());
        if wanted.is_empty() {
            self.fold_history();
        }
    }

    /// Folds the whole history of a fork, oldest event first, which puts every event after its
    /// parents.
    fn fold_history(&mut self) {
        let Some(workforce) = self.workforce.as_mut() else {
            return;
        };
        let Some(ForkState::TakingIn {
            parent,
            heads,
            admitted,
            ..
        }) = workforce.fork.take()
        else {
            return;
        };
        workforce.fork = Some(ForkState::Complete { parent, heads });
        let mut history: Vec<(Stamp, Event)> = admitted
            .into_iter()
            .map(|(id, event)| {
                let stamp = Stamp {
                    hlc: event.content.hlc,
                    id,
                };
                (stamp, event)
            })
            .collect();
        history.sort_by_key(|(stamp, _)| *stamp);
        for (stamp, event) in history {
            self.fold_body(&event, stamp.id, stamp, Origin::History);
        }
    }

    fn fold_body(&mut self, event: &Event, id: EventId, stamp: Stamp, origin: Origin) {
        let content = &event.content;
        let Some(workforce) = self.workforce.as_mut() else {
            return;
        };
        if origin == Origin::History
            && matches!(
                content.body,
                EventBody::Genesis(_)
                    | EventBody::PolicySet(_)
                    | EventBody::InviteCreated(_)
                    | EventBody::DeviceBound(_)
                    | EventBody::ForkAnnounced(_)
                    | EventBody::SessionStarted(_)
                    | EventBody::SessionEnded(_)
                    | EventBody::Review(_)
                    | EventBody::Merged(_)
                    | EventBody::Unknown { .. }
            )
        {
            return;
        }
        // What a device said about itself carries over for the devices the fork keeps.
        if origin == Origin::History
            && matches!(content.body, EventBody::DeviceProfile(_))
            && !workforce.devices.contains_key(&content.author)
        {
            return;
        }
        match &content.body {
            EventBody::Genesis(_) | EventBody::Unknown { .. } => {}
            EventBody::ForkAnnounced(signed) => {
                workforce.announcements.insert(
                    id,
                    Announcement {
                        to: signed.statement.to,
                        members: signed.statement.members.clone(),
                        by: signed.signer,
                    },
                );
            }
            EventBody::InviteCreated(signed) => {
                let created = &signed.statement;
                workforce.invites.insert(
                    id,
                    Invite {
                        kind: created.kind,
                        principal: signed.signer,
                        key: created.invite,
                        expires_ms: created.expires_ms,
                        redeemed_by: BTreeSet::new(),
                    },
                );
            }
            EventBody::DeviceBound(bound) => {
                let Some(invite) = workforce.invites.get_mut(&bound.invite) else {
                    return;
                };
                invite.redeemed_by.insert(content.author);
                let principal = invite.principal;
                let binding = Binding { principal, stamp };
                let earlier = workforce
                    .devices
                    .get(&content.author)
                    .is_none_or(|existing| binding.stamp < existing.stamp);
                if earlier {
                    if let Some(previous) = workforce.devices.insert(content.author, binding)
                        && let Some(member) = workforce.members.get_mut(&previous.principal)
                    {
                        member.devices.remove(&content.author);
                    }
                    if let Some(member) = workforce.members.get_mut(&principal) {
                        member.devices.insert(content.author);
                    }
                }
            }
            EventBody::DeviceProfile(profile) => {
                match workforce.profiles.get_mut(&content.author) {
                    Some(existing) => {
                        existing.set(profile.clone(), stamp);
                    }
                    None => {
                        workforce
                            .profiles
                            .insert(content.author, Lww::new(profile.clone(), stamp));
                    }
                }
            }
            EventBody::PolicySet(signed) => {
                let source = &signed.statement.policy;
                workforce.policy_source.set(source.clone(), stamp);
                if let Ok(policy) = PolicyDoc::parse(source) {
                    workforce.policy.set(policy, stamp);
                }
            }
            EventBody::AgentDefined(_)
                if origin == Origin::History
                    && !workforce
                        .members
                        .contains_key(&workforce.canonical(&content.actor.principal())) => {}
            EventBody::AgentDefined(defined) => {
                let agent = AgentId {
                    principal: workforce.canonical(&content.actor.principal()),
                    name: defined.name.clone(),
                };
                let definition = AgentDefinition {
                    harness: defined.harness.clone(),
                    model: defined.model.clone(),
                    capabilities: defined.capabilities.clone(),
                    lanes: defined.lanes.clone(),
                    autonomy: defined.autonomy.clone(),
                    instructions: defined.instructions.clone(),
                };
                match workforce.agents.get_mut(&agent) {
                    Some(existing) => {
                        existing.set(definition, stamp);
                    }
                    None => {
                        workforce.agents.insert(agent, Lww::new(definition, stamp));
                    }
                }
            }
            EventBody::SessionStarted(started) => {
                let session = SessionId::from(id);
                workforce.sessions.insert(
                    session,
                    Session {
                        id: session,
                        agent: AgentId {
                            principal: workforce.canonical(&content.actor.principal()),
                            name: started.agent.clone(),
                        },
                        item: started.item,
                        role: started.role,
                        device: content.author,
                        started: stamp,
                        ended: None,
                    },
                );
            }
            EventBody::SessionEnded(ended) => {
                if let Some(session) = workforce.sessions.get_mut(&ended.session)
                    && session.ended.is_none_or(|earlier| stamp < earlier.stamp)
                {
                    session.ended = Some(Ended {
                        reason: ended.reason,
                        stamp,
                    });
                }
            }
            EventBody::Review(review) => {
                if let Some(item) = workforce.items.get_mut(&review.item) {
                    item.touch(stamp, &content.actor);
                    item.verdicts.insert(
                        id,
                        Verdicted {
                            commit: review.commit,
                            verdict: review.verdict,
                            by: content.actor.clone(),
                            stamp,
                        },
                    );
                }
            }
            EventBody::Merged(merged) => {
                if let Some(item) = workforce.items.get_mut(&merged.item) {
                    item.touch(stamp, &content.actor);
                    item.merges.insert(
                        id,
                        Merge {
                            reviewed: merged.reviewed,
                            commit: merged.commit,
                        },
                    );
                }
            }
            EventBody::ItemCreated(created) => {
                let item_id = ItemId::from(id);
                workforce.items.insert(
                    item_id,
                    Item::new(item_id, content.actor.clone(), stamp, created),
                );
            }
            EventBody::ItemEdited(edited) => {
                if let Some(item) = workforce.items.get_mut(&edited.item) {
                    item.touch(stamp, &content.actor);
                    for change in &edited.changes {
                        apply_change(item, change, stamp);
                    }
                }
            }
            EventBody::ItemMoved(moved) => {
                if let Some(item) = workforce.items.get_mut(&moved.item) {
                    item.touch(stamp, &content.actor);
                    item.state.set(Some(moved.to.clone()), stamp);
                }
            }
            EventBody::ItemLinked(ItemLinked { item, link }) => match link {
                Link::DependsOn(change) => {
                    if let Some(item) = workforce.items.get_mut(item) {
                        item.touch(stamp, &content.actor);
                        item.depends_on.apply(change, stamp);
                    }
                }
                Link::Supersedes(target) => {
                    if let Some(item) = workforce.items.get_mut(item) {
                        item.touch(stamp, &content.actor);
                        item.supersedes.set(Some(*target), stamp);
                    }
                    if let Some(target) = workforce.items.get_mut(target) {
                        target.touch(stamp, &content.actor);
                        target.state.set(Some(superseded_state()), stamp);
                    }
                }
            },
            EventBody::ItemArchived(reference) => {
                if let Some(item) = workforce.items.get_mut(&reference.item) {
                    item.touch(stamp, &content.actor);
                    item.archived.set(Some(content.hlc), stamp);
                }
            }
            EventBody::ItemUnarchived(reference) => {
                if let Some(item) = workforce.items.get_mut(&reference.item) {
                    item.touch(stamp, &content.actor);
                    item.archived.set(None, stamp);
                }
            }
            EventBody::Comment(comment) => {
                if let Some(item) = workforce.items.get_mut(&comment.item) {
                    item.touch(stamp, &content.actor);
                    let by = CommentId::from(id);
                    item.comments.insert(
                        by,
                        Comment {
                            kind: comment.kind,
                            by: content.actor.clone(),
                            at: content.hlc,
                            step: comment.step,
                            tokens: refs::parse(&comment.text),
                            resolved: None,
                        },
                    );
                    for resolved in &comment.resolves {
                        let Some(target) = item.comments.get_mut(resolved) else {
                            continue;
                        };
                        if target.kind == CommentKind::Plain {
                            continue;
                        }
                        if target.resolved.is_none_or(|earlier| stamp < earlier.stamp) {
                            target.resolved = Some(Resolved { by, stamp });
                        }
                    }
                }
            }
        }
    }
}

fn superseded_state() -> StateName {
    StateName::new("superseded").expect("a valid built-in state name")
}

fn apply_change(item: &mut Item, change: &Change, stamp: Stamp) {
    match change {
        Change::Title(title) => {
            item.title.set(title.clone(), stamp);
        }
        Change::Body(body) => {
            if item.body.set(crate::item::body_of(body), stamp) {
                item.step_marks.retain(|_, mark| mark.stamp() > stamp);
            }
        }
        // Marks older than the body are dropped, so the stored marks do not depend on arrival order.
        Change::StepDone { step, done } if stamp > item.body.stamp() => {
            match item.step_marks.get_mut(step) {
                Some(mark) => {
                    mark.set(*done, stamp);
                }
                None => {
                    item.step_marks.insert(*step, Lww::new(*done, stamp));
                }
            }
        }
        Change::StepDone { .. } => {}
        Change::Priority(priority) => {
            item.priority.set(*priority, stamp);
        }
        Change::Lane(lane) => {
            item.lane.set(lane.clone(), stamp);
        }
        Change::Review(review) => {
            item.review.set(review.clone(), stamp);
        }
        Change::Recovery(recovery) => {
            item.recovery.set(recovery.clone(), stamp);
        }
        Change::Labels(change) => item.labels.apply(change, stamp),
        Change::Requires(change) => item.requires.apply(change, stamp),
    }
}

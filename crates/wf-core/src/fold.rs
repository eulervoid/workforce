//! Applying admitted events. Every register and set update here is commutative, so the result is
//! a function of the set of events, not of the order they were applied in.

use std::collections::{BTreeMap, BTreeSet};

use crate::crdt::{Lww, Stamp};
use crate::event::{Change, Event, EventBody, ItemLinked, Link};
use crate::id::{EventId, ItemId};
use crate::identity::{AgentId, Role};
use crate::item::{Item, Note, NoteKind};
use crate::name::StateName;
use crate::policy::Policy;
use crate::refs;
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
            self.take_in_history(event, id, stamp);
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
            let policy =
                Policy::parse(&genesis.policy).expect("admitted genesis has a valid policy");
            self.expected = None;
            self.workforce = Some(Workforce {
                id: id.into(),
                name: genesis.name.clone(),
                founder: genesis.founder.principal,
                members,
                devices,
                policy: Lww::new(
                    PolicyDoc {
                        source: genesis.policy.clone(),
                        policy,
                    },
                    stamp,
                ),
                agents: BTreeMap::new(),
                items: BTreeMap::new(),
                invites: BTreeMap::new(),
                profiles: BTreeMap::new(),
                fork: genesis.forked_from.as_ref().map(|point| ForkState {
                    parent: point.workforce,
                    heads: point.heads.clone(),
                    wanted: point.heads.iter().copied().collect(),
                }),
                aliases,
                announcements: BTreeMap::new(),
            });
            return;
        }

        self.fold_body(event, id, stamp, Origin::Own);
    }

    /// Takes in an event of the history this workforce was forked from. What happened to items
    /// and agents carries over, and what the devices that are kept said about themselves: the
    /// members, devices, invites and policy of the parent are not those of the fork.
    fn take_in_history(&mut self, event: &Event, id: EventId, stamp: Stamp) {
        if let Some(fork) = self
            .workforce
            .as_mut()
            .and_then(|workforce| workforce.fork.as_mut())
        {
            fork.wanted.remove(&id);
            fork.wanted.extend(
                event
                    .causal_parents()
                    .filter(|parent| !self.known.contains_key(parent)),
            );
        }
        self.fold_body(event, id, stamp, Origin::History);
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
                let policy = Policy::parse(source).expect("admitted policy is valid");
                workforce.policy.set(
                    PolicyDoc {
                        source: source.clone(),
                        policy,
                    },
                    stamp,
                );
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
            EventBody::ItemCreated(created) => {
                let item_id = ItemId::from(id);
                workforce.items.insert(
                    item_id,
                    Item::new(item_id, content.actor.clone(), stamp, created),
                );
            }
            EventBody::ItemEdited(edited) => {
                if let Some(item) = workforce.items.get_mut(&edited.item) {
                    item.touch(content.hlc);
                    for change in &edited.changes {
                        apply_change(item, change, stamp);
                    }
                }
            }
            EventBody::ItemTransitioned(transition) => {
                if let Some(item) = workforce.items.get_mut(&transition.item) {
                    item.touch(content.hlc);
                    item.state.set(Some(transition.to.clone()), stamp);
                    if let Some(note) = &transition.note {
                        item.notes.insert(
                            id,
                            note_from(NoteKind::Transition, &content.actor, content.hlc, note),
                        );
                    }
                }
            }
            EventBody::ItemLinked(ItemLinked { item, link }) => match link {
                Link::DependsOn(change) => {
                    if let Some(item) = workforce.items.get_mut(item) {
                        item.touch(content.hlc);
                        item.depends_on.apply(change, stamp);
                    }
                }
                Link::Supersedes(target) => {
                    if let Some(item) = workforce.items.get_mut(item) {
                        item.touch(content.hlc);
                        item.supersedes.set(Some(*target), stamp);
                    }
                    if let Some(target) = workforce.items.get_mut(target) {
                        target.touch(content.hlc);
                        target.state.set(Some(superseded_state()), stamp);
                    }
                }
            },
            EventBody::ItemArchived(reference) => {
                if let Some(item) = workforce.items.get_mut(&reference.item) {
                    item.touch(content.hlc);
                    item.archived.set(Some(content.hlc), stamp);
                }
            }
            EventBody::ItemUnarchived(reference) => {
                if let Some(item) = workforce.items.get_mut(&reference.item) {
                    item.touch(content.hlc);
                    item.archived.set(None, stamp);
                }
            }
            EventBody::Comment(comment) => {
                if let Some(item) = workforce.items.get_mut(&comment.item) {
                    item.touch(content.hlc);
                    item.notes.insert(
                        id,
                        note_from(
                            NoteKind::Comment,
                            &content.actor,
                            content.hlc,
                            &comment.body,
                        ),
                    );
                }
            }
        }
    }
}

fn superseded_state() -> StateName {
    StateName::new("superseded").expect("a valid built-in state name")
}

fn note_from(kind: NoteKind, by: &crate::identity::Actor, at: crate::hlc::Hlc, text: &str) -> Note {
    Note {
        kind,
        by: by.clone(),
        at,
        tokens: refs::parse(text),
    }
}

fn apply_change(item: &mut Item, change: &Change, stamp: Stamp) {
    match change {
        Change::Title(title) => {
            item.title.set(title.clone(), stamp);
        }
        Change::Body(body) => {
            item.body
                .set(crate::item::Markdown::new(body.clone()), stamp);
        }
        Change::Priority(priority) => {
            item.priority.set(*priority, stamp);
        }
        Change::Lane(lane) => {
            item.lane.set(lane.clone(), stamp);
        }
        Change::Labels(change) => item.labels.apply(change, stamp),
        Change::Requires(change) => item.requires.apply(change, stamp),
    }
}

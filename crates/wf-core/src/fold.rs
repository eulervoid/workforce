//! Applying admitted events. Every register and set update here is commutative, so the result is
//! a function of the set of events, not of the order they were applied in.

use std::collections::BTreeMap;

use crate::crdt::{Lww, Stamp};
use crate::event::{Change, Event, EventBody, ItemLinked, Link};
use crate::id::{EventId, ItemId};
use crate::identity::{AgentId, Role};
use crate::item::{Item, Note, NoteKind};
use crate::name::StateName;
use crate::policy::Policy;
use crate::refs;
use crate::state::{AgentDefinition, Feed, FeedHead, Known, Member, PolicyDoc, State, Workforce};

impl State {
    /// Applies an event that [`State::receive`] has admitted.
    pub(crate) fn apply(&mut self, event: &Event, id: EventId) {
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
        match self.feeds.get_mut(&content.author) {
            Some(feed) => feed.extend(head, content.prev),
            None => {
                self.feeds.insert(content.author, Feed::new(head));
            }
        }
        for parent in event.causal_parents() {
            self.heads.remove(&parent);
        }
        self.heads.insert(id);

        if let EventBody::Genesis(signed) = &content.body {
            let genesis = &signed.statement;
            let mut members = BTreeMap::new();
            let mut devices = BTreeMap::new();
            let seeds = std::iter::once((&genesis.founder, Role::Owner))
                .chain(genesis.members.iter().map(|seed| (seed, Role::Member)));
            for (seed, role) in seeds {
                for device in &seed.devices {
                    devices.insert(*device, seed.principal);
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
            });
            return;
        }

        let Some(workforce) = self.workforce.as_mut() else {
            return;
        };
        match &content.body {
            EventBody::Genesis(_)
            | EventBody::InviteCreated(_)
            | EventBody::DeviceBound(_)
            | EventBody::DeviceProfile(_)
            | EventBody::ForkAnnounced(_)
            | EventBody::Unknown { .. } => {}
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
            EventBody::AgentDefined(defined) => {
                let agent = AgentId {
                    principal: content.actor.principal(),
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

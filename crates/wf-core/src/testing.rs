//! Deterministic fixtures for tests in this and downstream crates (feature `testing`).

use crate::crdt::SetChange;
use crate::event::{
    AgentDefined, Change, Comment, DeviceBound, Event, EventBody, EventBuilder, Genesis,
    InviteCreated, InviteKind, ItemCreated, ItemEdited, ItemLinked, ItemRef, ItemTransitioned,
    Link, PolicySet,
};
use crate::id::{ItemId, WorkforceId};
use crate::identity::{Actor, DeviceSigner, InviteSigner, MemberSeed, PrincipalSigner, Signed};
use crate::item::Priority;
use crate::name::{AgentName, Handle, HarnessName, StateName, TypeName};
use crate::policy::DEFAULT_POLICY_TOML;
use crate::state::{State, Trust};

/// A workforce founded by `@joshua` (an owner with several devices) plus a plain member `@ada`.
pub struct Fixture {
    pub founder: PrincipalSigner,
    pub member: PrincipalSigner,
    pub devices: Vec<DeviceSigner>,
    pub member_device: DeviceSigner,
    pub genesis: Event,
}

fn seeded_signer<T>(seed: u8, make: impl Fn(&[u8]) -> T) -> T {
    make(&[seed; 32])
}

impl Fixture {
    pub fn new(founder_devices: usize) -> Self {
        let founder = seeded_signer(1, |bytes| PrincipalSigner::from_bytes(bytes).unwrap());
        let member = seeded_signer(2, |bytes| PrincipalSigner::from_bytes(bytes).unwrap());
        let devices: Vec<DeviceSigner> = (0..founder_devices)
            .map(|index| {
                seeded_signer(10 + index as u8, |bytes| {
                    DeviceSigner::from_bytes(bytes).unwrap()
                })
            })
            .collect();
        let member_device = seeded_signer(100, |bytes| DeviceSigner::from_bytes(bytes).unwrap());
        let genesis = Genesis {
            name: "demo".into(),
            founder: MemberSeed {
                principal: founder.key(),
                handle: Handle::new("joshua").unwrap(),
                devices: devices.iter().map(DeviceSigner::key).collect(),
                previous: vec![],
            },
            members: vec![MemberSeed {
                principal: member.key(),
                handle: Handle::new("ada").unwrap(),
                devices: vec![member_device.key()],
                previous: vec![],
            }],
            policy: DEFAULT_POLICY_TOML.into(),
            forked_from: None,
        };
        let body = EventBody::Genesis(Signed::sign(&founder, None, genesis).unwrap());
        let genesis = EventBuilder::genesis(devices[0].key(), Actor::Human(founder.key()), 1_000)
            .sign(&devices[0], body)
            .unwrap();
        Self {
            founder,
            member,
            devices,
            member_device,
            genesis,
        }
    }

    pub fn workforce(&self) -> WorkforceId {
        self.genesis.id().into()
    }

    /// A fresh replica that has applied genesis.
    pub fn state(&self) -> State {
        let mut state = State::new();
        state
            .receive(self.genesis.clone(), Trust::Untrusted)
            .expect("genesis is valid");
        state
    }

    pub fn founder_actor(&self) -> Actor {
        Actor::Human(self.founder.key())
    }

    /// Authors the founder's next event on `device` as seen by `state`. Does not apply it.
    pub fn author(&self, state: &State, device: usize, now_ms: u64, body: EventBody) -> Event {
        let signer = &self.devices[device];
        state
            .builder(signer.key(), self.founder_actor(), now_ms)
            .expect("state has a genesis")
            .sign(signer, body)
            .unwrap()
    }

    /// Authors and applies in one step; returns the event.
    pub fn apply(&self, state: &mut State, device: usize, now_ms: u64, body: EventBody) -> Event {
        let event = self.author(state, device, now_ms, body);
        state
            .receive(event.clone(), Trust::Untrusted)
            .unwrap_or_else(|error| panic!("event should be valid: {error}"));
        event
    }

    /// An invite for the founder's principal, authored on device 0.
    pub fn invite(&self, state: &State, now_ms: u64, expires_ms: u64) -> (Event, InviteSigner) {
        let secret = InviteSigner::generate();
        let body = EventBody::InviteCreated(
            Signed::sign(
                &self.founder,
                Some(self.workforce()),
                InviteCreated {
                    kind: InviteKind::Device,
                    invite: secret.key(),
                    expires_ms,
                },
            )
            .unwrap(),
        );
        (self.author(state, 0, now_ms, body), secret)
    }

    /// The first event of `joiner`: redeems `invite` for the founder's principal.
    pub fn bind(
        &self,
        state: &State,
        joiner: &DeviceSigner,
        invite: &Event,
        secret: &InviteSigner,
        now_ms: u64,
    ) -> Event {
        let proof = secret
            .prove(self.workforce(), invite.id(), joiner.key())
            .unwrap();
        state
            .builder(joiner.key(), self.founder_actor(), now_ms)
            .expect("state has a genesis")
            .depending_on(invite.id())
            .sign(
                joiner,
                EventBody::DeviceBound(DeviceBound {
                    invite: invite.id(),
                    proof,
                }),
            )
            .unwrap()
    }

    pub fn policy_set(&self, signer: &PrincipalSigner, source: &str) -> EventBody {
        EventBody::PolicySet(
            Signed::sign(
                signer,
                Some(self.workforce()),
                PolicySet {
                    policy: source.into(),
                },
            )
            .unwrap(),
        )
    }
}

pub fn create(item_type: &str, title: &str) -> EventBody {
    EventBody::ItemCreated(ItemCreated {
        item_type: TypeName::new(item_type).unwrap(),
        title: title.into(),
        body: String::new(),
        priority: matches!(item_type, "task" | "question").then_some(Priority::DEFAULT),
        lane: None,
        labels: vec![],
        requires: vec![],
    })
}

pub fn create_with_body(item_type: &str, title: &str, body: &str) -> EventBody {
    let EventBody::ItemCreated(mut created) = create(item_type, title) else {
        unreachable!()
    };
    created.body = body.into();
    EventBody::ItemCreated(created)
}

pub fn transition(item: ItemId, to: &str) -> EventBody {
    EventBody::ItemTransitioned(ItemTransitioned {
        item,
        to: StateName::new(to).unwrap(),
        note: None,
    })
}

pub fn depends_on(item: ItemId, target: ItemId) -> EventBody {
    EventBody::ItemLinked(ItemLinked {
        item,
        link: Link::DependsOn(SetChange::Add(target)),
    })
}

pub fn supersedes(item: ItemId, target: ItemId) -> EventBody {
    EventBody::ItemLinked(ItemLinked {
        item,
        link: Link::Supersedes(target),
    })
}

pub fn edit(item: ItemId, change: Change) -> EventBody {
    EventBody::ItemEdited(ItemEdited {
        item,
        changes: vec![change],
    })
}

pub fn archive(item: ItemId) -> EventBody {
    EventBody::ItemArchived(ItemRef { item })
}

pub fn unarchive(item: ItemId) -> EventBody {
    EventBody::ItemUnarchived(ItemRef { item })
}

pub fn comment(item: ItemId, body: &str) -> EventBody {
    EventBody::Comment(Comment {
        item,
        body: body.into(),
    })
}

pub fn define_agent(name: &str) -> EventBody {
    EventBody::AgentDefined(AgentDefined {
        name: AgentName::new(name).unwrap(),
        harness: HarnessName::new("opencode").unwrap(),
        model: None,
        capabilities: vec![],
        lanes: vec![],
    })
}

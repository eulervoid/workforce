//! Deterministic fixtures for tests in this and downstream crates (feature `testing`).

use crate::autonomy::Autonomy;
use crate::body::StepId;
use crate::crdt::SetChange;
use crate::event::{
    AgentDefined, Change, Comment, CommentKind, DeviceBound, Event, EventBody, EventBuilder,
    ForkPoint, Genesis, InviteCreated, InviteKind, ItemCreated, ItemEdited, ItemLinked, ItemMoved,
    ItemRef, Link, PolicySet, SessionEnded, SessionStarted, WrittenState,
};
use crate::id::{CommentId, ItemId, SessionId, WorkforceId};
use crate::identity::{
    Actor, AgentId, DeviceSigner, InviteSigner, MemberSeed, PrincipalSigner, Signed,
};
pub use crate::item::ItemState;
use crate::item::Priority;
use crate::name::{AgentName, Handle, HarnessName, LaneName, ReviewLevelName, TypeName};
use crate::policy::{DEFAULT_POLICY_TOML, Policy};
use crate::session::{CommitId, EndReason, SessionRole, Verdict};
use crate::state::{State, Trust};

/// A workforce founded by `@ada` (an owner with several devices) plus a plain member `@grace`.
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
                handle: Handle::new("ada").unwrap(),
                devices: devices.iter().map(DeviceSigner::key).collect(),
                previous: vec![],
            },
            members: vec![MemberSeed {
                principal: member.key(),
                handle: Handle::new("grace").unwrap(),
                devices: vec![member_device.key()],
                previous: vec![],
            }],
            policy: DEFAULT_POLICY_TOML.replace("review = \"manual\"\n", ""),
            forked_from: None,
        };
        let body = EventBody::Genesis(Signed::sign(&founder, None, genesis).unwrap());
        let genesis =
            EventBuilder::genesis(devices[0].key(), Actor::Principal(founder.key()), 1_000)
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
        Actor::Principal(self.founder.key())
    }

    /// The founder's agent `name` acting in `session`.
    pub fn agent_actor(&self, name: &str, session: SessionId) -> Actor {
        Actor::Agent {
            agent: AgentId {
                principal: self.founder.key(),
                name: AgentName::new(name).unwrap(),
            },
            session,
        }
    }

    /// Authors the founder's next event on `device` as seen by `state`. Does not apply it.
    pub fn author(&self, state: &State, device: usize, now_ms: u64, body: EventBody) -> Event {
        self.author_as(state, device, self.founder_actor(), now_ms, body)
    }

    /// Like [`Fixture::author`], with another actor for the same principal.
    pub fn author_as(
        &self,
        state: &State,
        device: usize,
        actor: Actor,
        now_ms: u64,
        body: EventBody,
    ) -> Event {
        let signer = &self.devices[device];
        state
            .builder(signer.key(), actor, now_ms)
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

    /// A fork of `parent` made on device 0, leaving device 1 out. The founder is `founder`, a new
    /// principal key when it differs from the one of this fixture.
    pub fn fork(&self, parent: &State, founder: &PrincipalSigner, now_ms: u64) -> Event {
        let old = self.founder.key();
        let genesis = Genesis {
            name: "demo (fork)".into(),
            founder: MemberSeed {
                principal: founder.key(),
                handle: Handle::new("ada").unwrap(),
                devices: vec![self.devices[0].key()],
                previous: if founder.key() == old {
                    vec![]
                } else {
                    vec![old]
                },
            },
            members: vec![MemberSeed {
                principal: self.member.key(),
                handle: Handle::new("grace").unwrap(),
                devices: vec![self.member_device.key()],
                previous: vec![],
            }],
            policy: DEFAULT_POLICY_TOML.into(),
            forked_from: Some(ForkPoint {
                workforce: self.workforce(),
                heads: parent.heads().iter().copied().collect(),
            }),
        };
        let body = EventBody::Genesis(Signed::sign(founder, None, genesis).unwrap());
        parent
            .fork_genesis_builder(
                self.devices[0].key(),
                Actor::Principal(founder.key()),
                now_ms,
            )
            .unwrap()
            .sign(&self.devices[0], body)
            .unwrap()
    }

    /// An invite for the founder's principal, authored on device 0.
    pub fn invite(&self, state: &State, now_ms: u64, expires_ms: u64) -> (Event, InviteSigner) {
        // Seeded by the time, so two invites made at different times differ.
        let mut random = [3; 32];
        random[..8].copy_from_slice(&now_ms.to_le_bytes());
        let secret = InviteSigner::generate(random);
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
        priority: (item_type == "task").then_some(Priority::DEFAULT),
        lane: None,
        labels: vec![],
        requires: vec![],
        review: None,
        state: Policy::default_policy()
            .type_config(&TypeName::new(item_type).unwrap())
            .map(|config| config.initial.into()),
    })
}

pub fn create_in(item_type: &str, title: &str, state: ItemState) -> EventBody {
    let EventBody::ItemCreated(mut created) = create(item_type, title) else {
        unreachable!()
    };
    created.state = Some(state.into());
    EventBody::ItemCreated(created)
}

/// An item as created before `ItemCreated` carried a state.
pub fn create_without_state(item_type: &str, title: &str) -> EventBody {
    let EventBody::ItemCreated(mut created) = create(item_type, title) else {
        unreachable!()
    };
    created.state = None;
    EventBody::ItemCreated(created)
}

pub fn create_with_body(item_type: &str, title: &str, body: &str) -> EventBody {
    let EventBody::ItemCreated(mut created) = create(item_type, title) else {
        unreachable!()
    };
    created.body = body.into();
    EventBody::ItemCreated(created)
}

pub fn moved(item: ItemId, to: ItemState) -> EventBody {
    EventBody::ItemMoved(ItemMoved {
        item,
        to: to.into(),
        by_session: None,
        because: None,
    })
}

/// A move as an older version wrote it, to a state named the way its type did.
pub fn moved_as_written(item: ItemId, to: &str) -> EventBody {
    EventBody::ItemMoved(ItemMoved {
        item,
        to: WrittenState::as_written(to),
        by_session: None,
        because: None,
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

pub fn comment(item: ItemId, text: &str) -> EventBody {
    EventBody::Commented(Comment::plain(item, text))
}

pub fn blocker(item: ItemId, text: &str) -> EventBody {
    EventBody::Commented(Comment {
        kind: CommentKind::Blocker,
        ..Comment::plain(item, text)
    })
}

pub fn change_request(item: ItemId, text: &str) -> EventBody {
    EventBody::Commented(Comment {
        kind: CommentKind::ChangeRequest,
        ..Comment::plain(item, text)
    })
}

pub fn resolving(item: ItemId, text: &str, resolves: &[CommentId]) -> EventBody {
    EventBody::Commented(Comment {
        resolves: resolves.to_vec(),
        ..Comment::plain(item, text)
    })
}

pub fn define_agent(name: &str) -> EventBody {
    EventBody::AgentDefined(AgentDefined {
        name: AgentName::new(name).unwrap(),
        harness: HarnessName::new("opencode").unwrap(),
        model: None,
        instructions: None,
        capabilities: vec![],
        lanes: vec![],
        autonomy: Autonomy::default(),
    })
}

pub fn start_session(agent: &str, item: ItemId) -> EventBody {
    start_session_as(agent, item, SessionRole::Work)
}

pub fn start_session_as(agent: &str, item: ItemId, role: SessionRole) -> EventBody {
    EventBody::SessionStarted(SessionStarted {
        agent: AgentName::new(agent).unwrap(),
        item,
        role,
    })
}

pub fn end_session(session: SessionId, reason: EndReason) -> EventBody {
    EventBody::SessionEnded(SessionEnded {
        session,
        reason,
        because: None,
    })
}

/// A comment about a step of the item's body: a report, when a session writes it.
pub fn on_step(item: ItemId, text: &str, step: StepId) -> EventBody {
    EventBody::Commented(Comment {
        step: Some(step),
        ..Comment::plain(item, text)
    })
}

/// An item that needs the review level `review`.
pub fn create_reviewed(item_type: &str, title: &str, review: &str) -> EventBody {
    let EventBody::ItemCreated(mut created) = create(item_type, title) else {
        unreachable!("`create` makes an `ItemCreated`")
    };
    created.review = Some(ReviewLevelName::new(review).unwrap());
    EventBody::ItemCreated(created)
}

/// An agent in the `supervision` lane.
pub fn define_supervisor(name: &str) -> EventBody {
    let EventBody::AgentDefined(mut defined) = define_agent(name) else {
        unreachable!("`define_agent` makes an `AgentDefined`")
    };
    defined.lanes = vec![LaneName::new("supervision").unwrap()];
    EventBody::AgentDefined(defined)
}

pub fn review_of(item: ItemId, review: Option<&str>) -> EventBody {
    edit(
        item,
        Change::ReviewLevel(review.map(|name| ReviewLevelName::new(name).unwrap())),
    )
}

/// An agent with capabilities.
pub fn define_agent_with(name: &str, capabilities: &[&str]) -> EventBody {
    let EventBody::AgentDefined(mut defined) = define_agent(name) else {
        unreachable!("`define_agent` makes an `AgentDefined`")
    };
    defined.capabilities = capabilities
        .iter()
        .map(|capability| crate::name::Requirement::new(*capability).unwrap())
        .collect();
    EventBody::AgentDefined(defined)
}

/// A session of `agent` that reviews `commit` of `item`.
pub fn start_review(agent: &str, item: ItemId, commit: impl Into<Option<CommitId>>) -> EventBody {
    EventBody::SessionStarted(SessionStarted {
        agent: AgentName::new(agent).unwrap(),
        item,
        role: SessionRole::Review {
            commit: commit.into(),
        },
    })
}

pub fn verdict(item: ItemId, commit: impl Into<Option<CommitId>>, verdict: Verdict) -> EventBody {
    EventBody::Reviewed(crate::event::Review {
        item,
        commit: commit.into(),
        verdict,
        because: None,
    })
}

pub fn merged(item: ItemId, reviewed: CommitId, commit: CommitId) -> EventBody {
    EventBody::Merged(crate::event::Merged {
        item,
        reviewed,
        commit,
    })
}

pub fn check_failure(item: ItemId, text: &str) -> EventBody {
    EventBody::Commented(Comment {
        kind: CommentKind::CheckFailure,
        ..Comment::plain(item, text)
    })
}

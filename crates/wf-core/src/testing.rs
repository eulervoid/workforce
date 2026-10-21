//! Deterministic fixtures for tests in this and downstream crates (feature `testing`).

use std::collections::BTreeMap;

use crate::autonomy::Autonomy;
use crate::body::StepId;
use crate::crdt::SetChange;
use crate::event::{
    AgentDefined, BoundPolicySet, Change, CommentKind, Commented, DeviceBound, DeviceProfile,
    Event, EventBody, EventBuilder, ForkAnnounced, ForkPoint, Genesis, InviteCreated, InviteKind,
    ItemCreated, ItemEdited, ItemLinked, ItemMoved, ItemRef, Link, SessionEnded, SessionStarted,
    WrittenState,
};
use crate::id::{CommentId, EventId, ItemId, SessionId, WorkforceId};
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
        self.fork_at(
            parent,
            parent.heads().iter().copied().collect(),
            founder,
            now_ms,
        )
    }

    /// Like [`Fixture::fork`], at `heads` instead of the heads of `parent`.
    pub fn fork_at(
        &self,
        parent: &State,
        heads: Vec<EventId>,
        founder: &PrincipalSigner,
        now_ms: u64,
    ) -> Event {
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
                heads,
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

    /// A policy signed by `signer` for the event that `device` would write next on `state` at
    /// `now_ms`, for the workforce of `state`.
    pub fn policy_set(
        &self,
        state: &State,
        device: usize,
        now_ms: u64,
        signer: &PrincipalSigner,
        source: &str,
    ) -> EventBody {
        let hlc = state
            .builder(
                self.devices[device].key(),
                Actor::Principal(signer.key()),
                now_ms,
            )
            .expect("state has a genesis")
            .hlc();
        EventBody::BoundPolicySet(
            Signed::sign(
                signer,
                state.workforce_id(),
                BoundPolicySet {
                    policy: source.into(),
                    hlc,
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
    EventBody::Commented(Commented::plain(item, text))
}

pub fn blocker(item: ItemId, text: &str) -> EventBody {
    EventBody::Commented(Commented {
        kind: CommentKind::Blocker,
        ..Commented::plain(item, text)
    })
}

pub fn change_request(item: ItemId, text: &str) -> EventBody {
    EventBody::Commented(Commented {
        kind: CommentKind::ChangeRequest,
        ..Commented::plain(item, text)
    })
}

pub fn resolving(item: ItemId, text: &str, resolves: &[CommentId]) -> EventBody {
    EventBody::Commented(Commented {
        resolves: resolves.to_vec(),
        ..Commented::plain(item, text)
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
        head: None,
    })
}

/// A work session ends with `reason`, naming where its branch is.
pub fn end_session_at(session: SessionId, reason: EndReason, head: CommitId) -> EventBody {
    EventBody::SessionEnded(SessionEnded {
        session,
        reason,
        because: None,
        head: Some(head),
    })
}

/// A work session submits `commit`.
pub fn submit_session(session: SessionId, commit: CommitId) -> EventBody {
    end_session_at(session, EndReason::Submitted, commit)
}

/// A comment about a step of the item's body: a report, when a session writes it.
pub fn on_step(item: ItemId, text: &str, step: StepId) -> EventBody {
    EventBody::Commented(Commented {
        step: Some(step),
        ..Commented::plain(item, text)
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
    defined.lanes = vec![LaneName::supervision()];
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

/// A session of `agent` that reviews `commit` of `item`, for the step without a name.
pub fn start_review(agent: &str, item: ItemId, commit: impl Into<Option<CommitId>>) -> EventBody {
    start_review_step(agent, item, commit, None)
}

/// A session of `agent` that reviews `commit` of `item`, for the step called `step`.
pub fn start_review_step(
    agent: &str,
    item: ItemId,
    commit: impl Into<Option<CommitId>>,
    step: Option<&str>,
) -> EventBody {
    EventBody::SessionStarted(SessionStarted {
        agent: AgentName::new(agent).unwrap(),
        item,
        role: SessionRole::Review {
            commit: commit.into(),
            step: step.map(|name| crate::name::ReviewStepName::new(name).unwrap()),
        },
    })
}

pub fn verdict(item: ItemId, commit: impl Into<Option<CommitId>>, verdict: Verdict) -> EventBody {
    EventBody::Reviewed(crate::event::Reviewed {
        item,
        commit: commit.into(),
        verdict,
        because: None,
        round_limit_reached: None,
    })
}

pub fn merged(item: ItemId, reviewed: CommitId, commit: CommitId) -> EventBody {
    EventBody::Merged(crate::event::Merged {
        item,
        reviewed,
        commit,
        onto: None,
    })
}

/// A merge that moved the target from `onto` to `commit`.
pub fn merged_onto(
    item: ItemId,
    reviewed: CommitId,
    commit: CommitId,
    onto: CommitId,
) -> EventBody {
    EventBody::Merged(crate::event::Merged {
        item,
        reviewed,
        commit,
        onto: Some(onto),
    })
}

pub fn target_moved(commit: CommitId, onto: CommitId) -> EventBody {
    EventBody::TargetMoved(crate::event::TargetMoved { commit, onto })
}

pub fn merge_claimed(item: ItemId, commit: CommitId) -> EventBody {
    EventBody::MergeClaimed(crate::event::MergeClaimed { item, commit })
}

pub fn check_failure(item: ItemId, text: &str) -> EventBody {
    EventBody::Commented(Commented {
        kind: CommentKind::CheckFailure,
        ..Commented::plain(item, text)
    })
}

fn golden_commit(byte: u8) -> CommitId {
    CommitId::Git(format!("{byte:02x}").repeat(20).parse().unwrap())
}

/// A state that holds one of each kind of thing the encoding of [`State`] has a field for, from
/// fixed keys and clocks: a fork of a parent workforce that was taken in completely (so it has
/// aliases and history), with an invite, a device bound by it, device profiles, a fork
/// announcement, both policy registers, agents, sessions of each role, a reviewed and merged
/// item, and items with links, edits, supersession, steps and every kind of comment. The golden
/// tests that pin `SNAPSHOT_FORMAT` and `PROTOCOL_VERSION` encode it together with
/// [`golden_forking_state`]; extend them when `State` grows something they do not reach.
pub fn golden_state() -> State {
    let fixture = Fixture::new(2);
    let mut parent = fixture.state();
    let mut history = vec![fixture.genesis.clone()];
    let mut apply = |state: &mut State, device, now_ms, body| {
        let event = fixture.apply(state, device, now_ms, body);
        history.push(event.clone());
        event
    };
    let step_text = "Pick the units";
    let task: ItemId = apply(
        &mut parent,
        0,
        2_000,
        create_with_body("task", "Parse durations", &format!("- [ ] {step_text}\n")),
    )
    .id()
    .into();
    let other: ItemId = apply(&mut parent, 0, 2_001, create("task", "Print durations"))
        .id()
        .into();
    let archived: ItemId = apply(&mut parent, 1, 2_002, create("task", "Forgotten"))
        .id()
        .into();
    let replaced: ItemId = apply(&mut parent, 1, 2_003, create("task", "Old way"))
        .id()
        .into();
    apply(&mut parent, 0, 2_004, depends_on(task, other));
    apply(&mut parent, 0, 2_005, supersedes(task, replaced));
    apply(&mut parent, 0, 2_006, comment(task, "Start with seconds"));
    apply(&mut parent, 0, 2_007, blocker(task, "Which units?"));
    apply(&mut parent, 0, 2_008, change_request(other, "Add tests"));
    apply(&mut parent, 0, 2_009, check_failure(other, "fmt failed"));
    apply(
        &mut parent,
        0,
        2_010,
        edit(task, Change::Title("Parse durations (units)".into())),
    );
    apply(&mut parent, 0, 2_011, moved(other, ItemState::Open));
    apply(&mut parent, 0, 2_012, archive(archived));
    apply(
        &mut parent,
        0,
        2_013,
        EventBody::DeviceProfile(DeviceProfile {
            name: "laptop".into(),
            hardware: "gpu, os:linux".into(),
            agents: BTreeMap::from([(AgentName::new("coder").unwrap(), 2)]),
            max_sessions: 3,
        }),
    );

    // A new principal founds the fork, so the old one becomes an alias.
    let fork_founder = PrincipalSigner::from_bytes(&[200; 32]).unwrap();
    let genesis = fixture.fork(&parent, &fork_founder, 5_000);
    let mut state = State::new();
    state.receive(genesis, Trust::Untrusted).unwrap();
    loop {
        let wanted = state.missing_parents();
        if wanted.is_empty() {
            break;
        }
        let mut batch: Vec<&Event> = history
            .iter()
            .filter(|event| wanted.contains(&event.id()))
            .collect();
        batch.sort_by_key(|event| std::cmp::Reverse((event.content.hlc, event.id())));
        for event in batch {
            state.receive(event.clone(), Trust::Untrusted).unwrap();
        }
    }

    let fork_actor = Actor::Principal(fork_founder.key());
    let act = |state: &mut State, device, now_ms, body| {
        let event = fixture.author_as(state, device, fork_actor.clone(), now_ms, body);
        state.receive(event.clone(), Trust::Untrusted).unwrap();
        event
    };
    act(&mut state, 0, 6_000, define_agent("coder"));
    act(
        &mut state,
        0,
        6_001,
        define_agent_with("reviewer", &["review", "strong-model"]),
    );
    let session: SessionId = act(&mut state, 0, 6_002, start_session("coder", task))
        .id()
        .into();
    act(
        &mut state,
        0,
        6_003,
        on_step(task, "chose seconds", StepId::of(step_text)),
    );
    act(
        &mut state,
        0,
        6_004,
        edit(
            task,
            Change::StepDone {
                step: StepId::of(step_text),
                done: true,
            },
        ),
    );
    act(&mut state, 0, 6_005, start_session("coder", other));
    act(
        &mut state,
        0,
        6_006,
        end_session(session, EndReason::Released),
    );

    let reviewed: ItemId = act(
        &mut state,
        0,
        7_000,
        create_reviewed("task", "Reviewed work", "critical"),
    )
    .id()
    .into();
    let coding: SessionId = act(&mut state, 0, 7_001, start_session("coder", reviewed))
        .id()
        .into();
    act(&mut state, 0, 7_002, moved(reviewed, ItemState::Review));
    act(
        &mut state,
        0,
        7_003,
        submit_session(coding, golden_commit(1)),
    );
    let reviewing: SessionId = act(
        &mut state,
        0,
        7_004,
        start_review("reviewer", reviewed, golden_commit(1)),
    )
    .id()
    .into();
    let by_reviewer = fixture.author_as(
        &state,
        0,
        Actor::Agent {
            agent: AgentId {
                principal: fork_founder.key(),
                name: AgentName::new("reviewer").unwrap(),
            },
            session: reviewing,
        },
        7_005,
        verdict(reviewed, golden_commit(1), Verdict::Approve),
    );
    state.receive(by_reviewer, Trust::Untrusted).unwrap();
    act(
        &mut state,
        0,
        7_006,
        end_session(reviewing, EndReason::Reviewed),
    );
    act(
        &mut state,
        0,
        7_007,
        merged(reviewed, golden_commit(1), golden_commit(9)),
    );

    let child_workforce: WorkforceId = state.workforce_id().unwrap();
    let secret = InviteSigner::generate([3; 32]);
    let invite = act(
        &mut state,
        0,
        8_000,
        EventBody::InviteCreated(
            Signed::sign(
                &fork_founder,
                Some(child_workforce),
                InviteCreated {
                    kind: InviteKind::Device,
                    invite: secret.key(),
                    expires_ms: 9_000_000,
                },
            )
            .unwrap(),
        ),
    );
    let joiner = DeviceSigner::from_bytes(&[50; 32]).unwrap();
    let proof = secret
        .prove(child_workforce, invite.id(), joiner.key())
        .unwrap();
    let bound = state
        .builder(joiner.key(), fork_actor.clone(), 8_001)
        .unwrap()
        .depending_on(invite.id())
        .sign(
            &joiner,
            EventBody::DeviceBound(DeviceBound {
                invite: invite.id(),
                proof,
            }),
        )
        .unwrap();
    state.receive(bound, Trust::Untrusted).unwrap();
    let announcement = EventBody::ForkAnnounced(
        Signed::sign(
            &fork_founder,
            Some(child_workforce),
            ForkAnnounced {
                to: WorkforceId::from(EventId::of(b"a later fork")),
                members: vec![fork_founder.key(), fixture.member.key()],
            },
        )
        .unwrap(),
    );
    act(&mut state, 0, 8_002, announcement);
    let bound_policy = fixture.policy_set(&state, 0, 8_004, &fork_founder, DEFAULT_POLICY_TOML);
    act(&mut state, 0, 8_004, bound_policy);
    state
}

/// A fork that has not yet taken in the history it names: the other variant of its fork state.
pub fn golden_forking_state() -> State {
    let fixture = Fixture::new(2);
    let parent = fixture.state();
    let genesis = fixture.fork(&parent, &fixture.founder, 5_000);
    let mut state = State::new();
    state.receive(genesis, Trust::Untrusted).unwrap();
    state
}

/// Whether a test that needs more than Fence allows (such as the `AF_ROUTE` socket iroh opens) is
/// skipped here, saying why. Such a test starts with `if skip_in_fence("...") { return; }`.
pub fn skip_in_fence(reason: &str) -> bool {
    let in_fence = std::env::var_os("FENCE_SANDBOX").is_some();
    if in_fence {
        eprintln!("skipped in Fence: {reason}");
    }
    in_fence
}

#[cfg(test)]
mod tests {
    use crate::state::ForkState;

    use super::*;

    /// The golden tests only pin what the golden states hold, so none of it may go missing.
    #[test]
    fn the_golden_state_reaches_every_part_of_the_state() {
        let state = golden_state();
        let workforce = state.workforce.as_ref().unwrap();
        assert!(matches!(workforce.fork, Some(ForkState::Complete { .. })));
        assert!(!workforce.aliases.is_empty());
        assert!(!workforce.profiles.is_empty());
        assert!(!workforce.announcements.is_empty());
        assert!(workforce.bound_policy.is_some());
        assert!(
            workforce
                .invites
                .values()
                .any(|invite| !invite.redeemed_by.is_empty())
        );
        assert!(!workforce.sessions.is_empty());
        assert!(!workforce.sessions_by_item.is_empty());
        assert!(
            state
                .review_status(
                    state
                        .items()
                        .find(|item| item.title() == "Reviewed work")
                        .unwrap()
                        .id()
                )
                .is_some()
        );
        let forking = golden_forking_state();
        assert!(matches!(
            forking.workforce.as_ref().unwrap().fork,
            Some(ForkState::TakingIn { .. })
        ));
    }
}

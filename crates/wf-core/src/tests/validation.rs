use crate::event::{Event, EventBody, EventBuilder, Genesis, ItemRef};
use crate::hlc::Hlc;
use crate::id::{EventId, ItemId, SessionId, WorkforceId};
use crate::identity::{Actor, AgentId, DeviceSigner, PrincipalSigner, Signed};
use crate::item::ItemState;
use crate::name::{AgentName, Handle};
use crate::policy::PolicyError;
use crate::state::{Disposition, State, Trust};
use crate::testing::*;
use crate::validate::{GenesisError, Invalid};

fn receive(state: &mut State, event: Event) -> Result<Disposition, Invalid> {
    state
        .receive(event, Trust::Untrusted)
        .map(|received| received.disposition)
}

fn forged(
    fixture: &Fixture,
    state: &State,
    forge: impl FnOnce(&mut crate::event::EventContent),
) -> Event {
    let mut content = fixture
        .author(state, 0, 5_000, create("task", "forged"))
        .content;
    forge(&mut content);
    Event::sign_content(content, &fixture.devices[0]).unwrap()
}

#[test]
fn tampered_events_are_rejected() {
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let mut event = fixture.author(&state, 0, 5_000, create("task", "x"));
    event.content.hlc = Hlc::next_after(event.content.hlc, 9_999);
    assert_eq!(receive(&mut state, event), Err(Invalid::BadSignature));
}

#[test]
fn unsupported_version() {
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let event = forged(&fixture, &state, |content| content.v = 2);
    assert_eq!(
        receive(&mut state, event),
        Err(Invalid::UnsupportedVersion(2))
    );
}

#[test]
fn workforce_must_match() {
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let missing = forged(&fixture, &state, |content| content.workforce = None);
    assert_eq!(receive(&mut state, missing), Err(Invalid::MissingWorkforce));
    let other = WorkforceId::from(EventId::of(b"other"));
    let wrong = forged(&fixture, &state, |content| content.workforce = Some(other));
    assert_eq!(
        receive(&mut state, wrong),
        Err(Invalid::WrongWorkforce(other))
    );
}

#[test]
fn a_second_genesis_is_rejected() {
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let other = Fixture::new(2);
    assert_eq!(
        receive(&mut state, other.genesis.clone()),
        Err(Invalid::DuplicateGenesis)
    );
    assert_eq!(
        receive(&mut state, fixture.genesis.clone()),
        Ok(Disposition::Duplicate)
    );
}

fn forged_genesis(
    fixture: &Fixture,
    author: &DeviceSigner,
    forge: impl FnOnce(&mut Genesis),
) -> Result<Disposition, Invalid> {
    let EventBody::Genesis(signed) = &fixture.genesis.content.body else {
        unreachable!()
    };
    let mut genesis = signed.statement.clone();
    forge(&mut genesis);
    let body = EventBody::Genesis(Signed::sign(&fixture.founder, None, genesis).unwrap());
    let event = EventBuilder::genesis(author.key(), fixture.founder_actor(), 1_000)
        .sign(author, body)
        .unwrap();
    State::new()
        .receive(event, Trust::Untrusted)
        .map(|received| received.disposition)
}

#[test]
fn genesis_rules() {
    let fixture = Fixture::new(1);
    assert_eq!(
        forged_genesis(&fixture, &fixture.devices[0], |_| {}),
        Ok(Disposition::Applied)
    );
    assert_eq!(
        forged_genesis(&fixture, &fixture.devices[0], |genesis| genesis.name =
            " ".into()),
        Err(GenesisError::EmptyName.into())
    );
    assert_eq!(
        forged_genesis(&fixture, &fixture.devices[0], |genesis| {
            genesis.forked_from = Some(crate::event::ForkPoint {
                workforce: fixture.workforce(),
                heads: vec![],
            })
        }),
        Err(GenesisError::EmptyForkPoint.into())
    );
    assert_eq!(
        forged_genesis(&fixture, &fixture.member_device, |_| {}),
        Err(GenesisError::AuthorNotFounderDevice.into())
    );
    assert_eq!(
        forged_genesis(&fixture, &fixture.devices[0], |genesis| {
            genesis.members[0].handle = Handle::new("ada").unwrap()
        }),
        Err(GenesisError::DuplicateHandle("ada".into()).into())
    );
    assert_eq!(
        forged_genesis(&fixture, &fixture.devices[0], |genesis| {
            genesis.members[0].principal = genesis.founder.principal
        }),
        Err(GenesisError::DuplicatePrincipal(fixture.founder.key()).into())
    );
    assert_eq!(
        forged_genesis(&fixture, &fixture.devices[0], |genesis| {
            genesis.members[0].devices = genesis.founder.devices.clone()
        }),
        Err(GenesisError::DuplicateDevice(fixture.devices[0].key()).into())
    );
}

#[test]
fn genesis_must_be_signed_by_the_founder() {
    let fixture = Fixture::new(1);
    let intruder = PrincipalSigner::from_bytes(&[77; 32]).unwrap();
    let EventBody::Genesis(signed) = &fixture.genesis.content.body else {
        unreachable!()
    };
    let body = EventBody::Genesis(Signed::sign(&intruder, None, signed.statement.clone()).unwrap());
    let event = EventBuilder::genesis(fixture.devices[0].key(), fixture.founder_actor(), 1_000)
        .sign(&fixture.devices[0], body)
        .unwrap();
    assert_eq!(
        State::new().receive(event, Trust::Untrusted).map(|_| ()),
        Err(GenesisError::NotSignedByFounder.into())
    );
}

#[test]
fn events_before_genesis_wait_for_it() {
    let fixture = Fixture::new(1);
    let mut reference = fixture.state();
    let task = fixture.apply(&mut reference, 0, 2_000, create("task", "a"));

    let mut state = State::new();
    assert_eq!(receive(&mut state, task.clone()), Ok(Disposition::Pending));
    assert_eq!(state.pending_events().count(), 1);
    let received = state
        .receive(fixture.genesis.clone(), Trust::Untrusted)
        .unwrap();
    assert_eq!(received.applied.len(), 2);
    assert_eq!(state, reference);
}

#[test]
fn out_of_order_events_wait_for_their_parents() {
    let fixture = Fixture::new(1);
    let mut author = fixture.state();
    let first = fixture.apply(&mut author, 0, 2_000, create("task", "a"));
    let second = fixture.apply(&mut author, 0, 2_001, create("task", "b"));
    let third = fixture.apply(&mut author, 0, 2_002, create("task", "c"));

    let mut state = fixture.state();
    assert_eq!(receive(&mut state, third.clone()), Ok(Disposition::Pending));
    assert_eq!(
        receive(&mut state, third.clone()),
        Ok(Disposition::Duplicate)
    );
    assert_eq!(
        receive(&mut state, second.clone()),
        Ok(Disposition::Pending)
    );
    assert_eq!(state.event_count(), 1);
    let received = state.receive(first, Trust::Untrusted).unwrap();
    assert_eq!(received.disposition, Disposition::Applied);
    assert_eq!(received.applied.len(), 3);
    assert_eq!(state.pending_events().count(), 0);
    assert_eq!(state, author);
    assert_eq!(receive(&mut state, second), Ok(Disposition::Duplicate));
}

#[test]
fn pending_events_that_turn_out_invalid_are_reported() {
    let fixture = Fixture::new(1);
    let mut author = fixture.state();
    let parent = fixture.apply(&mut author, 0, 2_000, create("task", "a"));
    let child = {
        let mut content = fixture
            .author(&author, 0, 2_001, create("task", "b"))
            .content;
        content.actor = Actor::Principal(fixture.member.key());
        Event::sign_content(content, &fixture.devices[0]).unwrap()
    };

    let mut state = fixture.state();
    assert_eq!(receive(&mut state, child.clone()), Ok(Disposition::Pending));
    let received = state.receive(parent, Trust::Untrusted).unwrap();
    assert_eq!(received.applied.len(), 1);
    assert_eq!(
        received.rejected,
        vec![(child.id(), Invalid::ActorMismatch)]
    );
    assert_eq!(state.pending_events().count(), 0);
}

#[test]
fn prev_must_be_the_previous_seq_of_the_same_author() {
    let fixture = Fixture::new(2);
    let mut state = fixture.state();
    let first = fixture.apply(&mut state, 0, 2_000, create("task", "a"));
    let author = fixture.devices[0].key();

    let skipping = forged(&fixture, &state, |content| content.seq += 1);
    assert_eq!(
        receive(&mut state, skipping),
        Err(Invalid::PrevMismatch { author })
    );

    let wrong_prev = forged(&fixture, &state, |content| {
        content.prev = Some(fixture.genesis.id())
    });
    assert_eq!(
        receive(&mut state, wrong_prev),
        Err(Invalid::PrevMismatch { author })
    );

    let missing_prev = forged(&fixture, &state, |content| content.prev = None);
    assert_eq!(
        receive(&mut state, missing_prev),
        Err(Invalid::PrevMismatch { author })
    );

    let unexpected_prev = forged(&fixture, &state, |content| {
        content.seq = 1;
        content.prev = Some(first.id());
    });
    assert_eq!(
        receive(&mut state, unexpected_prev),
        Err(Invalid::PrevMismatch { author })
    );

    let zero = forged(&fixture, &state, |content| content.seq = 0);
    assert_eq!(
        receive(&mut state, zero),
        Err(Invalid::UnexpectedSeq {
            author,
            expected: 1,
            got: 0
        })
    );
}

#[test]
fn a_forked_feed_is_admitted_on_both_branches_and_flagged() {
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let first = fixture.apply(&mut state, 0, 2_000, create("task", "a"));
    assert_eq!(state.equivocations().count(), 0);

    let branch = forged(&fixture, &state, |content| {
        content.seq = first.content.seq;
        content.prev = first.content.prev;
        content.deps = first.content.deps.clone();
        content.hlc = Hlc::next_after(first.content.hlc, 9_000);
    });
    assert_eq!(
        receive(&mut state, branch.clone()),
        Ok(Disposition::Applied)
    );
    let author = fixture.devices[0].key();
    assert_eq!(
        state.equivocations().collect::<Vec<_>>(),
        vec![(author, first.content.seq)]
    );
    assert_eq!(state.feeds()[&author].tips().count(), 2);
    assert_eq!(state.versions()[&author].max_seq, first.content.seq);

    // Receiving the same events in the other order flags the same seq.
    let mut other = fixture.state();
    receive(&mut other, branch).unwrap();
    receive(&mut other, first.clone()).unwrap();
    assert_eq!(
        other.equivocations().collect::<Vec<_>>(),
        state.equivocations().collect::<Vec<_>>()
    );
    assert_eq!(other, state);
}

#[test]
fn hlc_must_follow_everything_the_event_depends_on() {
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let event = forged(&fixture, &state, |content| {
        content.hlc = Hlc::next_after(Hlc::ZERO, 1)
    });
    assert_eq!(receive(&mut state, event), Err(Invalid::HlcNotAfterParents));
}

#[test]
fn author_must_be_a_bound_device_acting_for_its_own_principal() {
    let fixture = Fixture::new(1);
    let mut state = fixture.state();

    let stranger = DeviceSigner::from_bytes(&[55; 32]).unwrap();
    let mut content = fixture
        .author(&state, 0, 5_000, create("task", "x"))
        .content;
    content.author = stranger.key();
    content.seq = 1;
    content.prev = None;
    let event = Event::sign_content(content, &stranger).unwrap();
    assert_eq!(
        receive(&mut state, event),
        Err(Invalid::UnknownDevice(stranger.key()))
    );

    let impersonation = forged(&fixture, &state, |content| {
        content.actor = Actor::Principal(fixture.member.key())
    });
    assert_eq!(
        receive(&mut state, impersonation),
        Err(Invalid::ActorMismatch)
    );

    let ghost = forged(&fixture, &state, |content| {
        content.actor = Actor::Agent {
            agent: AgentId {
                principal: fixture.founder.key(),
                name: AgentName::new("ghost").unwrap(),
            },
            session: SessionId::from(EventId::of(b"s")),
        }
    });
    assert_eq!(receive(&mut state, ghost), Err(Invalid::UnknownAgent));
}

#[test]
fn a_member_may_write_items_but_not_policy() {
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let member_actor = Actor::Principal(fixture.member.key());
    let build = |state: &State, body| {
        state
            .builder(fixture.member_device.key(), member_actor.clone(), 5_000)
            .unwrap()
            .sign(&fixture.member_device, body)
            .unwrap()
    };
    let item = build(&state, create("task", "from a member"));
    assert_eq!(receive(&mut state, item), Ok(Disposition::Applied));

    let policy = build(
        &state,
        fixture.policy_set(&fixture.member, crate::policy::DEFAULT_POLICY_TOML),
    );
    assert_eq!(
        receive(&mut state, policy),
        Err(Invalid::NotOwner(fixture.member.key()))
    );
}

#[test]
fn policy_changes_need_a_valid_principal_signature_for_this_workforce() {
    let fixture = Fixture::new(1);
    let mut state = fixture.state();

    let other_workforce = WorkforceId::from(EventId::of(b"elsewhere"));
    let replayed = EventBody::PolicySet(
        Signed::sign(
            &fixture.founder,
            Some(other_workforce),
            crate::event::PolicySet {
                policy: crate::policy::DEFAULT_POLICY_TOML.into(),
            },
        )
        .unwrap(),
    );
    let event = fixture.author(&state, 0, 5_000, replayed);
    assert_eq!(
        receive(&mut state, event),
        Err(Invalid::BadPrincipalSignature)
    );

    let valid = fixture.author(
        &state,
        0,
        5_001,
        fixture.policy_set(&fixture.founder, crate::policy::DEFAULT_POLICY_TOML),
    );
    assert_eq!(receive(&mut state, valid), Ok(Disposition::Applied));
}

/// A policy written by a newer version, with a recovery step this version does not know.
fn newer_policy() -> String {
    crate::policy::DEFAULT_POLICY_TOML.replace(
        r#"steps = ["retry", "retry", "principal"]"#,
        r#"steps = ["retry", { command = "make reset" }, "principal"]"#,
    )
}

#[test]
fn a_policy_this_version_cannot_read_is_flagged_and_the_last_readable_one_kept() {
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let readable = format!("{}\n# the first edit\n", crate::policy::DEFAULT_POLICY_TOML);
    let first = fixture.author(
        &state,
        0,
        5_000,
        fixture.policy_set(&fixture.founder, &readable),
    );
    assert_eq!(receive(&mut state, first), Ok(Disposition::Applied));

    let newer = fixture.author(
        &state,
        0,
        5_001,
        fixture.policy_set(&fixture.founder, &newer_policy()),
    );
    assert_eq!(receive(&mut state, newer), Ok(Disposition::Applied));
    assert!(matches!(
        state.unreadable_policy(),
        Some(PolicyError::Parse(_))
    ));
    assert_eq!(state.policy().unwrap().source, readable);

    let fixed = fixture.author(
        &state,
        0,
        5_002,
        fixture.policy_set(&fixture.founder, crate::policy::DEFAULT_POLICY_TOML),
    );
    assert_eq!(receive(&mut state, fixed), Ok(Disposition::Applied));
    assert_eq!(state.unreadable_policy(), None);
}

#[test]
fn a_workforce_whose_first_policy_is_unreadable_runs_on_the_default() {
    let fixture = Fixture::new(1);
    let EventBody::Genesis(signed) = &fixture.genesis.content.body else {
        unreachable!()
    };
    let mut genesis = signed.statement.clone();
    genesis.policy = newer_policy();
    let author = &fixture.devices[0];
    let body = EventBody::Genesis(Signed::sign(&fixture.founder, None, genesis).unwrap());
    let event = EventBuilder::genesis(author.key(), fixture.founder_actor(), 1_000)
        .sign(author, body)
        .unwrap();
    let mut state = State::new();
    state.receive(event, Trust::Untrusted).unwrap();
    assert!(state.unreadable_policy().is_some());
    assert_eq!(
        state.policy().unwrap().source,
        crate::policy::DEFAULT_POLICY_TOML
    );
}

#[test]
fn items_must_exist() {
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let ghost = ItemId::from(EventId::of(b"ghost"));
    let real = ItemId::from(
        fixture
            .apply(&mut state, 0, 2_000, create("task", "a"))
            .id(),
    );
    for body in [
        moved(ghost, ItemState::Done),
        comment(ghost, "hello"),
        archive(ghost),
        EventBody::ItemUnarchived(ItemRef { item: ghost }),
        depends_on(real, ghost),
        depends_on(ghost, real),
        supersedes(real, ghost),
    ] {
        let event = fixture.author(&state, 0, 5_000, body);
        assert_eq!(receive(&mut state, event), Err(Invalid::UnknownItem(ghost)));
    }
}

#[test]
fn an_item_cannot_depend_on_itself() {
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let real = ItemId::from(
        fixture
            .apply(&mut state, 0, 2_000, create("task", "a"))
            .id(),
    );
    let event = fixture.author(&state, 0, 5_000, depends_on(real, real));
    assert_eq!(receive(&mut state, event), Err(Invalid::SelfDependency));
}

#[test]
fn unknown_bodies_are_stored_and_ignored() {
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let before = state.items().count();
    fixture.apply(
        &mut state,
        0,
        2_000,
        EventBody::Unknown {
            tag: 9_999,
            payload: vec![1, 2, 3],
        },
    );
    assert_eq!(state.event_count(), 2);
    assert_eq!(state.items().count(), before);
}

#[test]
fn authors_are_checked_against_the_workforce_id() {
    let fixture = Fixture::new(1);
    let state = fixture.state();
    let builder = state
        .builder(fixture.devices[0].key(), fixture.founder_actor(), 5_000)
        .unwrap();
    let event = builder
        .sign(&fixture.devices[0], create("task", "x"))
        .unwrap();
    assert_eq!(event.content.workforce, Some(fixture.workforce()));
    assert_eq!(event.content.seq, 2);
    assert_eq!(event.content.prev, Some(fixture.genesis.id()));
    assert_eq!(event.content.deps, vec![fixture.genesis.id()]);
}

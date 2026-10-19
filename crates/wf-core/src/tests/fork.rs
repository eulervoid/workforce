use crate::event::{Change, EVENT_VERSION, Event, EventBody, EventContent};
use crate::hlc::Hlc;
use crate::id::WorkforceId;
use crate::identity::{Actor, DeviceSigner, PrincipalSigner, Signed};
use crate::item::ItemState;
use crate::state::{Disposition, MAX_PENDING, MAX_PENDING_BINDINGS, State, Trust};
use crate::testing::*;
use crate::validate::Invalid;

struct Scenario {
    fixture: Fixture,
    parent: State,
    history: Vec<Event>,
    genesis: Event,
}

fn scenario(new_principal: bool) -> Scenario {
    let fixture = Fixture::new(2);
    let mut parent = fixture.state();
    let mut history = vec![fixture.genesis.clone()];
    for (index, device) in [0usize, 1, 0, 1, 0].into_iter().enumerate() {
        let body = if index == 2 {
            comment(
                parent.items().next().expect("an item exists").id(),
                "a note from the parent",
            )
        } else {
            create("task", &format!("task {index}"))
        };
        history.push(fixture.apply(&mut parent, device, 2_000 + index as u64, body));
    }
    let first = parent.items().next().expect("an item exists").id();
    let second = parent.items().nth(1).expect("two items exist").id();
    let later = [
        edit(first, Change::Title("renamed in the parent".into())),
        moved(second, ItemState::Done),
        depends_on(first, second),
        comment(second, "a second note"),
        archive(second),
    ];
    for (index, body) in later.into_iter().enumerate() {
        history.push(fixture.apply(&mut parent, index % 2, 3_000 + index as u64, body));
    }
    let founder = if new_principal {
        PrincipalSigner::from_bytes(&[200; 32]).unwrap()
    } else {
        PrincipalSigner::from_bytes(&[1; 32]).unwrap()
    };
    let genesis = fixture.fork(&parent, &founder, 9_000);
    Scenario {
        fixture,
        parent,
        history,
        genesis,
    }
}

fn child(scenario: &Scenario) -> State {
    let mut state = State::new();
    state
        .receive(scenario.genesis.clone(), Trust::Untrusted)
        .unwrap();
    state
}

fn items_of(state: &State) -> Vec<&crate::item::Item> {
    state.items_including_archived().collect()
}

/// Hands the state what it asks for, newest first, until it asks for nothing.
fn fetch_history(state: &mut State, history: &[Event]) {
    loop {
        let wanted = state.missing_parents();
        if wanted.is_empty() {
            return;
        }
        let mut batch: Vec<&Event> = history
            .iter()
            .filter(|event| wanted.contains(&event.id()))
            .collect();
        assert!(!batch.is_empty(), "the history has what is asked for");
        batch.sort_by_key(|event| std::cmp::Reverse((event.content.hlc, event.id())));
        for event in batch {
            state.receive(event.clone(), Trust::Untrusted).unwrap();
        }
    }
}

#[test]
fn a_fork_takes_in_the_history_it_names_by_descent() {
    let scenario = scenario(false);
    let mut state = child(&scenario);
    assert_ne!(state.workforce_id(), Some(scenario.fixture.workforce()));
    assert_eq!(
        state.fork_point().map(|(parent, _)| parent),
        Some(scenario.fixture.workforce())
    );
    assert_eq!(
        state.missing_parents().len(),
        1,
        "only the fork point is named"
    );

    fetch_history(&mut state, &scenario.history);
    assert_eq!(state.items().count(), 4);
    assert_eq!(items_of(&state), items_of(&scenario.parent));
    assert_eq!(state.event_count(), scenario.history.len() + 1);
    assert_eq!(state.heads().len(), 1, "history is not a head of the fork");
    assert_eq!(state.equivocations().count(), 0);
}

#[test]
fn history_not_named_yet_is_refused_until_a_descendant_names_it() {
    let scenario = scenario(false);
    let mut state = child(&scenario);
    let oldest = &scenario.history[1];
    let result = state.receive(oldest.clone(), Trust::Untrusted);
    assert!(
        matches!(result, Err(Invalid::NotInForkHistory(_))),
        "{result:?}"
    );
    assert_eq!(state.event_count(), 1);
    fetch_history(&mut state, &scenario.history);
    assert!(state.contains(&oldest.id()));
}

#[test]
fn parent_events_after_the_fork_point_are_never_admitted() {
    let scenario = scenario(false);
    let mut state = child(&scenario);
    fetch_history(&mut state, &scenario.history);

    let mut later = scenario.parent.clone();
    let after = scenario.fixture.apply(
        &mut later,
        1,
        12_000,
        create("task", "written in the parent later"),
    );
    let result = state.receive(after.clone(), Trust::Untrusted);
    assert!(
        matches!(result, Err(Invalid::NotInForkHistory(_))),
        "{result:?}"
    );
    assert!(!state.contains(&after.id()));
}

#[test]
fn the_history_arrives_in_any_order_it_is_asked_for() {
    let scenario = scenario(false);
    let mut descending = child(&scenario);
    fetch_history(&mut descending, &scenario.history);

    // Asking for each name one event at a time, in the order they are named.
    let mut stepwise = child(&scenario);
    while let Some(next) = stepwise.missing_parents().into_iter().next() {
        let event = scenario
            .history
            .iter()
            .find(|event| event.id() == next)
            .unwrap();
        stepwise.receive(event.clone(), Trust::Untrusted).unwrap();
    }
    assert_eq!(descending, stepwise);
}

#[test]
fn an_excluded_device_keeps_its_history_but_loses_its_standing() {
    let scenario = scenario(false);
    let mut state = child(&scenario);
    fetch_history(&mut state, &scenario.history);
    let excluded = scenario.fixture.devices[1].key();

    assert!(state.principal_of(&excluded).is_none());
    assert!(state.feeds().contains_key(&excluded), "its events stay");
    assert!(state.devices().all(|info| info.device != excluded));

    let own = scenario.fixture.devices[1].clone();
    let event = state
        .builder(own.key(), scenario.fixture.founder_actor(), 20_000)
        .unwrap()
        .sign(&own, create("task", "after the exclusion"))
        .unwrap();
    assert_eq!(
        state
            .receive(event, Trust::Untrusted)
            .map(|r| r.disposition),
        Err(Invalid::UnknownDevice(excluded))
    );
}

#[test]
fn remaining_devices_continue_their_feeds_in_the_fork() {
    let scenario = scenario(false);
    let mut state = child(&scenario);
    fetch_history(&mut state, &scenario.history);
    let device = &scenario.fixture.devices[0];
    let event = state
        .builder(device.key(), scenario.fixture.founder_actor(), 20_000)
        .unwrap()
        .sign(device, create("task", "written in the fork"))
        .unwrap();
    assert_eq!(
        state
            .receive(event, Trust::Untrusted)
            .map(|r| r.disposition),
        Ok(Disposition::Applied)
    );
    assert_eq!(state.equivocations().count(), 0);
}

#[test]
fn a_new_principal_keeps_the_handle_of_events_in_the_history() {
    let scenario = scenario(true);
    let mut state = child(&scenario);
    fetch_history(&mut state, &scenario.history);
    let new = PrincipalSigner::from_bytes(&[200; 32]).unwrap().key();
    let old = scenario.fixture.founder.key();

    assert_eq!(state.member(&old).unwrap().handle.as_str(), "ada");
    assert_eq!(state.member(&new).unwrap().handle.as_str(), "ada");
    assert_eq!(state.workforce.as_ref().unwrap().canonical(&old), new);
    assert_eq!(
        state.principal_of(&scenario.fixture.devices[0].key()),
        Some(new)
    );
}

#[test]
fn only_an_owner_can_announce_a_fork() {
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let fork_id = WorkforceId::from(crate::id::EventId::of(b"child"));
    let body = |signer: &PrincipalSigner| {
        EventBody::ForkAnnounced(
            Signed::sign(
                signer,
                Some(fixture.workforce()),
                crate::event::ForkAnnounced {
                    to: fork_id,
                    members: vec![fixture.founder.key()],
                },
            )
            .unwrap(),
        )
    };
    let announced = fixture.author(&state, 0, 2_000, body(&fixture.founder));
    state.receive(announced, Trust::Untrusted).unwrap();
    assert_eq!(state.announcements().count(), 1);
    assert_eq!(state.announcements().next().unwrap().to, fork_id);

    let by_member = fixture.author(&state, 0, 2_001, body(&fixture.member));
    assert!(matches!(
        state.receive(by_member, Trust::Untrusted),
        Err(Invalid::NotOwner(_)) | Err(Invalid::ActorMismatch)
    ));
}

#[test]
fn a_state_expecting_the_fork_refuses_the_genesis_of_the_parent() {
    let scenario = scenario(false);
    let child_id: WorkforceId = scenario.genesis.id().into();
    let mut state = State::expecting(child_id);

    let result = state.receive(scenario.fixture.genesis.clone(), Trust::Untrusted);
    assert!(
        matches!(result, Err(Invalid::WrongWorkforce(_))),
        "{result:?}"
    );
    let result = state.receive(scenario.history[1].clone(), Trust::Untrusted);
    assert!(
        matches!(result, Err(Invalid::WrongWorkforce(_))),
        "{result:?}"
    );
    assert_eq!(state.event_count(), 0);

    state
        .receive(scenario.genesis.clone(), Trust::Untrusted)
        .unwrap();
    assert_eq!(state.workforce_id(), Some(child_id));
}

#[test]
fn the_items_of_a_fork_are_those_of_the_parent_as_they_were_written() {
    let scenario = scenario(false);
    let mut state = child(&scenario);
    fetch_history(&mut state, &scenario.history);
    assert_eq!(items_of(&state), items_of(&scenario.parent));
    let notes: usize = items_of(&state)
        .iter()
        .map(|item| item.comments().len())
        .sum();
    assert_eq!(notes, 2, "both comments carry over");
    assert!(
        items_of(&state)
            .iter()
            .any(|item| item.title() == "renamed in the parent")
    );
}

#[test]
fn history_in_batches_of_any_size_gives_the_same_state() {
    let scenario = scenario(false);
    let mut reference = child(&scenario);
    fetch_history(&mut reference, &scenario.history);

    for batch_size in [1, 2, 3, 100] {
        let mut state = child(&scenario);
        loop {
            let wanted = state.missing_parents();
            if wanted.is_empty() {
                break;
            }
            let mut batch: Vec<&Event> = scenario
                .history
                .iter()
                .filter(|event| wanted.contains(&event.id()))
                .take(batch_size)
                .collect();
            batch.sort_by_key(|event| std::cmp::Reverse((event.content.hlc, event.id())));
            for event in batch {
                state.receive(event.clone(), Trust::Untrusted).unwrap();
            }
        }
        assert_eq!(items_of(&state), items_of(&reference), "batch {batch_size}");
    }
}

#[test]
fn what_a_kept_device_writes_waits_for_the_history_and_is_not_lost() {
    let scenario = scenario(false);
    let mut writer = child(&scenario);
    fetch_history(&mut writer, &scenario.history);
    let device = &scenario.fixture.devices[0];
    let own = writer
        .builder(device.key(), scenario.fixture.founder_actor(), 20_000)
        .unwrap()
        .sign(device, create("task", "written in the fork"))
        .unwrap();

    let mut state = child(&scenario);
    let received = state.receive(own.clone(), Trust::Untrusted).unwrap();
    assert_eq!(received.disposition, Disposition::Pending);
    assert!(state.history_incomplete());
    fetch_history(&mut state, &scenario.history);
    assert!(!state.history_incomplete());
    assert!(state.contains(&own.id()), "released once the history is in");
    assert_eq!(state.items().count(), 5);
    assert_eq!(state.equivocations().count(), 0);
}

#[test]
fn nothing_can_be_written_until_the_history_has_arrived() {
    let scenario = scenario(false);
    let mut state = child(&scenario);
    let device = &scenario.fixture.devices[0];
    let result = state.builder(device.key(), scenario.fixture.founder_actor(), 20_000);
    assert_eq!(
        result.err(),
        Some(crate::state::BuildError::HistoryIncomplete)
    );
    fetch_history(&mut state, &scenario.history);
    assert!(
        state
            .builder(device.key(), scenario.fixture.founder_actor(), 20_000)
            .is_ok()
    );
}

/// An event of the fork written by `signer` that claims a place in a feed it has no history in.
fn forged_for(
    state: &State,
    signer: &DeviceSigner,
    actor: Actor,
    seq: u64,
    body: EventBody,
) -> Event {
    let content = EventContent {
        v: EVENT_VERSION,
        workforce: state.workforce_id(),
        author: signer.key(),
        actor,
        seq,
        prev: (seq > 1).then(|| crate::id::EventId::of(&seq.to_le_bytes())),
        deps: vec![],
        hlc: Hlc::next_after(Hlc::ZERO, 20_000 + seq).unwrap(),
        body,
    };
    Event::sign_content(content, signer).unwrap()
}

#[test]
fn keys_that_are_not_bound_cannot_park_events_while_history_is_taken_in() {
    let scenario = scenario(false);
    let mut state = child(&scenario);
    let stranger = DeviceSigner::from_bytes(&[77; 32]).unwrap();
    let actor = scenario.fixture.founder_actor();

    let unbound = forged_for(&state, &stranger, actor.clone(), 1, create("task", "spam"));
    assert_eq!(
        state.receive(unbound, Trust::Untrusted).unwrap_err(),
        Invalid::UnknownDevice(stranger.key())
    );
    let wrong_actor = forged_for(
        &state,
        &scenario.fixture.devices[0],
        Actor::Principal(scenario.fixture.member.key()),
        2,
        create("task", "as someone else"),
    );
    assert_eq!(
        state.receive(wrong_actor, Trust::Untrusted).unwrap_err(),
        Invalid::ActorMismatch
    );
    assert_eq!(state.pending_events().count(), 0);

    // A binding is a device's first event; later events of a device that is not bound wait for
    // nothing.
    let proof = scenario
        .fixture
        .invite(&scenario.parent, 4_000, 99_000)
        .1
        .prove(
            scenario.fixture.workforce(),
            crate::id::EventId::of(b"invite"),
            stranger.key(),
        )
        .unwrap();
    let late_binding = forged_for(
        &state,
        &stranger,
        actor,
        2,
        EventBody::DeviceBound(crate::event::DeviceBound {
            invite: crate::id::EventId::of(b"invite"),
            proof,
        }),
    );
    assert_eq!(
        state.receive(late_binding, Trust::Untrusted).unwrap_err(),
        Invalid::BindingNotFirst
    );
}

#[test]
fn events_waiting_for_history_count_against_the_pending_cap() {
    let scenario = scenario(false);
    let mut state = child(&scenario);
    let device = &scenario.fixture.devices[0];
    let actor = scenario.fixture.founder_actor();
    for seq in 1..=MAX_PENDING as u64 {
        let event = forged_for(&state, device, actor.clone(), seq, create("task", "wait"));
        let received = state.receive(event, Trust::Stored).unwrap();
        assert_eq!(received.disposition, Disposition::Pending);
    }
    let one_more = forged_for(
        &state,
        device,
        actor,
        MAX_PENDING as u64 + 1,
        create("task", "one too many"),
    );
    assert_eq!(
        state.receive(one_more, Trust::Stored).unwrap_err(),
        Invalid::PendingFull
    );
}

#[test]
fn a_history_head_with_an_impossible_feed_position_is_refused() {
    let scenario = scenario(false);
    let device = &scenario.fixture.devices[0];
    let actor = scenario.fixture.founder_actor();
    let workforce = Some(scenario.fixture.workforce());
    let malformed = |seq: u64, prev: Option<crate::id::EventId>| {
        let content = EventContent {
            v: EVENT_VERSION,
            workforce,
            author: device.key(),
            actor: actor.clone(),
            seq,
            prev,
            deps: vec![],
            hlc: Hlc::next_after(Hlc::ZERO, 8_000).unwrap(),
            body: create("task", "malformed"),
        };
        Event::sign_content(content, device).unwrap()
    };
    let somewhere = Some(crate::id::EventId::of(b"somewhere"));
    for (head, expected) in [
        (
            malformed(0, somewhere),
            Invalid::UnexpectedSeq {
                author: device.key(),
                expected: 1,
                got: 0,
            },
        ),
        (
            malformed(1, somewhere),
            Invalid::PrevMismatch {
                author: device.key(),
            },
        ),
        (
            malformed(5, None),
            Invalid::PrevMismatch {
                author: device.key(),
            },
        ),
    ] {
        let genesis = scenario.fixture.fork_at(
            &scenario.parent,
            vec![head.id()],
            &scenario.fixture.founder,
            9_000,
        );
        let mut state = State::new();
        state.receive(genesis, Trust::Untrusted).unwrap();
        assert_eq!(state.receive(head, Trust::Untrusted).unwrap_err(), expected);
        assert!(state.history_incomplete());
    }
}

#[test]
fn fresh_keys_cannot_fill_the_pending_events_with_bindings() {
    let scenario = scenario(false);
    let mut state = child(&scenario);
    let actor = scenario.fixture.founder_actor();
    let invite = crate::id::EventId::of(b"invite");
    let binding = |state: &State, index: usize| {
        let stranger = DeviceSigner::from_bytes(&[index as u8 + 1; 32]).unwrap();
        let proof = scenario
            .fixture
            .invite(&scenario.parent, 4_000, 99_000)
            .1
            .prove(scenario.fixture.workforce(), invite, stranger.key())
            .unwrap();
        forged_for(
            state,
            &stranger,
            actor.clone(),
            1,
            EventBody::DeviceBound(crate::event::DeviceBound { invite, proof }),
        )
    };
    for index in 0..MAX_PENDING_BINDINGS {
        let received = state
            .receive(binding(&state, 100 + index), Trust::Untrusted)
            .unwrap();
        assert_eq!(received.disposition, Disposition::Pending);
    }
    assert_eq!(
        state
            .receive(
                binding(&state, 100 + MAX_PENDING_BINDINGS),
                Trust::Untrusted
            )
            .unwrap_err(),
        Invalid::PendingFull
    );
    // Events of bound devices still have room.
    let device = &scenario.fixture.devices[0];
    let event = forged_for(&state, device, actor.clone(), 1, create("task", "legit"));
    assert_eq!(
        state.receive(event, Trust::Untrusted).unwrap().disposition,
        Disposition::Pending
    );
}

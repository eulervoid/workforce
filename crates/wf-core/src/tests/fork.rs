use crate::event::{Change, Event, EventBody};
use crate::id::WorkforceId;
use crate::identity::{PrincipalSigner, Signed};
use crate::state::{Disposition, State, Trust};
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
        moved(second, "done"),
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
    state.raw_items().collect()
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

    assert_eq!(state.member(&old).unwrap().handle.as_str(), "joshua");
    assert_eq!(state.member(&new).unwrap().handle.as_str(), "joshua");
    assert_eq!(state.canonical_principal(&old), new);
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

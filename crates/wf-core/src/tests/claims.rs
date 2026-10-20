use std::time::Duration;

use crate::claims::Presence;
use crate::event::{Event, EventBody};
use crate::id::{ItemId, SessionId};
use crate::identity::Actor;
use crate::name::ReviewStepName;
use crate::session::{CommitId, EndReason, SessionRole};
use crate::state::{State, Trust};
use crate::testing::*;

fn commit(byte: u8) -> CommitId {
    let text: String = std::iter::repeat_n(format!("{byte:02x}"), 20).collect();
    CommitId::Git(text.parse().unwrap())
}

fn review(commit_byte: u8, step: Option<&str>) -> SessionRole {
    SessionRole::Review {
        commit: Some(commit(commit_byte)),
        step: step.map(|name| ReviewStepName::new(name).unwrap()),
    }
}

fn every_role() -> [SessionRole; 4] {
    [
        SessionRole::Work,
        review(1, None),
        SessionRole::Supervise,
        SessionRole::Diagnose,
    ]
}

/// Two devices of `@ada`, an agent and a task.
fn with_task() -> (Fixture, State, ItemId) {
    let fixture = Fixture::new(2);
    let mut state = fixture.state();
    fixture.apply(&mut state, 0, 2_000, define_agent("coder"));
    let task = fixture
        .apply(&mut state, 0, 2_001, create("task", "work"))
        .id()
        .into();
    (fixture, state, task)
}

fn received(state: &State, events: &[&Event]) -> State {
    let mut state = state.clone();
    for event in events {
        state.receive((*event).clone(), Trust::Untrusted).unwrap();
    }
    state
}

/// Each device starts a session in `role` on the task without having seen the other's, and the
/// logs merge in either order.
fn claimed_concurrently(role: &SessionRole) -> (Fixture, State, ItemId, [Event; 2]) {
    let (fixture, state, task) = with_task();
    let start = || start_session_as("coder", task, role.clone());
    // The second device's clock is behind, so the event ids do not decide.
    let first = fixture.author(&state, 0, 3_000, start());
    let second = fixture.author(&state, 1, 2_900, start());
    let forward = received(&state, &[&first, &second]);
    let backward = received(&state, &[&second, &first]);
    assert_eq!(forward, backward);
    (fixture, forward, task, [first, second])
}

fn earlier_first(events: [Event; 2]) -> [Event; 2] {
    let [one, other] = events;
    if (one.content.hlc, one.id()) < (other.content.hlc, other.id()) {
        [one, other]
    } else {
        [other, one]
    }
}

#[test]
fn of_two_concurrent_claims_in_any_role_the_earliest_holds_and_the_other_lost() {
    for role in every_role() {
        let (fixture, state, task, events) = claimed_concurrently(&role);
        let [earlier, later] = earlier_first(events);
        let (earlier_id, later_id) = (SessionId::from(earlier.id()), SessionId::from(later.id()));
        assert_eq!(
            state.holder(task, &role).map(|session| session.id()),
            Some(earlier_id),
            "{role:?}"
        );
        assert_eq!(
            state.outclaimed(later.content.author),
            [later_id],
            "{role:?}"
        );
        assert!(
            state.outclaimed(earlier.content.author).is_empty(),
            "{role:?}"
        );
        assert!(
            state.ensure_unclaimed(task, &role).is_err(),
            "{role:?}: a third session is refused"
        );

        // Once the holder ends, the one that is left holds the claim.
        let mut state = state;
        let device = fixture
            .devices
            .iter()
            .position(|device| device.key() == earlier.content.author)
            .unwrap();
        fixture.apply(
            &mut state,
            device,
            4_000,
            end_session(earlier_id, EndReason::Released),
        );
        assert_eq!(
            state.holder(task, &role).map(|session| session.id()),
            Some(later_id),
            "{role:?}"
        );
        assert!(state.outclaimed(later.content.author).is_empty());
    }
}

#[test]
fn a_claim_in_one_role_leaves_the_others_free() {
    let (fixture, mut state, task) = with_task();
    let work: SessionId = fixture
        .apply(&mut state, 0, 3_000, start_session("coder", task))
        .id()
        .into();
    for role in every_role() {
        let holder = state.holder(task, &role).map(|session| session.id());
        let expected = (role == SessionRole::Work).then_some(work);
        assert_eq!(holder, expected, "{role:?}");
    }
}

#[test]
fn a_review_claims_its_step_of_its_commit() {
    let (fixture, mut state, task) = with_task();
    let start = |role: SessionRole| -> EventBody { start_session_as("coder", task, role) };
    let security: SessionId = fixture
        .apply(&mut state, 0, 3_000, start(review(1, Some("security"))))
        .id()
        .into();
    let held =
        |state: &State, role: SessionRole| state.holder(task, &role).map(|session| session.id());
    assert_eq!(held(&state, review(1, Some("security"))), Some(security));
    assert_eq!(held(&state, review(1, Some("style"))), None, "another step");
    assert_eq!(held(&state, review(1, None)), None, "the unnamed step");
    assert_eq!(
        held(&state, review(2, Some("security"))),
        None,
        "the same step of newer work"
    );

    // Started later on the same step, it lost; on the step of newer work, it holds.
    let late: SessionId = fixture
        .apply(&mut state, 1, 3_001, start(review(1, Some("security"))))
        .id()
        .into();
    let newer: SessionId = fixture
        .apply(&mut state, 1, 3_002, start(review(2, Some("security"))))
        .id()
        .into();
    assert_eq!(state.outclaimed(fixture.devices[1].key()), [late]);
    assert_eq!(held(&state, review(2, Some("security"))), Some(newer));
}

const MINUTE_MS: u64 = 60_000;
const LEASE: Duration = Duration::from_secs(3 * 60);

#[test]
fn a_device_is_gone_once_silent_for_longer_than_the_lease_since_the_daemon_started() {
    let (fixture, _, _) = with_task();
    let device = fixture.devices[1].key();
    let started = 100 * MINUTE_MS;
    let never_heard = Presence::new(started, []);
    assert!(
        !never_heard.gone(device, LEASE, started + 3 * MINUTE_MS),
        "the daemon has not run for a lease yet"
    );
    assert!(never_heard.gone(device, LEASE, started + 3 * MINUTE_MS + 1));

    let heard = Presence::new(started, [(device, started + 10 * MINUTE_MS)]);
    assert!(!heard.gone(device, LEASE, started + 13 * MINUTE_MS));
    assert!(heard.gone(device, LEASE, started + 13 * MINUTE_MS + 1));

    let heard_before_the_start = Presence::new(started, [(device, started - 60 * MINUTE_MS)]);
    assert!(!heard_before_the_start.gone(device, LEASE, started + MINUTE_MS));
}

#[test]
fn a_daemon_takes_over_the_claim_of_a_gone_device_of_its_principal() {
    let (fixture, mut state, task) = with_task();
    let (asking, other) = (fixture.devices[0].key(), fixture.devices[1].key());
    let gone: SessionId = fixture
        .apply(&mut state, 1, 3_000, start_session("coder", task))
        .id()
        .into();
    let started = 10 * MINUTE_MS;
    let silent = Presence::new(started, []);
    let later = started + 4 * MINUTE_MS;
    let claimable = |state: &State, device, presence: &Presence, now_ms| {
        state.claimable(task, &SessionRole::Work, device, presence, now_ms)
    };
    assert_eq!(claimable(&state, asking, &silent, later), Ok(vec![gone]));
    assert!(
        claimable(&state, asking, &silent, started + MINUTE_MS).is_err(),
        "the daemon has not run for a lease"
    );
    let heard = Presence::new(started, [(other, later - MINUTE_MS)]);
    assert!(claimable(&state, asking, &heard, later).is_err());
    assert!(
        claimable(&state, other, &silent, later).is_err(),
        "a device does not take over its own session"
    );
    assert_eq!(
        state.claimable(task, &SessionRole::Supervise, asking, &heard, later),
        Ok(vec![]),
        "nothing claims the role"
    );

    // A longer lease in the policy keeps the claim.
    let source = format!(
        "{}\n[claims]\nlease = \"5m\"\n",
        crate::policy::DEFAULT_POLICY_TOML
    );
    let body = fixture.policy_set(&state, 0, 4_000, &fixture.founder, &source);
    fixture.apply(&mut state, 0, 4_000, body);
    assert!(claimable(&state, asking, &silent, later).is_err());
    assert_eq!(
        claimable(&state, asking, &silent, started + 6 * MINUTE_MS),
        Ok(vec![gone])
    );
}

#[test]
fn a_claim_of_another_principal_is_not_taken_over() {
    let (fixture, mut state, task) = with_task();
    let member = Actor::Principal(fixture.member.key());
    for body in [define_agent("helper"), start_session("helper", task)] {
        let event = state
            .builder(fixture.member_device.key(), member.clone(), 3_000)
            .unwrap()
            .sign(&fixture.member_device, body)
            .unwrap();
        state.receive(event, Trust::Untrusted).unwrap();
    }
    let silent = Presence::new(0, []);
    let claim = state
        .claimable(
            task,
            &SessionRole::Work,
            fixture.devices[0].key(),
            &silent,
            60 * MINUTE_MS,
        )
        .unwrap_err();
    assert_eq!(claim.item, task);
}

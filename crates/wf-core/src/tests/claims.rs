use crate::event::{Event, EventBody};
use crate::id::{ItemId, SessionId};
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

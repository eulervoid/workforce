use crate::claims::Presence;
use crate::event::{Event, EventBody};
use crate::id::{EventId, ItemId};
use crate::identity::Actor;
use crate::item::{ItemEntry, ItemState};
use crate::review::{PendingMerge, ReviewStatus};
use crate::session::CommitId;
use crate::state::{State, Trust};
use crate::target::MoveCause;
use crate::testing::*;
use crate::validate::PolicyViolation;

fn commit(byte: u8) -> CommitId {
    let text: String = std::iter::repeat_n(format!("{byte:02x}"), 20).collect();
    CommitId::Git(text.parse().unwrap())
}

/// Two devices of `@ada` and a task submitted at `commit(1)`, which needs no review: it waits in
/// `review` for its merge.
fn submitted() -> (Fixture, State, ItemId) {
    let fixture = Fixture::new(2);
    let mut state = fixture.state();
    fixture.apply(&mut state, 0, 2_000, define_agent("coder"));
    let task: ItemId = fixture
        .apply(&mut state, 0, 2_001, create("task", "work"))
        .id()
        .into();
    let session = fixture
        .apply(&mut state, 0, 2_002, start_session("coder", task))
        .id()
        .into();
    fixture.apply(&mut state, 0, 2_003, moved(task, ItemState::Review));
    fixture.apply(&mut state, 0, 2_004, submit_session(session, commit(1)));
    (fixture, state, task)
}

fn daemon(fixture: &Fixture) -> Actor {
    Actor::Daemon(fixture.founder.key())
}

/// Writes `body` as the daemon of `device` on `state`.
fn write(
    fixture: &Fixture,
    state: &mut State,
    device: usize,
    now_ms: u64,
    body: EventBody,
) -> Event {
    let event = fixture.author_as(state, device, daemon(fixture), now_ms, body);
    state.receive(event.clone(), Trust::Untrusted).unwrap();
    event
}

fn received(state: &State, events: &[&Event]) -> State {
    let mut state = state.clone();
    for event in events {
        state.receive((*event).clone(), Trust::Untrusted).unwrap();
    }
    state
}

fn merge_of(state: &State, item: ItemId) -> Option<ReviewStatus> {
    state.review_status(item)
}

#[test]
fn the_target_is_the_end_of_the_chain_of_moves() {
    let (fixture, mut state, task) = submitted();
    assert_eq!(state.target(), None, "no move named a tip yet");
    write(
        &fixture,
        &mut state,
        0,
        3_000,
        target_moved(commit(11), commit(10)),
    );
    assert_eq!(state.target(), Some(commit(11)));
    write(
        &fixture,
        &mut state,
        0,
        3_001,
        merged_onto(task, commit(1), commit(12), commit(11)),
    );
    assert_eq!(state.target(), Some(commit(12)));
    assert_eq!(
        state.target_tip().unwrap().cause,
        MoveCause::Merge { item: task }
    );
    // A merge that found the work landed already moved nothing.
    write(
        &fixture,
        &mut state,
        0,
        3_002,
        merged(task, commit(1), commit(12)),
    );
    assert_eq!(state.target(), Some(commit(12)));
    assert_eq!(state.lost_moves(fixture.devices[0].key()).count(), 0);
}

#[test]
fn of_two_moves_onto_the_same_tip_the_earliest_wins_on_every_device() {
    let (fixture, mut state, task) = submitted();
    write(
        &fixture,
        &mut state,
        0,
        3_000,
        target_moved(commit(11), commit(10)),
    );
    // Cut off from each other: device 1 merges the task, and the principal pushes to device 0
    // later.
    let mut merging = state.clone();
    let merge = write(
        &fixture,
        &mut merging,
        1,
        4_000,
        merged_onto(task, commit(1), commit(12), commit(11)),
    );
    let mut pushing = state.clone();
    let push = write(
        &fixture,
        &mut pushing,
        0,
        5_000,
        target_moved(commit(13), commit(11)),
    );
    // A push on top of the lost one loses with it.
    let after = write(
        &fixture,
        &mut pushing,
        0,
        5_001,
        target_moved(commit(14), commit(13)),
    );

    let one = received(&state, &[&merge, &push, &after]);
    let other = received(&state, &[&push, &after, &merge]);
    assert_eq!(one, other);
    assert_eq!(one.target(), Some(commit(12)));
    let lost: Vec<EventId> = one
        .lost_moves(fixture.devices[0].key())
        .map(|step| step.event())
        .collect();
    assert_eq!(lost, sorted([push.id(), after.id()]));
    assert_eq!(one.lost_moves(fixture.devices[1].key()).count(), 0);
    assert!(matches!(
        merge_of(&one, task),
        Some(ReviewStatus::Merged { commit: merge }) if merge == commit(12)
    ));
    // The principal hears of the latest push that lost, until a later one is on the chain.
    let principal = fixture.founder.key();
    let pushes: Vec<EventId> = one
        .lost_pushes(principal)
        .iter()
        .map(|push| push.event())
        .collect();
    assert_eq!(pushes, [after.id()]);
    let mut rebased = one.clone();
    write(
        &fixture,
        &mut rebased,
        0,
        6_000,
        target_moved(commit(15), commit(12)),
    );
    assert_eq!(rebased.lost_pushes(principal), []);
}

fn sorted(mut ids: [EventId; 2]) -> Vec<EventId> {
    ids.sort();
    ids.to_vec()
}

#[test]
fn a_merge_that_lost_a_fork_waits_for_its_merge_again() {
    let (fixture, mut state, task) = submitted();
    write(
        &fixture,
        &mut state,
        0,
        3_000,
        target_moved(commit(11), commit(10)),
    );
    let mut pushing = state.clone();
    let push = write(
        &fixture,
        &mut pushing,
        0,
        4_000,
        target_moved(commit(13), commit(11)),
    );
    // Device 1 merges the task later, onto the same tip, and moves it to done.
    write(
        &fixture,
        &mut state,
        1,
        5_000,
        merged_onto(task, commit(1), commit(12), commit(11)),
    );
    fixture.apply(&mut state, 1, 5_001, moved(task, ItemState::Done));
    let device = fixture.devices[1].key();
    let there = everyone_there(&fixture);
    let lost_merges = |state: &State, device| state.lost_merges(device, &there, NOW_MS);
    assert!(lost_merges(&state, device).is_empty());

    state.receive(push, Trust::Untrusted).unwrap();
    assert_eq!(state.target(), Some(commit(13)));
    assert_eq!(lost_merges(&state, device), vec![task]);
    assert!(lost_merges(&state, fixture.devices[0].key()).is_empty());
    let Some(ItemEntry::Live(live)) = state.item(task) else {
        panic!("the task is live")
    };
    assert_eq!(
        live.merges().count(),
        0,
        "the lost merge is not in the target"
    );
    assert!(live.all_merges().all(|merge| merge.lost()));
    assert_eq!(
        merge_of(&state, task),
        None,
        "needs no review, and is not merged"
    );

    // Its daemon moves it back to review, and it waits for its merge again.
    fixture.apply(&mut state, 1, 6_000, moved(task, ItemState::Review));
    assert!(lost_merges(&state, device).is_empty());
    assert_eq!(
        state.pending_merges(),
        vec![PendingMerge {
            item: task,
            commit: commit(1)
        }]
    );
    // Merged again on the winner, it stands.
    write(
        &fixture,
        &mut state,
        1,
        6_001,
        merged_onto(task, commit(1), commit(14), commit(13)),
    );
    assert_eq!(state.target(), Some(commit(14)));
    assert!(matches!(
        merge_of(&state, task),
        Some(ReviewStatus::Merged { .. })
    ));
}

const NOW_MS: u64 = 10_000_000;

fn everyone_there(fixture: &Fixture) -> Presence {
    Presence::new(
        0,
        fixture.devices.iter().map(|device| (device.key(), NOW_MS)),
    )
}

#[test]
fn a_device_of_the_principal_sends_back_a_lost_merge_once_its_maker_is_gone() {
    let (fixture, mut state, task) = submitted();
    write(
        &fixture,
        &mut state,
        0,
        3_000,
        target_moved(commit(11), commit(10)),
    );
    let push = fixture.author_as(
        &state,
        0,
        daemon(&fixture),
        4_000,
        target_moved(commit(13), commit(11)),
    );
    write(
        &fixture,
        &mut state,
        1,
        5_000,
        merged_onto(task, commit(1), commit(12), commit(11)),
    );
    fixture.apply(&mut state, 1, 5_001, moved(task, ItemState::Done));
    state.receive(push, Trust::Untrusted).unwrap();
    let [other, maker] = [0, 1].map(|index| fixture.devices[index].key());

    // While the maker is there, it alone sends the item back.
    let there = everyone_there(&fixture);
    assert_eq!(state.lost_merges(maker, &there, NOW_MS), vec![task]);
    assert!(state.lost_merges(other, &there, NOW_MS).is_empty());
    // Once it is gone, which may be for good, the other device of the principal does.
    let lease_ms = u64::try_from(state.lease().as_millis()).unwrap();
    let maker_gone = Presence::new(0, [(other, NOW_MS), (maker, NOW_MS - lease_ms - 1)]);
    assert_eq!(state.lost_merges(other, &maker_gone, NOW_MS), vec![task]);
}

#[test]
fn the_chain_starts_at_the_earliest_move() {
    let (fixture, state, _) = submitted();
    // Two devices whose targets were apart before the log named a tip.
    let mut first = state.clone();
    let early = write(
        &fixture,
        &mut first,
        0,
        3_000,
        target_moved(commit(11), commit(10)),
    );
    let mut second = state.clone();
    let late = write(
        &fixture,
        &mut second,
        1,
        3_500,
        target_moved(commit(21), commit(20)),
    );
    let both = received(&state, &[&late, &early]);
    assert_eq!(both.target(), Some(commit(11)));
    assert_eq!(
        both.lost_moves(fixture.devices[1].key())
            .map(|step| step.event())
            .collect::<Vec<_>>(),
        vec![late.id()]
    );
}

#[test]
fn moves_that_go_round_in_a_circle_are_followed_round_it() {
    let (fixture, mut state, _) = submitted();
    write(
        &fixture,
        &mut state,
        0,
        3_000,
        target_moved(commit(11), commit(10)),
    );
    write(
        &fixture,
        &mut state,
        0,
        3_001,
        target_moved(commit(12), commit(11)),
    );
    write(
        &fixture,
        &mut state,
        0,
        3_002,
        target_moved(commit(11), commit(12)),
    );
    assert_eq!(state.target(), Some(commit(11)));
    assert_eq!(state.lost_moves(fixture.devices[0].key()).count(), 0);
}

#[test]
fn a_circle_before_the_log_named_a_tip_leaves_the_target_where_it_went() {
    let (fixture, state, _) = submitted();
    // The target is at 11: the principal deletes it, pushes 10 and then 11 again, and then a
    // commit on top of it.
    let mut pushing = state.clone();
    let moves = [
        (3_000, commit(10), commit(11)),
        (3_001, commit(11), commit(10)),
        (3_002, commit(12), commit(11)),
    ]
    .map(|(now_ms, moved_to, onto)| {
        write(
            &fixture,
            &mut pushing,
            0,
            now_ms,
            target_moved(moved_to, onto),
        )
    });
    assert_eq!(pushing.target(), Some(commit(12)));
    assert_eq!(pushing.lost_moves(fixture.devices[0].key()).count(), 0);
    let [first, second, third] = &moves;
    assert_eq!(received(&state, &[third, second, first]), pushing);
}

#[test]
fn the_tip_of_the_target_is_among_the_commits_a_device_fetches() {
    let (fixture, mut state, _) = submitted();
    write(
        &fixture,
        &mut state,
        1,
        3_000,
        target_moved(commit(11), commit(10)),
    );
    let named = state.named_commits();
    let target = named
        .iter()
        .find(|named| named.naming == crate::commits::Naming::Target)
        .unwrap();
    assert_eq!(
        (target.commit, target.author),
        (commit(11), fixture.devices[1].key())
    );
}

#[test]
fn the_earliest_claim_of_a_device_that_is_there_holds_the_merge() {
    let (fixture, mut state, task) = submitted();
    let [first, second] = [0, 1].map(|index| fixture.devices[index].key());
    let lease = state.lease();
    let now_ms = 10_000_000;
    let lease_ms = u64::try_from(lease.as_millis()).unwrap();
    let everyone_there = Presence::new(0, [(first, now_ms), (second, now_ms)]);
    assert!(
        state
            .merge_holder(task, commit(1), second, &everyone_there, now_ms)
            .is_none()
    );

    let mut racing = state.clone();
    let late = write(
        &fixture,
        &mut racing,
        1,
        4_000,
        merge_claimed(task, commit(1)),
    );
    write(
        &fixture,
        &mut state,
        0,
        3_000,
        merge_claimed(task, commit(1)),
    );
    state.receive(late, Trust::Untrusted).unwrap();
    for asking in [first, second] {
        let holder = state
            .merge_holder(task, commit(1), asking, &everyone_there, now_ms)
            .unwrap();
        assert_eq!(holder.device, first);
    }
    // A claim on another commit holds nothing of this one.
    assert!(
        state
            .merge_holder(task, commit(2), second, &everyone_there, now_ms)
            .is_none()
    );

    // The first device is gone: the second holds it, as the first still does on its own.
    let first_gone = Presence::new(0, [(first, now_ms - lease_ms - 1), (second, now_ms)]);
    let holder = |asking| {
        state
            .merge_holder(task, commit(1), asking, &first_gone, now_ms)
            .unwrap()
            .device
    };
    assert_eq!(holder(second), second);
    assert_eq!(holder(first), first);
}

#[test]
fn an_agent_writes_no_move_and_no_claim() {
    let (fixture, mut state, task) = submitted();
    let session = fixture
        .apply(&mut state, 0, 3_000, start_session("coder", task))
        .id()
        .into();
    let agent = fixture.agent_actor("coder", session);
    for body in [
        target_moved(commit(11), commit(10)),
        merge_claimed(task, commit(1)),
    ] {
        assert!(matches!(
            state.check_actor(fixture.devices[0].key(), &agent, &body),
            Err(PolicyViolation::AgentCannotWrite(_))
        ));
    }
}

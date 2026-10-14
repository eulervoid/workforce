use crate::id::{ItemId, SessionId};
use crate::identity::Actor;
use crate::item::ItemState;
use crate::lifecycle::{
    AfterEnd, Closing, Finish, Interruption, MergeFailure, SessionEnd, TurnOutcome,
};
use crate::session::{CommitId, EndReason};
use crate::state::State;
use crate::testing::*;

fn commit() -> CommitId {
    CommitId::Git("00112233445566778899aabbccddeeff00112233".parse().unwrap())
}

struct Scene {
    fixture: Fixture,
    state: State,
    clock: u64,
}

impl Scene {
    fn new() -> Self {
        let fixture = Fixture::new(1);
        let mut state = fixture.state();
        fixture.apply(&mut state, 0, 2_000, define_agent("coder"));
        Self {
            fixture,
            state,
            clock: 3_000,
        }
    }

    fn apply(&mut self, body: crate::event::EventBody) -> crate::event::Event {
        self.clock += 1;
        self.fixture.apply(&mut self.state, 0, self.clock, body)
    }

    fn item(&mut self, body: crate::event::EventBody) -> ItemId {
        self.apply(body).id().into()
    }

    fn start(&mut self, item: ItemId) -> SessionId {
        self.apply(start_session("coder", item)).id().into()
    }

    fn end(&mut self, session: SessionId, reason: EndReason) {
        self.apply(end_session(session, reason));
    }
}

#[test]
fn a_work_session_moves_an_item_to_the_active_state_of_its_type() {
    let mut scene = Scene::new();
    let task = scene.item(create("task", "work"));
    let decision = scene.item(create("decision", "why"));
    let task_session = scene.start(task);
    let decision_session = scene.start(decision);
    assert_eq!(
        scene.state.start_move(task_session),
        Some((task, ItemState::Active))
    );
    assert_eq!(
        scene.state.start_move(decision_session),
        None,
        "a decision has no active state"
    );
}

#[test]
fn ending_a_session_moves_its_item_by_why_it_ended() {
    let mut scene = Scene::new();
    let task = scene.item(create("task", "work"));
    let reviewed = scene.item(create_reviewed("task", "careful", "standard"));
    let unreviewed = scene.item(create_reviewed("task", "finding", "none"));
    let submitted = EndReason::Submitted {
        commit: Some(commit()),
    };
    let after = |scene: &Scene, session, reason| scene.state.after_end(session, reason).unwrap();

    let session = scene.start(task);
    scene.apply(moved(task, ItemState::Active));
    assert_eq!(
        after(&scene, session, submitted),
        AfterEnd::MergeThenDone {
            wait_in: Some(ItemState::Review)
        }
    );
    scene.end(session, submitted);

    let session = scene.start(reviewed);
    assert_eq!(
        after(&scene, session, submitted),
        AfterEnd::MoveTo(ItemState::Review)
    );
    scene.end(session, submitted);

    let session = scene.start(unreviewed);
    assert_eq!(
        after(&scene, session, EndReason::Submitted { commit: None }),
        AfterEnd::MoveTo(ItemState::Done)
    );
    scene.end(session, EndReason::Released);

    let doubted = scene.item(create_reviewed("task", "unsure", "manual"));
    let session = scene.start(doubted);
    assert_eq!(
        after(&scene, session, EndReason::Submitted { commit: None }),
        AfterEnd::MoveTo(ItemState::Review),
        "work without commits under review is not done yet"
    );
    scene.end(session, EndReason::Released);

    let blocked = scene.start(task);
    for reason in [
        EndReason::Released,
        EndReason::TimedOut,
        EndReason::Failed,
        EndReason::Cancelled,
    ] {
        scene.apply(moved(task, ItemState::Active));
        assert_eq!(
            after(&scene, blocked, reason),
            AfterEnd::MoveTo(ItemState::Open),
            "{reason:?}"
        );
    }
    assert_eq!(
        after(&scene, blocked, EndReason::Supervised),
        AfterEnd::Nothing
    );
}

#[test]
fn an_item_that_is_not_being_worked_on_stays_where_it_is() {
    let mut scene = Scene::new();
    let task = scene.item(create("task", "work"));
    let session = scene.start(task);
    scene.apply(moved(task, ItemState::Done));
    assert_eq!(
        scene.state.after_end(session, EndReason::Failed),
        Some(AfterEnd::Nothing),
        "a stale session does not reopen finished work"
    );
}

#[test]
fn an_item_another_session_holds_is_not_reopened() {
    let mut scene = Scene::new();
    let task = scene.item(create("task", "work"));
    scene.apply(moved(task, ItemState::Active));
    let _first = scene.start(task);
    let second = scene.start(task);
    assert_eq!(
        scene.state.after_end(second, EndReason::Cancelled),
        Some(AfterEnd::Nothing),
        "the earlier session holds the item, the later one gives way"
    );
}

#[test]
fn a_turn_continues_until_the_nudges_are_used_up() {
    let mut scene = Scene::new();
    let task = scene.item(create("task", "work"));
    let session = scene.start(task);
    scene.apply(moved(task, ItemState::Active));
    let outcome = |scene: &Scene, used| scene.state.turn_outcome(session, used, 3);
    assert_eq!(outcome(&scene, 0), TurnOutcome::Continue);
    assert_eq!(outcome(&scene, 3), TurnOutcome::Continue);
    assert_eq!(outcome(&scene, 4), TurnOutcome::GiveUp);
}

#[test]
fn a_turn_closes_when_the_session_ended_or_the_item_is_blocked() {
    let mut scene = Scene::new();
    let task = scene.item(create("task", "work"));
    let session = scene.start(task);
    for moved_on in [ItemState::Review, ItemState::Cancelled, ItemState::Draft] {
        scene.apply(moved(task, moved_on));
        assert_eq!(
            scene.state.turn_outcome(session, 0, 3),
            TurnOutcome::Close,
            "{moved_on}"
        );
    }

    scene.apply(moved(task, ItemState::Active));
    assert_eq!(
        scene.state.turn_outcome(session, 0, 3),
        TurnOutcome::Continue
    );
    scene.end(session, EndReason::Released);
    assert_eq!(scene.state.turn_outcome(session, 0, 3), TurnOutcome::Close);
}

#[test]
fn a_turn_parks_while_the_item_waits_on_a_dependency() {
    let mut scene = Scene::new();
    let task = scene.item(create("task", "work"));
    let session = scene.start(task);
    scene.apply(moved(task, ItemState::Active));
    let first = scene.item(create("task", "pick a database"));
    scene.apply(depends_on(task, first));
    assert_eq!(scene.state.turn_outcome(session, 0, 3), TurnOutcome::Park);

    scene.apply(moved(first, ItemState::Done));
    assert_eq!(
        scene.state.turn_outcome(session, 0, 3),
        TurnOutcome::Continue
    );
}

#[test]
fn an_agent_finish_is_written_by_the_agent_and_an_interruption_by_the_daemon() {
    let mut scene = Scene::new();
    let task = scene.item(create("task", "work"));
    let session = scene.start(task);
    let by_agent = Actor::Agent {
        agent: scene.state.session(session).unwrap().agent().clone(),
        session,
    };
    let by_daemon = Actor::Daemon(scene.fixture.founder.key());
    let closing = |end| scene.state.closing(session, &end, Some(commit())).unwrap();

    let submitted = closing(SessionEnd::Finish(Finish::Submit {
        note: Some("done".to_string()),
    }));
    assert_eq!(
        submitted,
        Closing {
            reason: EndReason::Submitted {
                commit: Some(commit())
            },
            actor: by_agent.clone(),
            note: Some("done".to_string()),
        }
    );
    let released = closing(SessionEnd::Finish(Finish::Release { note: None }));
    assert_eq!(
        (released.reason, released.actor),
        (EndReason::Released, by_agent)
    );
    let stopped = closing(SessionEnd::Interruption(Interruption::Stop));
    assert_eq!(
        (stopped.reason, stopped.actor, stopped.note),
        (EndReason::Cancelled, by_daemon.clone(), None)
    );
    let failed = closing(SessionEnd::Interruption(Interruption::Failed {
        because: "the harness exited".to_string(),
    }));
    assert_eq!(
        failed.note.as_deref(),
        Some("The session failed: the harness exited")
    );
}

#[test]
fn giving_up_says_what_was_missing_and_quotes_the_agent_shortened() {
    let mut scene = Scene::new();
    let task = scene.item(create("task", "work"));
    let session = scene.start(task);
    let gave_up = |last_message: String| {
        scene
            .state
            .closing(
                session,
                &SessionEnd::Interruption(Interruption::GaveUp {
                    turns: 3,
                    last_message,
                }),
                None,
            )
            .unwrap()
    };
    let silent = gave_up("  ".to_string());
    assert_eq!(silent.reason, EndReason::Released);
    assert_eq!(
        silent.note.as_deref(),
        Some("Gave up: 3 turns in a row without a new commit.")
    );
    let long = gave_up("é".repeat(700)).note.unwrap();
    assert!(long.ends_with(&format!("{}…", "é".repeat(600))), "{long}");
}

#[test]
fn work_that_conflicts_reopens_and_work_that_may_merge_later_waits_in_review() {
    let mut scene = Scene::new();
    let task = scene.item(create("task", "work"));
    assert_eq!(
        scene.state.after_failed_merge(task, MergeFailure::Conflict),
        Some(ItemState::Open)
    );
    assert_eq!(
        scene
            .state
            .after_failed_merge(task, MergeFailure::Transient),
        Some(ItemState::Review)
    );
}

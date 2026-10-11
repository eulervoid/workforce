use crate::id::{ItemId, SessionId};
use crate::lifecycle::{AfterEnd, TurnOutcome};
use crate::name::StateName;
use crate::session::{CommitId, EndReason};
use crate::state::State;
use crate::testing::*;

fn state_name(name: &str) -> StateName {
    StateName::new(name).unwrap()
}

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
    let question = scene.item(create("question", "why"));
    let task_session = scene.start(task);
    let question_session = scene.start(question);
    assert_eq!(
        scene.state.start_move(task_session),
        Some((task, state_name("in_progress")))
    );
    assert_eq!(scene.state.start_move(question_session), None);
}

#[test]
fn ending_a_session_moves_its_item_by_why_it_ended() {
    let mut scene = Scene::new();
    let task = scene.item(create("task", "work"));
    let reviewed = scene.item(create_reviewed("task", "careful", "standard"));
    let question = scene.item(create("question", "why"));
    let after = |scene: &Scene, session| scene.state.after_end(session).unwrap();
    let submitted = EndReason::Submitted {
        commit: Some(commit()),
    };

    let session = scene.start(task);
    assert_eq!(scene.state.after_end(session), None, "still live");
    scene.end(session, submitted);
    assert_eq!(
        after(&scene, session),
        AfterEnd::MergeThen {
            done: state_name("done")
        }
    );

    let session = scene.start(reviewed);
    scene.end(session, submitted);
    assert_eq!(
        after(&scene, session),
        AfterEnd::MoveTo(state_name("in_review"))
    );

    let session = scene.start(question);
    scene.end(session, EndReason::Submitted { commit: None });
    assert_eq!(
        after(&scene, session),
        AfterEnd::MoveTo(state_name("answered"))
    );

    for reason in [
        EndReason::Released,
        EndReason::TimedOut,
        EndReason::Failed,
        EndReason::Cancelled,
    ] {
        let session = scene.start(task);
        scene.end(session, reason);
        assert_eq!(after(&scene, session), AfterEnd::MoveTo(state_name("open")));
    }

    let session = scene.start(task);
    scene.end(session, EndReason::Blocked);
    assert_eq!(after(&scene, session), AfterEnd::Nothing);
}

#[test]
fn an_item_another_session_holds_is_not_reopened() {
    let mut scene = Scene::new();
    let task = scene.item(create("task", "work"));
    let first = scene.start(task);
    let _second = scene.start(task);
    scene.end(first, EndReason::Cancelled);
    assert_eq!(scene.state.after_end(first), Some(AfterEnd::Nothing));
}

#[test]
fn a_turn_continues_until_the_nudges_are_used_up() {
    let mut scene = Scene::new();
    let task = scene.item(create("task", "work"));
    let session = scene.start(task);
    scene.apply(transition(task, "in_progress"));
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
    scene.apply(transition(task, "in_review"));
    assert_eq!(scene.state.turn_outcome(session, 0, 3), TurnOutcome::Close);

    scene.apply(transition(task, "in_progress"));
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
    scene.apply(transition(task, "in_progress"));
    let question = scene.item(create("question", "which database?"));
    scene.apply(depends_on(task, question));
    assert_eq!(scene.state.turn_outcome(session, 0, 3), TurnOutcome::Park);

    scene.apply(transition(question, "answered"));
    assert_eq!(
        scene.state.turn_outcome(session, 0, 3),
        TurnOutcome::Continue
    );
}

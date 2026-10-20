use crate::event::{Commented, EventBody};
use crate::id::{ItemId, SessionId};
use crate::identity::Actor;
use crate::session::{CommitId, EndReason, SessionRole};
use crate::state::{State, Trust};
use crate::testing::*;
use crate::validate::Invalid;

fn commit(byte: u8) -> CommitId {
    let text: String = std::iter::repeat_n(format!("{byte:02x}"), 20).collect();
    CommitId::Git(text.parse().unwrap())
}

/// A founder with two devices and an agent `coder`; the work sessions run on device 0.
struct Scene {
    fixture: Fixture,
    state: State,
    clock: u64,
}

impl Scene {
    fn new() -> Self {
        let fixture = Fixture::new(2);
        let mut state = fixture.state();
        fixture.apply(&mut state, 0, 2_000, define_agent("coder"));
        Self {
            fixture,
            state,
            clock: 3_000,
        }
    }

    fn apply(&mut self, body: EventBody) -> crate::event::Event {
        self.clock += 1;
        self.fixture.apply(&mut self.state, 0, self.clock, body)
    }

    fn try_as(&mut self, device: usize, actor: Actor, body: EventBody) -> Result<(), Invalid> {
        self.clock += 1;
        let event = self
            .fixture
            .author_as(&self.state, device, actor, self.clock, body);
        self.state.receive(event, Trust::Untrusted).map(|_| ())
    }

    fn item(&mut self) -> ItemId {
        self.apply(create("task", "work")).id().into()
    }

    fn start(&mut self, item: ItemId, role: SessionRole) -> SessionId {
        self.apply(start_session_as("coder", item, role))
            .id()
            .into()
    }

    fn agent(&self, session: SessionId) -> Actor {
        Actor::Agent {
            agent: self.state.session(session).unwrap().agent().clone(),
            session,
        }
    }

    fn daemon(&self) -> Actor {
        Actor::Daemon(self.fixture.founder.key())
    }

    fn comment_naming(&mut self, session: SessionId, item: ItemId, head: CommitId) {
        let actor = self.agent(session);
        self.try_as(0, actor, comment_with_head(item, head))
            .unwrap();
    }
}

fn comment_with_head(item: ItemId, head: CommitId) -> EventBody {
    EventBody::Commented(Commented {
        head: Some(head),
        ..Commented::plain(item, "step done")
    })
}

#[test]
fn a_session_has_the_last_head_the_log_names() {
    let mut scene = Scene::new();
    let item = scene.item();
    let session = scene.start(item, SessionRole::Work);
    assert_eq!(scene.state.session(session).unwrap().head(), None);

    scene.comment_naming(session, item, commit(1));
    scene.comment_naming(session, item, commit(2));
    assert_eq!(
        scene.state.session(session).unwrap().head(),
        Some(commit(2))
    );

    scene.apply(end_session_at(session, EndReason::Released, commit(3)));
    assert_eq!(
        scene.state.session(session).unwrap().head(),
        Some(commit(3))
    );
}

#[test]
fn an_end_without_a_head_keeps_the_one_a_comment_named() {
    let mut scene = Scene::new();
    let item = scene.item();
    let session = scene.start(item, SessionRole::Work);
    scene.comment_naming(session, item, commit(1));
    scene.apply(end_session(session, EndReason::Released));
    assert_eq!(
        scene.state.session(session).unwrap().head(),
        Some(commit(1))
    );
}

#[test]
fn the_submission_is_the_head_of_the_session_that_submitted() {
    let mut scene = Scene::new();
    let item = scene.item();
    let session = scene.start(item, SessionRole::Work);
    scene.comment_naming(session, item, commit(1));
    scene.apply(submit_session(session, commit(2)));
    let (submitted, _) = scene.state.submission(item).unwrap();
    assert_eq!(submitted, Some(commit(2)));

    let empty = scene.item();
    let session = scene.start(empty, SessionRole::Work);
    scene.apply(end_session(session, EndReason::Submitted));
    let (submitted, _) = scene.state.submission(empty).unwrap();
    assert_eq!(submitted, None, "work submitted without commits");
}

#[test]
fn the_head_of_an_item_is_that_of_its_latest_work_session_that_named_one() {
    let mut scene = Scene::new();
    let item = scene.item();
    assert_eq!(scene.state.work_head(item), None);
    let first = scene.start(item, SessionRole::Work);
    scene.apply(end_session_at(first, EndReason::Released, commit(1)));
    assert_eq!(scene.state.work_head(item), Some(commit(1)));

    let second = scene.start(item, SessionRole::Work);
    assert_eq!(
        scene.state.work_head(item),
        Some(commit(1)),
        "a session that named none yet leaves the item's"
    );
    scene.comment_naming(second, item, commit(2));
    assert_eq!(scene.state.work_head(item), Some(commit(2)));
}

#[test]
fn a_head_is_named_only_from_the_device_that_runs_the_work_session() {
    let mut scene = Scene::new();
    let item = scene.item();
    let session = scene.start(item, SessionRole::Work);
    let ending = end_session_at(session, EndReason::Released, commit(1));

    // The principal's other device does not run the session.
    let refused = scene.try_as(1, scene.daemon(), ending.clone());
    assert_eq!(refused, Err(Invalid::HeadNotOfItsWorkSession));
    assert_eq!(scene.state.session(session).unwrap().head(), None);
    scene.try_as(0, scene.daemon(), ending).unwrap();
    assert_eq!(
        scene.state.session(session).unwrap().head(),
        Some(commit(1))
    );
}

#[test]
fn a_head_is_named_only_on_a_comment_of_a_work_session_on_its_item() {
    let mut scene = Scene::new();
    let item = scene.item();
    let other = scene.item();
    let work = scene.start(item, SessionRole::Work);
    let supervising = scene.start(item, SessionRole::Supervise);

    let principal = scene.fixture.founder_actor();
    let supervisor = scene.agent(supervising);
    let worker = scene.agent(work);
    for (actor, on) in [
        (principal, item),
        (supervisor, item),
        (worker.clone(), other),
    ] {
        let refused = scene.try_as(0, actor, comment_with_head(on, commit(1)));
        assert_eq!(refused, Err(Invalid::HeadNotOfItsWorkSession));
    }
    scene
        .try_as(0, worker, comment_with_head(item, commit(1)))
        .unwrap();

    // A session that does no work has no head at its end either.
    let refused = scene.try_as(
        0,
        scene.daemon(),
        end_session_at(supervising, EndReason::Supervised, commit(1)),
    );
    assert_eq!(refused, Err(Invalid::HeadNotOfItsWorkSession));
}

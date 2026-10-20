use crate::commits::Naming;
use crate::event::{Commented, EventBody};
use crate::id::{HashId, ItemId, SessionId};
use crate::identity::Actor;
use crate::item::ItemState;
use crate::session::{CommitId, EndReason, SessionRole, Verdict};
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

fn named(scene: &Scene) -> Vec<(CommitId, Naming)> {
    let mut named: Vec<_> = scene
        .state
        .named_commits()
        .into_iter()
        .map(|named| (named.commit, named.naming))
        .collect();
    named.sort_by_key(|(commit, _)| commit.to_string());
    named
}

#[test]
fn the_log_names_the_heads_the_submission_and_the_merge_with_the_device_that_wrote_them() {
    let mut scene = Scene::new();
    let item = scene.item();
    let session = scene.start(item, SessionRole::Work);
    scene.comment_naming(session, item, commit(1));
    assert_eq!(named(&scene), vec![(commit(1), Naming::Head { session })]);

    scene.apply(submit_session(session, commit(1)));
    scene.apply(moved(item, ItemState::Review));
    scene.apply(verdict(item, commit(1), Verdict::Approve));
    scene.apply(merged(item, commit(1), commit(2)));
    assert_eq!(
        named(&scene),
        vec![
            (commit(1), Naming::Head { session }),
            (commit(1), Naming::Submitted),
            (commit(1), Naming::Merged),
            (commit(2), Naming::Merged),
        ]
    );
    let device = scene.fixture.devices[0].key();
    assert!(
        scene
            .state
            .named_commits()
            .iter()
            .all(|named| named.author == device)
    );
}

#[test]
fn a_commit_no_ref_keeps_is_not_named() {
    let mut scene = Scene::new();
    let item = scene.item();
    let session = scene.start(item, SessionRole::Work);
    scene.comment_naming(session, item, commit(1));
    scene.apply(submit_session(session, commit(1)));
    scene.apply(moved(item, ItemState::Review));
    // A verdict on a commit that was rebased away since is not wanted from anyone.
    scene.apply(verdict(item, commit(9), Verdict::RequestChanges));
    assert!(
        !named(&scene).iter().any(|(named, _)| *named == commit(9)),
        "{:?}",
        named(&scene)
    );
}

#[test]
fn a_finished_item_names_no_commits() {
    let mut scene = Scene::new();
    let item = scene.item();
    let session = scene.start(item, SessionRole::Work);
    scene.comment_naming(session, item, commit(1));
    scene.apply(submit_session(session, commit(1)));
    scene.apply(moved(item, ItemState::Review));
    scene.apply(merged(item, commit(1), commit(2)));
    assert_eq!(named(&scene).len(), 4);
    scene.apply(moved(item, ItemState::Done));
    assert_eq!(named(&scene), vec![]);
}

#[test]
fn a_cancelled_item_names_no_commits() {
    let mut scene = Scene::new();
    let item = scene.item();
    let session = scene.start(item, SessionRole::Work);
    scene.comment_naming(session, item, commit(1));
    assert_eq!(named(&scene).len(), 1);
    scene.apply(moved(item, ItemState::Cancelled));
    assert_eq!(named(&scene), vec![]);
}

#[test]
fn without_a_claim_the_branch_is_at_the_head_named_last_not_that_of_the_session_started_last() {
    let mut scene = Scene::new();
    let item = scene.item();
    let older = scene.start(item, SessionRole::Work);
    let newer = scene.start(item, SessionRole::Work);
    scene.comment_naming(newer, item, commit(1));
    scene.comment_naming(older, item, commit(2));
    scene.apply(end_session(older, EndReason::Released));
    scene.apply(end_session(newer, EndReason::Released));

    assert_eq!(scene.state.work_head(item), Some(commit(2)));
    let heads = &scene.state.item_heads()[0];
    assert_eq!(heads.holder.map(|head| head.session), Some(older));
    assert_eq!(
        heads
            .others
            .iter()
            .map(|head| head.session)
            .collect::<Vec<_>>(),
        vec![newer]
    );
    assert_eq!(heads.sessions.len(), 2);
}

#[test]
fn the_branch_is_at_the_head_of_the_session_holding_the_claim_and_the_others_are_kept_apart() {
    let mut scene = Scene::new();
    let item = scene.item();
    let first = scene.start(item, SessionRole::Work);
    scene.comment_naming(first, item, commit(1));
    // A second work session that started later has lost the claim.
    let second = scene.start(item, SessionRole::Work);
    scene.comment_naming(second, item, commit(2));

    let heads = scene.state.item_heads();
    let [heads] = heads.as_slice() else {
        panic!("one item: {heads:?}");
    };
    let holder = heads.holder.expect("a holder");
    assert_eq!((holder.session, holder.commit), (first, commit(1)));
    let others: Vec<_> = heads.others.iter().map(|head| head.session).collect();
    assert_eq!(others, vec![second]);
    assert!(!heads.finished);

    // Once the holder is gone the latest named head is the branch, and nothing is apart.
    scene.apply(end_session(first, EndReason::Released));
    let heads = &scene.state.item_heads()[0];
    assert_eq!(heads.holder.map(|head| head.commit), Some(commit(2)));
    assert_eq!(
        heads
            .others
            .iter()
            .map(|head| head.commit)
            .collect::<Vec<_>>(),
        vec![commit(1)]
    );
}

#[test]
fn a_finished_item_says_so() {
    let mut scene = Scene::new();
    let item = scene.item();
    scene.apply(moved(item, ItemState::Cancelled));
    assert!(scene.state.item_heads()[0].finished);
}

/// Sources that keep the work of one session apart.
struct Apart(SessionId);

impl crate::brief::Sources for Apart {
    fn text(&self, _: crate::id::CommentId) -> Option<String> {
        None
    }

    fn branch(&self, _: ItemId) -> String {
        "wf/item".into()
    }

    fn diff(&self, _: &str, _: CommitId) -> String {
        String::new()
    }

    fn diverged(&self, session: SessionId) -> Option<String> {
        (session == self.0).then(|| format!("refs/wf/diverged/{}", session.short()))
    }
}

#[test]
fn the_brief_of_a_work_session_names_the_work_set_apart() {
    let mut scene = Scene::new();
    let item = scene.item();
    let holder = scene.start(item, SessionRole::Work);
    scene.comment_naming(holder, item, commit(1));
    let lost = scene.start(item, SessionRole::Work);
    scene.comment_naming(lost, item, commit(2));
    scene.apply(end_session(lost, EndReason::Released));

    let brief = crate::brief::Brief::of(&scene.state, holder, &Apart(lost))
        .unwrap()
        .render();
    let reference = format!("refs/wf/diverged/{}", lost.short());
    assert!(brief.contains("## Work set apart"), "{brief}");
    assert!(brief.contains(&reference), "{brief}");
    assert!(brief.contains(&commit(2).to_string()), "{brief}");

    let none =
        crate::brief::Brief::of(&scene.state, holder, &Apart(SessionId::from_bytes([1; 32])))
            .unwrap()
            .render();
    assert!(!none.contains("Work set apart"), "{none}");
}

#[test]
fn the_head_of_the_branch_is_not_work_set_apart_even_if_a_ref_is_left_over() {
    let mut scene = Scene::new();
    let item = scene.item();
    let holder = scene.start(item, SessionRole::Work);
    scene.comment_naming(holder, item, commit(1));
    let brief = crate::brief::Brief::of(&scene.state, holder, &Apart(holder))
        .unwrap()
        .render();
    assert!(!brief.contains("Work set apart"), "{brief}");
}

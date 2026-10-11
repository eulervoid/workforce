use crate::event::EventBody;
use crate::id::{CommentId, ItemId, SessionId};
use crate::item::{Item, ItemView};
use crate::state::{State, Trust};
use crate::testing::*;
use crate::validate::PolicyViolation;

fn live(state: &State, item: ItemId) -> &Item {
    match state.item(item).unwrap() {
        ItemView::Live(item) => item,
        ItemView::Archived(_) => panic!("live"),
    }
}

fn comment_id(event: &crate::event::Event) -> CommentId {
    event.id().into()
}

fn open(state: &State, item: ItemId) -> Vec<CommentId> {
    live(state, item)
        .open_comments()
        .into_iter()
        .map(|(id, _)| id)
        .collect()
}

fn with_task() -> (Fixture, State, ItemId) {
    let fixture = Fixture::new(2);
    let mut state = fixture.state();
    let task = fixture.apply(&mut state, 0, 2_000, create("task", "Parse durations"));
    (fixture, state, task.id().into())
}

#[test]
fn blockers_and_change_requests_stay_open_until_resolved() {
    let (fixture, mut state, task) = with_task();
    let question = comment_id(&fixture.apply(&mut state, 0, 2_001, blocker(task, "which crate?")));
    let change = comment_id(&fixture.apply(&mut state, 0, 2_002, change_request(task, "empty")));
    fixture.apply(&mut state, 0, 2_003, comment(task, "a remark"));
    assert_eq!(open(&state, task), [question, change]);
    assert!(live(&state, task).has_open_blocker());

    let answer = comment_id(&fixture.apply(
        &mut state,
        0,
        2_004,
        resolving(task, "humantime", &[question]),
    ));
    assert_eq!(open(&state, task), [change]);
    assert!(!live(&state, task).has_open_blocker());
    let note = &live(&state, task).notes()[&question.into()];
    assert_eq!(note.resolved().map(|resolved| resolved.by), Some(answer));

    fixture.apply(&mut state, 0, 2_005, resolving(task, "", &[change]));
    assert!(open(&state, task).is_empty());
}

#[test]
fn concurrent_resolutions_agree_on_the_earliest() {
    let (fixture, mut state, task) = with_task();
    let question = comment_id(&fixture.apply(&mut state, 0, 2_001, blocker(task, "which crate?")));
    let first = fixture.author(&state, 0, 3_000, resolving(task, "humantime", &[question]));
    let second = fixture.author(&state, 1, 3_001, resolving(task, "jiff", &[question]));

    let mut one = state.clone();
    let mut other = state;
    for event in [&first, &second] {
        one.receive(event.clone(), Trust::Untrusted).unwrap();
    }
    for event in [&second, &first] {
        other.receive(event.clone(), Trust::Untrusted).unwrap();
    }
    for state in [&one, &other] {
        let note = &live(state, task).notes()[&question.into()];
        assert_eq!(
            note.resolved().map(|resolved| resolved.by),
            Some(comment_id(&first))
        );
    }
}

#[test]
fn a_comment_resolves_only_open_blockers_and_change_requests_of_its_item() {
    let (fixture, mut state, task) = with_task();
    let other = fixture.apply(&mut state, 0, 2_001, create("task", "other"));
    let elsewhere =
        comment_id(&fixture.apply(&mut state, 0, 2_002, blocker(other.id().into(), "not here")));
    let remark = comment_id(&fixture.apply(&mut state, 0, 2_003, comment(task, "a remark")));
    let question = comment_id(&fixture.apply(&mut state, 0, 2_004, blocker(task, "which?")));
    fixture.apply(&mut state, 0, 2_005, resolving(task, "this", &[question]));

    assert_eq!(
        state.check_policy(&comment(task, "  ")),
        Err(PolicyViolation::EmptyComment)
    );
    assert_eq!(
        state.check_policy(&resolving(task, "", &[remark])),
        Err(PolicyViolation::NotResolvable(remark))
    );
    assert_eq!(
        state.check_policy(&resolving(task, "", &[elsewhere])),
        Err(PolicyViolation::NotResolvable(elsewhere))
    );
    assert_eq!(
        state.check_policy(&resolving(task, "", &[question])),
        Err(PolicyViolation::AlreadyResolved(question))
    );
}

#[test]
fn a_session_cannot_resolve_its_own_blocker() {
    let (fixture, mut state, task) = with_task();
    fixture.apply(&mut state, 0, 2_001, define_agent("coder"));
    let session: SessionId = fixture
        .apply(&mut state, 0, 3_000, start_session("coder", task))
        .id()
        .into();
    let actor = fixture.agent_actor("coder", session);
    let asked = fixture.author_as(&state, 0, actor.clone(), 3_001, blocker(task, "which?"));
    state.receive(asked.clone(), Trust::Untrusted).unwrap();
    let theirs = comment_id(&fixture.apply(&mut state, 0, 3_002, change_request(task, "tests")));

    let check = |body: &EventBody| state.check_actor(fixture.devices[0].key(), &actor, body);
    assert_eq!(
        check(&resolving(task, "never mind", &[comment_id(&asked)])),
        Err(PolicyViolation::SessionResolvesItsOwnBlocker)
    );
    assert_eq!(check(&resolving(task, "added", &[theirs])), Ok(()));
    assert_eq!(
        state.check_actor(
            fixture.devices[0].key(),
            &fixture.founder_actor(),
            &resolving(task, "use jiff", &[comment_id(&asked)])
        ),
        Ok(())
    );
}

#[test]
fn a_move_names_a_comment_of_its_item_as_its_reason() {
    let (fixture, mut state, task) = with_task();
    let other = fixture.apply(&mut state, 0, 2_001, create("task", "other"));
    let elsewhere = comment_id(&fixture.apply(
        &mut state,
        0,
        2_002,
        comment(other.id().into(), "not about this one"),
    ));
    let why = comment_id(&fixture.apply(&mut state, 0, 2_003, comment(task, "not needed")));
    let move_because = |because| match moved(task, "dropped") {
        EventBody::ItemMoved(moved) => EventBody::ItemMoved(crate::event::ItemMoved {
            because: Some(because),
            ..moved
        }),
        _ => unreachable!(),
    };
    assert_eq!(state.check_policy(&move_because(why)), Ok(()));
    assert_eq!(
        state.check_policy(&move_because(elsewhere)),
        Err(PolicyViolation::UnknownComment(elsewhere))
    );
}

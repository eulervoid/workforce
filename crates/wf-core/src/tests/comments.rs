use crate::event::EventBody;
use crate::id::{CommentId, ItemId, SessionId};
use crate::item::{Item, ItemEntry, ItemState};
use crate::state::{State, Trust};
use crate::testing::*;
use crate::validate::PolicyViolation;
use std::collections::BTreeSet;

fn live(state: &State, item: ItemId) -> &Item {
    match state.item(item).unwrap() {
        ItemEntry::Live(item) => item,
        ItemEntry::Archived(_) => panic!("live"),
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
    let note = &live(&state, task).comments()[&question];
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
        let note = &live(state, task).comments()[&question];
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
    let move_because = |because| {
        let EventBody::ItemMoved(moved) = moved(task, ItemState::Cancelled) else {
            unreachable!("`moved` builds an ItemMoved")
        };
        EventBody::ItemMoved(crate::event::ItemMoved {
            because: Some(because),
            ..moved
        })
    };
    assert_eq!(state.check_policy(&move_because(why)), Ok(()));
    assert_eq!(
        state.check_policy(&move_because(elsewhere)),
        Err(PolicyViolation::UnknownComment(elsewhere))
    );
}

#[test]
fn an_open_blocker_holds_up_the_work_until_it_is_resolved() {
    let (fixture, mut state, task) = with_task();
    fixture.apply(&mut state, 0, 2_001, define_agent("coder"));
    let ready = |state: &State| state.ready_items().iter().any(|item| item.id() == task);
    assert!(ready(&state));

    let session: SessionId = fixture
        .apply(&mut state, 0, 3_000, start_session("coder", task))
        .id()
        .into();
    let actor = fixture.agent_actor("coder", session);
    let asked = fixture.author_as(&state, 0, actor, 3_001, blocker(task, "which crate?"));
    state.receive(asked.clone(), Trust::Untrusted).unwrap();
    assert_eq!(
        state.turn_outcome(session, 1, 3, None),
        crate::lifecycle::TurnOutcome::Park
    );

    fixture.apply(
        &mut state,
        0,
        3_002,
        end_session(session, crate::session::EndReason::TimedOut),
    );
    fixture.apply(&mut state, 0, 3_003, moved(task, ItemState::Open));
    assert!(
        !ready(&state),
        "an open blocker keeps the item from being ready"
    );

    fixture.apply(
        &mut state,
        0,
        3_004,
        resolving(task, "humantime", &[comment_id(&asked)]),
    );
    assert!(ready(&state));
}

mod supervision {
    use super::*;
    use crate::identity::AgentId;
    use crate::name::AgentName;
    use crate::supervision::WaitsOn;

    fn agent(fixture: &Fixture, name: &str) -> AgentId {
        AgentId {
            principal: fixture.founder.key(),
            name: AgentName::new(name).unwrap(),
        }
    }

    fn waits_on(state: &State, item: ItemId) -> Vec<WaitsOn> {
        state
            .open_blockers_of(live(state, item))
            .into_iter()
            .map(|blocker| blocker.waits_on)
            .collect()
    }

    #[test]
    fn a_blocker_waits_on_whoever_it_names_first_then_a_supervisor_then_its_writer() {
        let (fixture, mut state, task) = with_task();
        fixture.apply(&mut state, 0, 2_001, define_agent("coder"));
        fixture.apply(&mut state, 0, 2_002, blocker(task, "which crate?"));
        assert_eq!(
            waits_on(&state, task),
            [WaitsOn::Principal(fixture.founder.key())],
            "without a supervisor it goes back to the principal"
        );

        fixture.apply(&mut state, 0, 2_003, define_supervisor("boss"));
        fixture.apply(&mut state, 0, 2_004, blocker(task, "@ada is it wanted?"));
        fixture.apply(&mut state, 0, 2_005, blocker(task, "@coder why this way?"));
        assert_eq!(
            waits_on(&state, task),
            [
                WaitsOn::Supervisor,
                WaitsOn::Principal(fixture.founder.key()),
                WaitsOn::Agent(agent(&fixture, "coder")),
            ]
        );
    }

    #[test]
    fn a_blocker_mentioning_two_waits_on_the_one_written_first() {
        let (fixture, mut state, task) = with_task();
        fixture.apply(&mut state, 0, 2_001, define_agent("coder"));
        fixture.apply(&mut state, 0, 2_002, blocker(task, "@coder, or else @ada?"));
        assert_eq!(
            waits_on(&state, task),
            [WaitsOn::Agent(agent(&fixture, "coder"))],
        );
    }

    #[test]
    fn a_supervisor_takes_blockers_on_items_it_did_not_work_on() {
        let (fixture, mut state, task) = with_task();
        fixture.apply(&mut state, 0, 2_001, define_agent("coder"));
        fixture.apply(&mut state, 0, 2_002, define_supervisor("boss"));
        fixture.apply(&mut state, 0, 2_003, define_supervisor("helper"));
        fixture.apply(&mut state, 0, 2_004, start_session("helper", task));
        fixture.apply(&mut state, 0, 2_005, blocker(task, "which crate?"));

        assert_eq!(
            state.supervisable(&agent(&fixture, "boss"), &BTreeSet::new()),
            [task]
        );
        assert!(
            state
                .supervisable(&agent(&fixture, "helper"), &BTreeSet::new())
                .is_empty(),
            "it worked on the item"
        );
        assert!(
            state
                .supervisable(&agent(&fixture, "coder"), &BTreeSet::new())
                .is_empty(),
            "not a supervisor"
        );

        fixture.apply(
            &mut state,
            0,
            2_006,
            EventBody::SessionStarted(crate::event::SessionStarted {
                agent: AgentName::new("boss").unwrap(),
                item: task,
                role: crate::session::SessionRole::Supervise,
            }),
        );
        assert!(
            state
                .supervisable(&agent(&fixture, "boss"), &BTreeSet::new())
                .is_empty(),
            "one supervise session at a time"
        );
    }
}

#[test]
fn a_comment_on_lines_needs_lines_from_1_the_last_after_the_first() {
    let (_, state, task) = with_task();
    let on_lines = |first, last| {
        let EventBody::Commented(written) = comment(task, "rename it back") else {
            unreachable!("a comment")
        };
        EventBody::Commented(crate::event::Commented {
            lines: Some(crate::event::LineAnchor {
                commit: crate::session::CommitId::Git(
                    "00112233445566778899aabbccddeeff00112233".parse().unwrap(),
                ),
                path: "src/lib.rs".into(),
                side: crate::event::DiffSide::New,
                first,
                last,
                quote: "+fn parse_lazily() {}".into(),
            }),
            ..written
        })
    };
    assert_eq!(state.check_policy(&on_lines(12, 12)), Ok(()));
    assert_eq!(
        state.check_policy(&on_lines(0, 3)),
        Err(PolicyViolation::NoLines { first: 0, last: 3 })
    );
    assert_eq!(
        state.check_policy(&on_lines(14, 12)),
        Err(PolicyViolation::NoLines {
            first: 14,
            last: 12
        })
    );
}

#[test]
fn a_comment_resolving_what_its_author_has_not_seen_is_refused() {
    let fixture = Fixture::new(2);
    let mut state = fixture.state();
    let mut other = fixture.state();
    let created = fixture.author(&state, 0, 2_000, create("task", "Parse durations"));
    let task: ItemId = created.id().into();
    state.receive(created.clone(), Trust::Stored).unwrap();
    other.receive(created, Trust::Stored).unwrap();
    let question = comment_id(&fixture.apply(&mut state, 0, 2_001, blocker(task, "which crate?")));
    // Device 1 writes without having seen the blocker.
    let unseen = fixture.author(&other, 1, 2_002, resolving(task, "answered", &[question]));
    let error = state.receive(unseen, Trust::Stored).unwrap_err();
    assert!(matches!(
        error,
        crate::validate::Invalid::ResolvesOutsideCausalPast(id) if id == question
    ));
    assert_eq!(open(&state, task), [question]);

    // Once the author has seen it, the comment resolves it, also through later events.
    fixture.apply(&mut state, 1, 2_003, comment(task, "a remark"));
    fixture.apply(
        &mut state,
        1,
        2_004,
        resolving(task, "answered", &[question]),
    );
    assert!(open(&state, task).is_empty());
}

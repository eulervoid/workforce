use crate::event::Event;
use crate::id::{ItemId, SessionId};
use crate::item::ItemView;
use crate::name::ReviewName;
use crate::session::EndReason;
use crate::state::{State, Trust};
use crate::testing::*;
use crate::validate::Invalid;

fn item_id(event: &Event) -> ItemId {
    event.id().into()
}

fn session_id(event: &Event) -> SessionId {
    event.id().into()
}

/// A state with an agent `coder` and one task.
fn with_task() -> (Fixture, State, ItemId) {
    let fixture = Fixture::new(2);
    let mut state = fixture.state();
    fixture.apply(&mut state, 0, 2_000, define_agent("coder"));
    let task = item_id(&fixture.apply(&mut state, 0, 2_001, create("task", "work")));
    (fixture, state, task)
}

#[test]
fn a_session_is_live_until_it_ends() {
    let (fixture, mut state, task) = with_task();
    let started = fixture.apply(&mut state, 0, 3_000, start_session("coder", task));
    let session = session_id(&started);
    {
        let folded = state.session(session).unwrap();
        assert!(folded.is_live());
        assert_eq!(folded.item(), task);
        assert_eq!(folded.agent().name.as_str(), "coder");
        assert_eq!(folded.device(), fixture.devices[0].key());
    }
    assert_eq!(state.live_sessions().count(), 1);

    fixture.apply(
        &mut state,
        0,
        3_001,
        end_session(session, EndReason::Released),
    );
    let ended = state.session(session).unwrap().ended().unwrap();
    assert_eq!(ended.reason, EndReason::Released);
    assert_eq!(state.live_sessions().count(), 0);
}

#[test]
fn the_earliest_end_counts() {
    let (fixture, mut state, task) = with_task();
    let session = session_id(&fixture.apply(&mut state, 0, 3_000, start_session("coder", task)));
    let cancelled = fixture.author(&state, 1, 3_001, end_session(session, EndReason::Cancelled));
    let failed = fixture.author(&state, 0, 3_002, end_session(session, EndReason::Failed));
    let mut forward = state.clone();
    let mut backward = state;
    for event in [&cancelled, &failed] {
        forward.receive((*event).clone(), Trust::Untrusted).unwrap();
    }
    for event in [&failed, &cancelled] {
        backward
            .receive((*event).clone(), Trust::Untrusted)
            .unwrap();
    }
    assert_eq!(forward, backward);
    let reason = forward.session(session).unwrap().ended().unwrap().reason;
    let earlier = if cancelled.content.hlc < failed.content.hlc {
        EndReason::Cancelled
    } else {
        EndReason::Failed
    };
    assert_eq!(reason, earlier);
}

#[test]
fn the_claim_belongs_to_the_earliest_live_work_session() {
    let (fixture, mut state, task) = with_task();
    let first = fixture.author(&state, 0, 3_000, start_session("coder", task));
    let second = fixture.author(&state, 1, 3_001, start_session("coder", task));
    let mut forward = state.clone();
    for event in [&first, &second] {
        forward.receive((*event).clone(), Trust::Untrusted).unwrap();
    }
    for event in [&second, &first] {
        state.receive((*event).clone(), Trust::Untrusted).unwrap();
    }
    assert_eq!(forward, state);

    let earlier = if first.content.hlc < second.content.hlc {
        &first
    } else {
        &second
    };
    let later = if std::ptr::eq(earlier, &first) {
        &second
    } else {
        &first
    };
    assert_eq!(state.claim(task).unwrap().id(), session_id(earlier));

    fixture.apply(
        &mut state,
        0,
        4_000,
        end_session(session_id(earlier), EndReason::Cancelled),
    );
    assert_eq!(state.claim(task).unwrap().id(), session_id(later));
}

#[test]
fn sessions_need_a_defined_agent_and_a_known_item() {
    let (fixture, mut state, task) = with_task();
    let unknown_agent = fixture.author(&state, 0, 3_000, start_session("ghost", task));
    assert_eq!(
        state.receive(unknown_agent, Trust::Untrusted).unwrap_err(),
        Invalid::UnknownAgent
    );
    let missing = ItemId::from(crate::id::EventId::of(b"missing"));
    let unknown_item = fixture.author(&state, 0, 3_000, start_session("coder", missing));
    assert_eq!(
        state.receive(unknown_item, Trust::Untrusted).unwrap_err(),
        Invalid::UnknownItem(missing)
    );
}

#[test]
fn ending_an_unknown_session_is_refused() {
    let (fixture, mut state, _) = with_task();
    let missing = SessionId::from(crate::id::EventId::of(b"missing"));
    let event = fixture.author(&state, 0, 3_000, end_session(missing, EndReason::Failed));
    assert_eq!(
        state.receive(event, Trust::Untrusted).unwrap_err(),
        Invalid::UnknownSession(missing)
    );
}

#[test]
fn an_item_held_by_a_live_session_is_not_ready() {
    use crate::graph::Readiness;

    let (fixture, mut state, task) = with_task();
    let readiness = |state: &State| {
        let ItemView::Live(item) = state.item(task).unwrap() else {
            panic!("live")
        };
        state.readiness(&state.dependency_graph(), item)
    };
    assert_eq!(readiness(&state), Some(Readiness::Ready));
    assert_eq!(state.ready_items().len(), 1);

    let session = session_id(&fixture.apply(&mut state, 0, 3_000, start_session("coder", task)));
    assert_eq!(readiness(&state), Some(Readiness::Claimed));
    assert!(state.ready_items().is_empty());

    fixture.apply(
        &mut state,
        0,
        3_001,
        end_session(session, EndReason::Failed),
    );
    assert_eq!(readiness(&state), Some(Readiness::Ready));
    assert_eq!(state.ready_items().len(), 1);
}

mod actors {
    use super::*;
    use crate::event::EventBody;
    use crate::validate::PolicyViolation;

    /// A state where `coder` has a live session on device 0 working on the returned task.
    fn working() -> (Fixture, State, ItemId, SessionId) {
        let (fixture, mut state, task) = with_task();
        let session =
            session_id(&fixture.apply(&mut state, 0, 3_000, start_session("coder", task)));
        (fixture, state, task, session)
    }

    fn check(
        fixture: &Fixture,
        state: &State,
        device: usize,
        session: SessionId,
        body: &EventBody,
    ) -> Result<(), PolicyViolation> {
        state.check_actor(
            fixture.devices[device].key(),
            &fixture.agent_actor("coder", session),
            body,
        )
    }

    #[test]
    fn a_session_acts_while_it_is_live_on_its_own_device() {
        let (fixture, mut state, task, session) = working();
        let comment = comment(task, "progress");
        assert_eq!(check(&fixture, &state, 0, session, &comment), Ok(()));
        assert_eq!(
            check(&fixture, &state, 1, session, &comment),
            Err(PolicyViolation::SessionNotLive(session))
        );
        let missing = SessionId::from(crate::id::EventId::of(b"missing"));
        assert_eq!(
            check(&fixture, &state, 0, missing, &comment),
            Err(PolicyViolation::UnknownSession(missing))
        );

        fixture.apply(
            &mut state,
            0,
            3_001,
            end_session(session, EndReason::Released),
        );
        assert_eq!(
            check(&fixture, &state, 0, session, &comment),
            Err(PolicyViolation::SessionNotLive(session))
        );
    }

    #[test]
    fn agents_cannot_write_definitions_or_authority() {
        let (fixture, state, _, session) = working();
        assert_eq!(
            check(&fixture, &state, 0, session, &define_agent("rogue")),
            Err(PolicyViolation::AgentCannotWrite("AgentDefined"))
        );
        let policy = fixture.policy_set(&fixture.founder, "");
        assert_eq!(
            check(&fixture, &state, 0, session, &policy),
            Err(PolicyViolation::AgentCannotWrite("PolicySet"))
        );
    }

    #[test]
    fn a_session_cannot_finish_its_own_item_but_can_finish_others() {
        let (fixture, mut state, task, session) = working();
        let other = item_id(&fixture.apply(&mut state, 0, 3_002, create("task", "other")));
        assert_eq!(
            check(&fixture, &state, 0, session, &moved(task, "done")),
            Err(PolicyViolation::SessionFinishesItsItem)
        );
        assert_eq!(
            check(&fixture, &state, 0, session, &moved(task, "in_review")),
            Ok(())
        );
        assert_eq!(
            check(&fixture, &state, 0, session, &moved(other, "done")),
            Ok(())
        );
    }

    #[test]
    fn a_session_ends_only_itself() {
        let (fixture, mut state, task, session) = working();
        let other = session_id(&fixture.apply(&mut state, 0, 3_002, start_session("coder", task)));
        assert_eq!(
            check(
                &fixture,
                &state,
                0,
                session,
                &end_session(other, EndReason::Cancelled)
            ),
            Err(PolicyViolation::SessionEndsAnother)
        );
        assert_eq!(
            check(
                &fixture,
                &state,
                0,
                session,
                &end_session(session, EndReason::Released)
            ),
            Ok(())
        );
    }

    #[test]
    fn events_of_a_session_are_admitted_only_from_its_device() {
        let (fixture, mut state, task, session) = working();
        let actor = fixture.agent_actor("coder", session);
        let good = fixture.author_as(&state, 0, actor.clone(), 3_003, comment(task, "ok"));
        let wrong_device = fixture.author_as(&state, 1, actor, 3_003, comment(task, "no"));
        assert_eq!(
            state.receive(wrong_device, Trust::Untrusted).unwrap_err(),
            Invalid::SessionMismatch(session)
        );
        state.receive(good, Trust::Untrusted).unwrap();

        let missing = SessionId::from(crate::id::EventId::of(b"missing"));
        let unknown = fixture.author_as(
            &state,
            0,
            fixture.agent_actor("coder", missing),
            3_004,
            comment(task, "ghost"),
        );
        assert_eq!(
            state.receive(unknown, Trust::Untrusted).unwrap_err(),
            Invalid::UnknownSession(missing)
        );
    }
}

mod reports {
    use super::*;
    use crate::body::StepId;
    use crate::event::{EventBody, SessionStarted};
    use crate::name::AgentName;
    use crate::session::SessionRole;
    use crate::validate::PolicyViolation;

    const STEP: &str = "pick storage";

    /// A coder working a task with one step, and the session it holds.
    fn working() -> (Fixture, State, ItemId, SessionId) {
        let fixture = Fixture::new(2);
        let mut state = fixture.state();
        fixture.apply(&mut state, 0, 2_000, define_agent("coder"));
        let task = item_id(&fixture.apply(
            &mut state,
            0,
            2_001,
            create_with_body("task", "work", &format!("- [ ] {STEP}\n")),
        ));
        let session =
            session_id(&fixture.apply(&mut state, 0, 3_000, start_session("coder", task)));
        (fixture, state, task, session)
    }

    fn write(
        fixture: &Fixture,
        state: &mut State,
        session: SessionId,
        now_ms: u64,
        body: EventBody,
    ) -> Event {
        let actor = fixture.agent_actor("coder", session);
        let event = fixture.author_as(state, 0, actor, now_ms, body);
        state.receive(event.clone(), Trust::Untrusted).unwrap();
        event
    }

    /// The steps the comments of sessions on the item are about.
    fn reports(state: &State, task: ItemId) -> Vec<(Option<SessionId>, Option<StepId>)> {
        let ItemView::Live(item) = state.item(task).unwrap() else {
            panic!("live")
        };
        item.comments()
            .values()
            .filter(|comment| comment.session().is_some())
            .map(|comment| (comment.session(), comment.step))
            .collect()
    }

    #[test]
    fn a_report_is_a_comment_of_a_session_that_may_name_a_step() {
        let (fixture, mut state, task, session) = working();
        let step = StepId::of(STEP);
        write(
            &fixture,
            &mut state,
            session,
            3_001,
            on_step(task, "chose redb", step),
        );
        write(
            &fixture,
            &mut state,
            session,
            3_002,
            comment(task, "halfway"),
        );
        fixture.apply(&mut state, 0, 3_003, comment(task, "a person's remark"));
        let reports = reports(&state, task);
        assert_eq!(reports.len(), 2);
        assert!(reports.contains(&(Some(session), Some(step))));
        assert!(reports.contains(&(Some(session), None)));
    }

    #[test]
    fn a_report_outlives_the_step_it_names() {
        let (fixture, mut state, task, session) = working();
        let step = StepId::of(STEP);
        write(
            &fixture,
            &mut state,
            session,
            3_001,
            on_step(task, "chose redb", step),
        );
        fixture.apply(
            &mut state,
            0,
            3_002,
            edit(
                task,
                crate::event::Change::Body("- [ ] pick a database\n".into()),
            ),
        );
        let ItemView::Live(item) = state.item(task).unwrap() else {
            panic!("live")
        };
        assert!(item.body().step(step).is_none());
        assert_eq!(reports(&state, task).len(), 1);
    }

    #[test]
    fn a_step_must_exist_when_the_comment_is_written() {
        let (_, state, task, _) = working();
        assert_eq!(
            state.check_policy(&on_step(task, "ok", StepId::of(STEP))),
            Ok(())
        );
        let missing = StepId::of("no such step");
        assert_eq!(
            state.check_policy(&on_step(task, "bad", missing)),
            Err(PolicyViolation::UnknownStep(missing))
        );
    }

    #[test]
    fn a_review_session_comments_without_steps() {
        let (fixture, mut state, task, _) = working();
        let commit = crate::session::CommitId::Git(
            "00112233445566778899aabbccddeeff00112233".parse().unwrap(),
        );
        let reviewer = session_id(&fixture.apply(
            &mut state,
            0,
            3_003,
            EventBody::SessionStarted(SessionStarted {
                agent: AgentName::new("coder").unwrap(),
                item: task,
                role: SessionRole::Review {
                    commit: Some(commit),
                },
            }),
        ));
        let actor = fixture.agent_actor("coder", reviewer);
        let device = fixture.devices[0].key();
        assert_eq!(
            state.check_actor(device, &actor, &comment(task, "looks fine")),
            Ok(())
        );
        assert_eq!(
            state.check_actor(device, &actor, &on_step(task, "ticking", StepId::of(STEP))),
            Err(PolicyViolation::StepInReviewComment)
        );
    }
}

mod review_levels {
    use super::*;
    use crate::validate::PolicyViolation;

    fn setup() -> (Fixture, State, ItemId) {
        let fixture = Fixture::new(2);
        let mut state = fixture.state();
        fixture.apply(&mut state, 0, 2_000, define_agent("coder"));
        fixture.apply(&mut state, 0, 2_001, define_supervisor("boss"));
        let task = item_id(&fixture.apply(
            &mut state,
            0,
            2_002,
            create_reviewed("task", "reviewed work", "standard"),
        ));
        (fixture, state, task)
    }

    fn check_as(
        fixture: &Fixture,
        state: &State,
        agent: &str,
        session: SessionId,
        body: &crate::event::EventBody,
    ) -> Result<(), PolicyViolation> {
        state.check_actor(
            fixture.devices[0].key(),
            &fixture.agent_actor(agent, session),
            body,
        )
    }

    #[test]
    fn items_name_levels_that_exist() {
        let (fixture, mut state, task) = setup();
        let ghost = ReviewName::new("ghost").unwrap();
        let mut created = create("task", "x");
        if let crate::event::EventBody::ItemCreated(created) = &mut created {
            created.review = Some(ghost.clone());
        }
        assert_eq!(
            state.check_policy(&created),
            Err(PolicyViolation::UnknownReviewLevel(ghost.clone()))
        );
        assert_eq!(
            state.check_policy(&edit(
                task,
                crate::event::Change::Review(Some(ghost.clone()))
            )),
            Err(PolicyViolation::UnknownReviewLevel(ghost))
        );
        assert_eq!(
            state.check_policy(&review_of(task, Some("critical"))),
            Ok(())
        );
        assert_eq!(state.check_policy(&review_of(task, None)), Ok(()));

        let distill = item_id(&fixture.apply(&mut state, 0, 2_003, create("distill", "sum up")));
        assert_eq!(
            state.check_policy(&review_of(distill, Some("light"))),
            Err(PolicyViolation::ReviewWithoutReviewState {
                type_name: crate::name::TypeName::new("distill").unwrap()
            })
        );
    }

    #[test]
    fn an_agent_that_worked_on_an_item_cannot_lower_its_review() {
        let (fixture, mut state, task) = setup();
        let session =
            session_id(&fixture.apply(&mut state, 0, 3_000, start_session("coder", task)));
        let lower = |level| review_of(task, level);
        assert_eq!(
            check_as(&fixture, &state, "coder", session, &lower(Some("light"))),
            Err(PolicyViolation::AgentLowersReview(task))
        );
        assert_eq!(
            check_as(&fixture, &state, "coder", session, &lower(None)),
            Err(PolicyViolation::AgentLowersReview(task))
        );
        assert_eq!(
            check_as(&fixture, &state, "coder", session, &lower(Some("standard"))),
            Ok(())
        );
        assert_eq!(
            check_as(&fixture, &state, "coder", session, &lower(Some("critical"))),
            Ok(())
        );
    }

    #[test]
    fn other_items_supervisors_and_humans_may_lower_it() {
        let (fixture, mut state, task) = setup();
        let other = item_id(&fixture.apply(
            &mut state,
            0,
            2_010,
            create_reviewed("task", "someone else's", "critical"),
        ));
        let session =
            session_id(&fixture.apply(&mut state, 0, 3_000, start_session("coder", task)));
        assert_eq!(
            check_as(
                &fixture,
                &state,
                "coder",
                session,
                &review_of(other, Some("light"))
            ),
            Ok(())
        );

        let boss = session_id(&fixture.apply(&mut state, 0, 3_001, start_session("boss", task)));
        assert_eq!(
            check_as(
                &fixture,
                &state,
                "boss",
                boss,
                &review_of(task, Some("light"))
            ),
            Ok(())
        );
        assert_eq!(
            state.check_actor(
                fixture.devices[0].key(),
                &fixture.founder_actor(),
                &review_of(task, None)
            ),
            Ok(())
        );
    }
}

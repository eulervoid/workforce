use crate::id::{HashId, ItemId, SessionId};
use crate::identity::Actor;
use crate::item::{ItemEntry, ItemState};
use crate::lifecycle::{
    AfterEnd, AfterFailedChecks, Closing, Finish, Interruption, MergeFailure, MergePlan,
    SessionEnd, TurnOutcome,
};
use crate::session::{CommitId, EndReason, MachineFault, Verdict};
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
            commit: commit(),
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
    let outcome = |scene: &Scene, used| scene.state.turn_outcome(session, used, 3, None);
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
            scene.state.turn_outcome(session, 0, 3, None),
            TurnOutcome::Close,
            "{moved_on}"
        );
    }

    scene.apply(moved(task, ItemState::Active));
    assert_eq!(
        scene.state.turn_outcome(session, 0, 3, None),
        TurnOutcome::Continue
    );
    scene.end(session, EndReason::Released);
    assert_eq!(
        scene.state.turn_outcome(session, 0, 3, None),
        TurnOutcome::Close
    );
}

#[test]
fn a_turn_parks_while_the_item_waits_on_a_dependency() {
    let mut scene = Scene::new();
    let task = scene.item(create("task", "work"));
    let session = scene.start(task);
    scene.apply(moved(task, ItemState::Active));
    let first = scene.item(create("task", "pick a database"));
    scene.apply(depends_on(task, first));
    assert_eq!(
        scene.state.turn_outcome(session, 0, 3, None),
        TurnOutcome::Park
    );

    scene.apply(moved(first, ItemState::Done));
    assert_eq!(
        scene.state.turn_outcome(session, 0, 3, None),
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

const MACHINE_FAULTS: [MachineFault; 3] = [
    MachineFault::LoginLapsed,
    MachineFault::NoSpace,
    MachineFault::HarnessUnreachable,
];

#[test]
fn a_fault_of_the_machine_ends_the_session_with_it_and_without_a_comment() {
    let mut scene = Scene::new();
    let task = scene.item(create("task", "work"));
    let by_daemon = Actor::Daemon(scene.fixture.founder.key());
    for fault in MACHINE_FAULTS {
        let session = scene.start(task);
        scene.apply(moved(task, ItemState::Active));
        let closing = scene
            .state
            .closing(
                session,
                &SessionEnd::Interruption(Interruption::MachineFault(fault)),
                None,
            )
            .unwrap();
        assert_eq!(
            closing,
            Closing {
                reason: EndReason::MachineFault(fault),
                actor: by_daemon.clone(),
                note: None,
            },
            "the inbox says what is wrong, not a comment per session"
        );
        assert_eq!(
            scene.state.after_end(session, closing.reason),
            Some(AfterEnd::MoveTo(ItemState::Open)),
            "{fault:?}"
        );
        scene.end(session, closing.reason);
        scene.apply(moved(task, ItemState::Open));
    }
}

#[test]
fn a_turn_on_a_machine_at_fault_ends_the_session_without_using_up_the_nudges() {
    let mut scene = Scene::new();
    let task = scene.item(create("task", "work"));
    let session = scene.start(task);
    scene.apply(moved(task, ItemState::Active));
    for fault in MACHINE_FAULTS {
        for used in [1, 4] {
            assert_eq!(
                scene.state.turn_outcome(session, used, 3, Some(fault)),
                TurnOutcome::MachineFault(fault)
            );
        }
    }
    scene.apply(moved(task, ItemState::Review));
    assert_eq!(
        scene
            .state
            .turn_outcome(session, 0, 3, Some(MachineFault::NoSpace)),
        TurnOutcome::Close,
        "an item that moved on is closed as before"
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

#[test]
fn work_rebases_at_submit_until_it_is_approved() {
    let mut scene = Scene::new();
    let task = scene.item(create_reviewed("task", "careful", "standard"));
    let first = scene.start(task);
    assert!(scene.state.rebases_at_submit(first));
    let review = scene
        .apply(start_review("coder", task, commit()))
        .id()
        .into();
    assert!(
        !scene.state.rebases_at_submit(review),
        "only work is rebased"
    );
    scene.apply(moved(task, ItemState::Review));
    assert!(
        !scene.state.rebases_at_submit(first),
        "work that was moved on to review is not rewritten"
    );
    let submitted = EndReason::Submitted {
        commit: Some(commit()),
    };
    scene.end(first, submitted);

    // Sent back by its review.
    scene.apply(verdict(task, commit(), Verdict::RequestChanges));
    scene.apply(moved(task, ItemState::Open));
    let again = scene.start(task);
    assert!(scene.state.rebases_at_submit(again));
    scene.apply(moved(task, ItemState::Review));
    scene.end(again, submitted);

    // Approved, it waits in review for its merge and is never rewritten.
    scene.apply(verdict(task, commit(), Verdict::Approve));
    let waiting = scene.start(task);
    assert!(!scene.state.rebases_at_submit(waiting));
    scene.end(waiting, EndReason::Released);

    // Sent back by the merge's conflict, it is rebased at the next submit.
    scene.apply(moved(task, ItemState::Open));
    let after_conflict = scene.start(task);
    assert!(scene.state.rebases_at_submit(after_conflict));
}

impl Scene {
    /// Applies `body` as the daemon, which does not count as the principal acting on an item.
    fn apply_by_daemon(&mut self, body: crate::event::EventBody) -> crate::event::Event {
        self.clock += 1;
        let actor = Actor::Daemon(self.fixture.founder_actor().principal());
        let event = self
            .fixture
            .author_as(&self.state, 0, actor, self.clock, body);
        self.state
            .receive(event.clone(), crate::state::Trust::Untrusted)
            .unwrap();
        event
    }

    /// A submission of work whose merge result then fails `checks` checks.
    fn submit_and_fail_checks(&mut self, item: ItemId, checks: usize) {
        let session = self
            .apply_by_daemon(start_session("coder", item))
            .id()
            .into();
        self.apply_by_daemon(end_session(
            session,
            EndReason::Submitted {
                commit: Some(commit()),
            },
        ));
        for check in 0..checks {
            let failed = self.apply_by_daemon(check_failure(item, &format!("check {check}")));
            // The agent fixes each before it submits again.
            let fixed = resolving(item, "fixed", &[failed.id().into()]);
            self.apply_by_daemon(fixed);
        }
    }
}

#[test]
fn failed_checks_send_work_back_twice_before_the_principal_decides() {
    let mut scene = Scene::new();
    let task = scene.item(create("task", "work"));
    let back_to_work = AfterFailedChecks::BackToWork {
        to: ItemState::Open,
    };
    assert_eq!(scene.state.failed_check_runs(task), 0);
    assert_eq!(scene.state.after_failed_checks(task), back_to_work);

    // One run with two failed checks counts once.
    scene.submit_and_fail_checks(task, 2);
    assert_eq!(scene.state.failed_check_runs(task), 1);
    assert_eq!(scene.state.after_failed_checks(task), back_to_work);

    scene.submit_and_fail_checks(task, 1);
    assert_eq!(scene.state.failed_check_runs(task), 2);
    assert_eq!(
        scene.state.after_failed_checks(task),
        AfterFailedChecks::WaitsForPrincipal { runs: 2 }
    );

    // Once the principal acts on the item, the count starts again.
    scene.apply(comment(task, "go on"));
    assert_eq!(scene.state.failed_check_runs(task), 0);
    assert_eq!(scene.state.after_failed_checks(task), back_to_work);
    scene.submit_and_fail_checks(task, 1);
    assert_eq!(scene.state.failed_check_runs(task), 1);
}

#[test]
fn change_requests_of_others_are_not_failed_checks() {
    let mut scene = Scene::new();
    let task = scene.item(create("task", "work"));
    scene.submit_and_fail_checks(task, 0);
    let principal = scene.apply(change_request(task, "rename it"));
    scene.apply(resolving(task, "done", &[principal.id().into()]));
    scene.apply_by_daemon(change_request(task, "from the daemon, but not a check"));
    assert_eq!(scene.state.failed_check_runs(task), 0);
}

#[test]
fn check_failures_before_any_submission_are_not_counted() {
    let mut scene = Scene::new();
    let task = scene.item(create("task", "work"));
    scene.apply_by_daemon(check_failure(task, "early"));
    assert_eq!(scene.state.failed_check_runs(task), 0);
    scene.submit_and_fail_checks(task, 1);
    assert_eq!(scene.state.failed_check_runs(task), 1);
}

#[test]
fn work_with_an_open_blocker_waits_to_merge_and_the_inbox_shows_the_blocker() {
    let mut scene = Scene::new();
    let task = scene.item(create("task", "work"));
    let session = scene.start(task);
    scene.apply(moved(task, ItemState::Review));
    scene.end(
        session,
        EndReason::Submitted {
            commit: Some(commit()),
        },
    );
    assert_eq!(scene.state.pending_merges().len(), 1);

    let blocker = scene.apply_by_daemon(blocker(task, "main itself fails `clippy`"));
    assert!(scene.state.pending_merges().is_empty());
    let waiting: Vec<_> = scene
        .state
        .inbox()
        .into_iter()
        .map(|entry| entry.waiting)
        .collect();
    assert_eq!(
        waiting,
        [crate::inbox::Waiting::Blocker {
            comment: blocker.id().into()
        }]
    );

    scene.apply(resolving(task, "fixed on main", &[blocker.id().into()]));
    assert_eq!(scene.state.pending_merges().len(), 1);
}

#[test]
fn a_merge_is_planned_from_what_the_item_already_went_through() {
    let mut scene = Scene::new();
    let task = scene.item(create("task", "work"));
    let plan = |scene: &Scene| {
        let state = &scene.state;
        let policy = &state.policy().unwrap().policy;
        let Some(ItemEntry::Live(item)) = state.item(task) else {
            panic!("the item is live");
        };
        MergePlan::of(item, policy, commit())
    };
    assert_eq!(plan(&scene), MergePlan::Merge);

    scene.apply(merged(task, commit(), commit()));
    assert_eq!(plan(&scene), MergePlan::MoveToDone);

    scene.apply(moved(task, ItemState::Done));
    assert_eq!(plan(&scene), MergePlan::Nothing);
}

#[test]
fn only_a_live_work_session_with_a_waiting_item_waits() {
    let mut scene = Scene::new();
    let task = scene.item(create("task", "work"));
    let session = scene.start(task);
    scene.apply(moved(task, ItemState::Active));
    assert!(
        !scene
            .state
            .session_waits(session, &scene.state.dependency_graph())
    );

    let first = scene.item(create("task", "pick a database"));
    scene.apply(depends_on(task, first));
    assert!(
        scene
            .state
            .session_waits(session, &scene.state.dependency_graph())
    );
    assert_eq!(
        scene.state.turn_outcome(session, 0, 3, None),
        TurnOutcome::Park
    );

    scene.end(session, EndReason::Released);
    assert!(
        !scene
            .state
            .session_waits(session, &scene.state.dependency_graph())
    );
}

#[test]
fn a_refusal_names_the_item_short_and_the_session_that_holds_it() {
    let mut scene = Scene::new();
    let item = scene.item(create("task", "Busy"));
    assert_eq!(scene.state.ensure_unclaimed(item), Ok(()));
    let session = scene.start(item);
    let holder = scene
        .state
        .actor_label(&scene.fixture.agent_actor("coder", session));
    assert!(
        holder.ends_with(&format!("/coder/{}", session.short())),
        "{holder}"
    );
    assert_eq!(
        scene.state.ensure_unclaimed(item).unwrap_err().to_string(),
        format!(
            "#{} already has a live session, {holder}; stop it first",
            item.short()
        )
    );
}

/// The texts of the comments of a scene, by what they were written as.
struct Texts(std::collections::BTreeMap<crate::id::CommentId, String>);

impl crate::brief::Sources for Texts {
    fn text(&self, comment: crate::id::CommentId) -> Option<String> {
        self.0.get(&comment).cloned()
    }

    fn branch(&self, _item: ItemId) -> String {
        String::new()
    }

    fn diff(&self, _target: &str, _commit: CommitId) -> String {
        String::new()
    }
}

impl Scene {
    /// Writes `body` as the principal and remembers its `text`.
    fn say(&mut self, texts: &mut Texts, body: crate::event::EventBody, text: &str) {
        let written = self.apply(body);
        texts.0.insert(written.id().into(), text.to_string());
    }

    /// Writes `body` as the agent of `session` and remembers its `text`.
    fn say_as_agent(
        &mut self,
        texts: &mut Texts,
        session: SessionId,
        body: crate::event::EventBody,
        text: &str,
    ) {
        use crate::state::Trust;
        self.clock += 1;
        let actor = self.fixture.agent_actor("coder", session);
        let written = self
            .fixture
            .author_as(&self.state, 0, actor, self.clock, body);
        self.state
            .receive(written.clone(), Trust::Untrusted)
            .unwrap();
        texts.0.insert(written.id().into(), text.to_string());
    }
}

#[test]
fn a_session_that_asked_a_blocker_since_it_was_prompted_resumes_as_waiting() {
    use crate::lifecycle::Resume;
    let mut scene = Scene::new();
    let mut texts = Texts(Default::default());
    let task = scene.item(create("task", "work"));
    let session = scene.start(task);
    scene.apply(moved(task, ItemState::Active));
    let prompted = scene.clock;
    assert_eq!(
        scene.state.resume_after_restart(session, prompted),
        Resume::AfterRestart
    );

    scene.say_as_agent(
        &mut texts,
        session,
        blocker(task, "which crate?"),
        "which crate?",
    );
    assert_eq!(
        scene.state.resume_after_restart(session, prompted),
        Resume::AfterWait
    );
    assert_eq!(
        scene.state.resume_after_restart(session, scene.clock + 1),
        Resume::AfterRestart,
        "a blocker from an earlier turn was answered already"
    );
}

#[test]
fn a_restarted_session_is_told_what_others_wrote_since_it_was_prompted() {
    use crate::brief::restarted_text;
    let mut scene = Scene::new();
    let mut texts = Texts(Default::default());
    let task = scene.item(create("task", "work"));
    let session = scene.start(task);
    scene.say(&mut texts, comment(task, "earlier note"), "earlier note");
    let prompted = scene.clock;
    let quiet = restarted_text(&scene.state, session, prompted, &texts);
    assert!(!quiet.contains("Written on the item since"), "{quiet}");
    assert!(quiet.ends_with("Continue."), "{quiet}");

    scene.say_as_agent(&mut texts, session, comment(task, "my own"), "my own");
    scene.say(&mut texts, comment(task, "use serde"), " use serde ");
    let told = restarted_text(&scene.state, session, prompted, &texts);
    assert!(told.contains("Written on the item since:\n- "), "{told}");
    assert!(told.contains(": use serde"), "{told}");
    assert!(
        !told.contains("earlier note") && !told.contains("my own"),
        "{told}"
    );
}

#[test]
fn a_resumed_session_is_told_the_answers_and_what_its_dependencies_left() {
    use crate::brief::resumed_text;
    let mut scene = Scene::new();
    let mut texts = Texts(Default::default());
    let task = scene.item(create("task", "work"));
    let first = scene.item(create("task", "pick a database"));
    scene.apply(depends_on(task, first));
    let session = scene.start(task);
    scene.say(&mut texts, comment(task, "old"), "old");
    scene.say_as_agent(
        &mut texts,
        session,
        blocker(task, "which one?"),
        "which one?",
    );
    scene.say(&mut texts, comment(task, "sqlite"), "sqlite");
    scene.say(
        &mut texts,
        comment(first, "chose postgres"),
        "chose postgres",
    );

    let told = resumed_text(&scene.state, session, &texts);
    assert!(
        told.starts_with("What you were waiting for is resolved."),
        "{told}"
    );
    assert!(told.contains(": sqlite"), "{told}");
    assert!(
        told.contains(&format!(
            "#{} pick a database: chose postgres",
            first.short()
        )),
        "{told}"
    );
    assert!(
        !told.contains("old") && !told.contains("which one?"),
        "{told}"
    );
    assert!(told.ends_with("\nContinue."), "{told}");
}

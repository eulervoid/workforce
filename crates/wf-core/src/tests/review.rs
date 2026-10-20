use crate::event::EventBody;
use crate::id::{ItemId, SessionId};
use crate::identity::{Actor, AgentId};
use crate::inbox::PrincipalReviewReason;
use crate::item::ItemState;
use crate::lifecycle::Rounds;
use crate::lifecycle::TurnOutcome;
use crate::name::AgentName;
use crate::policy::ReviewStep;
use crate::review::ReviewStatus;
use crate::session::{CommitId, EndReason, Verdict};
use crate::state::State;
use crate::testing::*;
use crate::validate::PolicyViolation;
use std::collections::BTreeSet;

fn commit(byte: u8) -> CommitId {
    let text: String = std::iter::repeat_n(format!("{byte:02x}"), 20).collect();
    CommitId::Git(text.parse().unwrap())
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
        fixture.apply(&mut state, 0, 2_000, define_agent_with("coder", &[]));
        fixture.apply(
            &mut state,
            0,
            2_001,
            define_agent_with("reviewer", &["review", "strong-model"]),
        );
        fixture.apply(&mut state, 0, 2_002, define_agent_with("weak", &["review"]));
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

    /// A task with a review level, submitted by `coder` at `commit(1)` and now in review.
    fn submitted(&mut self, level: &str) -> ItemId {
        let item = self
            .apply(create_reviewed("task", "work", level))
            .id()
            .into();
        self.submit(item, commit(1));
        item
    }

    /// As the daemon writes a submission: the item moves to review, then the session ends.
    fn submit(&mut self, item: ItemId, commit: CommitId) {
        let session: SessionId = self.apply(start_session("coder", item)).id().into();
        self.apply(moved(item, ItemState::Review));
        self.apply(submit_session(session, commit));
    }

    /// A verdict given by `reviewer` from a review session of `commit(1)`, which it then ends.
    fn agent_verdict(&mut self, item: ItemId, verdict_given: Verdict) {
        self.step_verdict("reviewer", item, None, verdict_given);
    }

    /// A verdict given by `agent` from a review session of `commit(1)` for the step called
    /// `step`, which it then ends.
    fn step_verdict(
        &mut self,
        agent: &str,
        item: ItemId,
        step: Option<&str>,
        verdict_given: Verdict,
    ) {
        self.review_as(agent, item, commit(1), step, verdict_given);
    }

    /// A verdict given by `agent` from a review session of `reviewed` for the step called `step`,
    /// which it then ends. The verdict records the round limit as `wf review` does.
    fn review_as(
        &mut self,
        agent: &str,
        item: ItemId,
        reviewed: CommitId,
        step: Option<&str>,
        verdict_given: Verdict,
    ) {
        let session: SessionId = self
            .apply(start_review_step(agent, item, reviewed, step))
            .id()
            .into();
        let actor = self.fixture.agent_actor(agent, session);
        let body = EventBody::Reviewed(crate::event::Reviewed {
            item,
            commit: Some(reviewed),
            verdict: verdict_given,
            because: None,
            round_limit_reached: self.state.round_limit_reached(item, verdict_given, &actor),
        });
        self.apply_as(actor, body);
        self.apply(end_session(session, EndReason::Reviewed));
    }

    fn apply_as(&mut self, actor: crate::identity::Actor, body: EventBody) {
        self.clock += 1;
        let event = self
            .fixture
            .author_as(&self.state, 0, actor, self.clock, body);
        self.state
            .receive(event, crate::state::Trust::Untrusted)
            .expect("the event is valid");
    }

    /// Replaces the policy with the default one and `extra` after it.
    fn set_policy(&mut self, extra: &str) {
        let source = format!("{}\n{extra}", crate::policy::DEFAULT_POLICY_TOML);
        let body = self.fixture.policy_set(
            &self.state,
            0,
            self.clock + 1,
            &self.fixture.founder,
            &source,
        );
        self.apply(body);
    }

    /// Three more agents that can review, and a level `wide` of three steps at once, then the
    /// principal.
    fn with_wide_level() -> Self {
        let mut scene = Scene::new();
        for name in ["second", "third"] {
            scene.apply(define_agent_with(name, &["review", "strong-model"]));
        }
        scene.set_policy(
            r#"
[[review]]
name = "wide"
steps = [
  [
    { name = "style", requires = ["review"], instructions = "Naming." },
    { name = "security", requires = ["review", "strong-model"] },
    { requires = ["review", "strong-model"] },
  ],
  "principal",
]
"#,
        );
        scene
    }

    fn status(&self, item: ItemId) -> Option<ReviewStatus> {
        self.state.review_status(item)
    }

    fn device(&self) -> crate::identity::DeviceKey {
        self.fixture.devices[0].key()
    }

    fn agent(&self, name: &str) -> AgentId {
        AgentId {
            principal: self.fixture.founder.key(),
            name: AgentName::new(name).unwrap(),
        }
    }
}

fn awaiting_agent() -> ReviewStatus {
    ReviewStatus::Awaiting {
        steps: vec![agent_step(None, &["review", "strong-model"], None)],
        approved: Vec::new(),
    }
}

fn awaiting_human() -> ReviewStatus {
    ReviewStatus::Awaiting {
        steps: vec![ReviewStep::Principal],
        approved: Vec::new(),
    }
}

fn agent_step(name: Option<&str>, requires: &[&str], instructions: Option<&str>) -> ReviewStep {
    ReviewStep::Agent {
        name: name.map(|name| crate::name::ReviewStepName::new(name).unwrap()),
        requires: requires
            .iter()
            .map(|name| crate::name::Requirement::new(*name).unwrap())
            .collect(),
        instructions: instructions.map(str::to_string),
    }
}

#[test]
fn work_without_a_level_has_no_status_until_it_is_merged() {
    let mut scene = Scene::new();
    let item = scene.apply(create("task", "plain")).id().into();
    assert_eq!(scene.status(item), None);
    scene.submit(item, commit(1));
    assert_eq!(scene.status(item), None);
    scene.apply(merged(item, commit(1), commit(9)));
    assert_eq!(
        scene.status(item),
        Some(ReviewStatus::Merged { commit: commit(9) })
    );
}

#[test]
fn the_latest_merge_is_the_one_recorded_last() {
    let mut scene = Scene::new();
    let item = scene.apply(create("task", "plain")).id().into();
    let latest_merge = |state: &State| {
        let Some(crate::item::ItemEntry::Live(live)) = state.item(item) else {
            panic!("the item is live");
        };
        live.latest_merge()
            .map(|merge| (merge.commit, merge.merged_at()))
    };
    assert_eq!(latest_merge(&scene.state), None);
    let first = scene.apply(merged(item, commit(1), commit(9)));
    assert_eq!(
        latest_merge(&scene.state),
        Some((commit(9), first.content.hlc))
    );
    let second = scene.apply(merged(item, commit(2), commit(8)));
    assert_eq!(
        latest_merge(&scene.state),
        Some((commit(8), second.content.hlc))
    );
}

#[test]
fn a_level_is_walked_one_approval_at_a_time() {
    let mut scene = Scene::new();
    let item = scene.submitted("critical");
    assert_eq!(scene.status(item), Some(awaiting_agent()));
    scene.apply(verdict(item, commit(1), Verdict::Approve));
    assert_eq!(scene.status(item), Some(awaiting_human()));
    scene.apply(verdict(item, commit(1), Verdict::Approve));
    assert_eq!(
        scene.status(item),
        Some(ReviewStatus::Approved {
            commit: Some(commit(1))
        })
    );
    scene.apply(merged(item, commit(1), commit(9)));
    assert_eq!(
        scene.status(item),
        Some(ReviewStatus::Merged { commit: commit(9) })
    );
}

#[test]
fn escalating_puts_a_human_step_right_after_the_current_one() {
    let mut scene = Scene::new();
    let item = scene.submitted("standard");
    scene.agent_verdict(item, Verdict::Escalate);
    assert_eq!(scene.status(item), Some(awaiting_human()));
    scene.apply(verdict(item, commit(1), Verdict::Approve));
    assert_eq!(
        scene.status(item),
        Some(ReviewStatus::Approved {
            commit: Some(commit(1))
        })
    );

    let item = scene.submitted("critical");
    scene.agent_verdict(item, Verdict::Escalate);
    scene.apply(verdict(item, commit(1), Verdict::Approve));
    assert_eq!(
        scene.status(item),
        Some(awaiting_human()),
        "the original principal step is still to come"
    );
    scene.apply(verdict(item, commit(1), Verdict::Approve));
    assert_eq!(
        scene.status(item),
        Some(ReviewStatus::Approved {
            commit: Some(commit(1))
        })
    );
}

#[test]
fn agent_approvals_do_not_satisfy_a_principal_step() {
    let mut scene = Scene::new();
    let item = scene.submitted("critical");
    scene.agent_verdict(item, Verdict::Approve);
    scene.agent_verdict(item, Verdict::Approve);
    assert_eq!(
        scene.status(item),
        Some(awaiting_human()),
        "the second agent approval is ignored, it does not stand in for the principal"
    );
    scene.apply(verdict(item, commit(1), Verdict::Approve));
    assert_eq!(
        scene.status(item),
        Some(ReviewStatus::Approved {
            commit: Some(commit(1))
        })
    );
}

#[test]
fn an_agent_verdict_after_an_escalation_does_not_pass_the_principal_step() {
    let mut scene = Scene::new();
    let item = scene.submitted("standard");
    scene.agent_verdict(item, Verdict::Escalate);
    scene.agent_verdict(item, Verdict::Approve);
    scene.agent_verdict(item, Verdict::Escalate);
    assert_eq!(scene.status(item), Some(awaiting_human()));
    scene.apply(verdict(item, commit(1), Verdict::Approve));
    assert_eq!(
        scene.status(item),
        Some(ReviewStatus::Approved {
            commit: Some(commit(1))
        })
    );
}

#[test]
fn a_principal_approval_stands_in_for_an_agent_step() {
    let mut scene = Scene::new();
    let item = scene.submitted("critical");
    scene.apply(verdict(item, commit(1), Verdict::Approve));
    assert_eq!(scene.status(item), Some(awaiting_human()));
}

#[test]
fn requested_changes_end_the_review_and_a_new_commit_starts_it_again() {
    let mut scene = Scene::new();
    let item = scene.submitted("standard");
    scene.apply(verdict(item, commit(1), Verdict::RequestChanges));
    assert_eq!(scene.status(item), Some(ReviewStatus::ChangesRequested));

    scene.submit(item, commit(2));
    assert_eq!(scene.status(item), Some(awaiting_agent()));
    scene.apply(verdict(item, commit(1), Verdict::Approve));
    assert_eq!(
        scene.status(item),
        Some(awaiting_agent()),
        "a verdict on an older commit counts for nothing"
    );
    scene.apply(verdict(item, commit(2), Verdict::Approve));
    assert_eq!(
        scene.status(item),
        Some(ReviewStatus::Approved {
            commit: Some(commit(2))
        })
    );
}

#[test]
fn requesting_changes_reopens_the_item_in_the_same_event() {
    let mut scene = Scene::new();
    let item = scene.submitted("standard");
    scene.apply(moved(item, ItemState::Review));
    scene.apply(verdict(item, commit(1), Verdict::RequestChanges));
    let policy = &scene.state.policy().unwrap().policy;
    let Some(crate::item::ItemEntry::Live(live)) = scene.state.item(item) else {
        panic!("the item is live")
    };
    assert_eq!(live.state(policy), ItemState::Open);
}

#[test]
fn submitting_the_same_commit_again_starts_the_level_over() {
    let mut scene = Scene::new();
    let item = scene.submitted("standard");
    scene.apply(verdict(item, commit(1), Verdict::RequestChanges));
    scene.submit(item, commit(1));
    assert_eq!(scene.status(item), Some(awaiting_agent()));
}

#[test]
fn approved_work_and_work_without_review_wait_for_the_merge() {
    let mut scene = Scene::new();
    let reviewed = scene.submitted("standard");
    let unreviewed = scene.apply(create("task", "plain")).id().into();
    scene.submit(unreviewed, commit(3));
    let running = scene.apply(create("task", "still going")).id().into();
    scene.apply(start_session("coder", running));

    let pending = |state: &State| -> Vec<(ItemId, CommitId)> {
        let mut pending: Vec<_> = state
            .pending_merges()
            .into_iter()
            .map(|pending| (pending.item, pending.commit))
            .collect();
        pending.sort();
        pending
    };
    assert_eq!(pending(&scene.state), [(unreviewed, commit(3))]);
    scene.apply(verdict(reviewed, commit(1), Verdict::Approve));
    let pending_now = pending(&scene.state);
    let mut expected = vec![(reviewed, commit(1)), (unreviewed, commit(3))];
    expected.sort();
    assert_eq!(pending_now, expected);

    scene.apply(merged(reviewed, commit(1), commit(9)));
    assert_eq!(
        pending(&scene.state),
        expected,
        "merged work waits for its merge to move it"
    );
    scene.apply(moved(reviewed, ItemState::Done));
    assert_eq!(pending(&scene.state), [(unreviewed, commit(3))]);
}

#[test]
fn reviewers_are_offered_what_they_are_capable_of_and_did_not_work_on() {
    let mut scene = Scene::new();
    let item = scene.submitted("standard");
    let offered = |scene: &Scene, name: &str| {
        scene
            .state
            .reviewable(scene.device(), &scene.agent(name), &BTreeSet::new())
    };
    assert_eq!(offered(&scene, "reviewer"), [(item, Some(commit(1)), None)]);
    assert!(offered(&scene, "weak").is_empty(), "missing strong-model");
    assert!(
        offered(&scene, "coder").is_empty(),
        "the author never reviews"
    );

    let held: SessionId = scene
        .apply(start_review("reviewer", item, commit(1)))
        .id()
        .into();
    assert!(
        offered(&scene, "reviewer").is_empty(),
        "a review session holds the step"
    );
    scene.apply(end_session(held, EndReason::Reviewed));
    assert_eq!(
        offered(&scene, "reviewer"),
        [(item, Some(commit(1)), None)],
        "after changes a reviewer looks again"
    );
}

#[test]
fn the_hardware_of_the_device_counts_for_a_review_step() {
    use crate::event::DeviceProfile;

    let mut scene = Scene::new();
    let item = scene.submitted("standard");
    let offered = |scene: &Scene| {
        scene
            .state
            .reviewable(scene.device(), &scene.agent("weak"), &BTreeSet::new())
    };
    assert!(offered(&scene).is_empty(), "missing strong-model");
    scene.apply(EventBody::DeviceProfile(DeviceProfile {
        name: "laptop".into(),
        hardware: "gpu, strong-model".into(),
        agents: [(AgentName::new("weak").unwrap(), 1)].into(),
        max_sessions: 1,
    }));
    assert_eq!(offered(&scene), [(item, Some(commit(1)), None)]);
}

#[test]
fn reviewers_that_do_not_finish_are_retried_until_the_principal_decides() {
    let mut scene = Scene::new();
    let item = scene.submitted("standard");
    let offered = |scene: &Scene| {
        scene
            .state
            .reviewable(scene.device(), &scene.agent("reviewer"), &BTreeSet::new())
    };
    for attempt in 1..=3 {
        assert_eq!(
            offered(&scene),
            [(item, Some(commit(1)), None)],
            "attempt {attempt}"
        );
        let session: SessionId = scene
            .apply(start_review("reviewer", item, commit(1)))
            .id()
            .into();
        scene.apply(end_session(session, EndReason::Released));
    }
    assert!(
        offered(&scene).is_empty(),
        "the standard level gives up after three"
    );
    let waiting: Vec<_> = scene
        .state
        .inbox()
        .into_iter()
        .map(|entry| entry.waiting)
        .collect();
    assert!(
        matches!(
            waiting.as_slice(),
            [crate::inbox::Waiting::Recovery { failures: 3, .. }]
        ),
        "{waiting:?}"
    );
}

#[test]
fn human_steps_are_not_offered_to_agents() {
    let mut scene = Scene::new();
    let item = scene.submitted("critical");
    scene.apply(verdict(item, commit(1), Verdict::Approve));
    assert!(
        scene
            .state
            .reviewable(scene.device(), &scene.agent("reviewer"), &BTreeSet::new())
            .is_empty()
    );
}

mod guards {
    use super::*;

    fn check(
        scene: &Scene,
        agent: &str,
        session: SessionId,
        body: &EventBody,
    ) -> Result<(), PolicyViolation> {
        scene.state.check_actor(
            scene.fixture.devices[0].key(),
            &scene.fixture.agent_actor(agent, session),
            body,
        )
    }

    #[test]
    fn an_agent_reviews_only_from_a_review_session_of_that_commit() {
        let mut scene = Scene::new();
        let item = scene.submitted("standard");
        let review: SessionId = scene
            .apply(start_review("reviewer", item, commit(1)))
            .id()
            .into();
        let ok = verdict(item, commit(1), Verdict::Approve);
        assert_eq!(check(&scene, "reviewer", review, &ok), Ok(()));
        assert_eq!(
            check(
                &scene,
                "reviewer",
                review,
                &verdict(item, commit(2), Verdict::Approve)
            ),
            Err(PolicyViolation::NotTheReviewer)
        );

        let work: SessionId = scene.apply(start_session("weak", item)).id().into();
        assert_eq!(
            check(&scene, "weak", work, &ok),
            Err(PolicyViolation::NotTheReviewer),
            "a work session does not review"
        );
    }

    #[test]
    fn an_agent_that_worked_on_the_item_cannot_review_it() {
        let mut scene = Scene::new();
        let item = scene.submitted("standard");
        let review: SessionId = scene
            .apply(start_review("coder", item, commit(1)))
            .id()
            .into();
        assert_eq!(
            check(
                &scene,
                "coder",
                review,
                &verdict(item, commit(1), Verdict::Approve)
            ),
            Err(PolicyViolation::NotTheReviewer)
        );
    }

    #[test]
    fn only_agents_escalate_and_only_the_submitted_commit_is_reviewed() {
        let mut scene = Scene::new();
        let item = scene.submitted("standard");
        let device = scene.fixture.devices[0].key();
        assert_eq!(
            scene.state.check_actor(
                device,
                &scene.fixture.founder_actor(),
                &verdict(item, commit(1), Verdict::Escalate)
            ),
            Err(PolicyViolation::PrincipalCannotEscalate)
        );
        assert_eq!(
            scene.state.check_actor(
                device,
                &scene.fixture.founder_actor(),
                &verdict(item, commit(1), Verdict::Approve)
            ),
            Ok(())
        );
        assert_eq!(
            scene
                .state
                .check_policy(&verdict(item, commit(1), Verdict::Approve)),
            Ok(())
        );
        assert_eq!(
            scene
                .state
                .check_policy(&verdict(item, commit(2), Verdict::Approve)),
            Err(PolicyViolation::StaleReview(item))
        );
    }

    #[test]
    fn a_review_session_does_not_close_because_its_item_is_blocked() {
        let mut scene = Scene::new();
        let item = scene.submitted("standard");
        let review: SessionId = scene
            .apply(start_review("reviewer", item, commit(1)))
            .id()
            .into();
        assert_eq!(
            scene.state.turn_outcome(review, 0, 3, None),
            crate::lifecycle::TurnOutcome::Continue
        );
    }
}

mod inbox {
    use super::*;
    use crate::inbox::{InboxEntry, Waiting};

    #[test]
    fn what_waits_on_a_person_is_listed_newest_first() {
        let mut scene = Scene::new();
        let asking: ItemId = scene.apply(create("task", "parse durations")).id().into();

        let careful = scene.submitted("critical");
        assert!(
            scene.state.inbox().is_empty(),
            "an agent has the first step of the review"
        );
        scene.apply(verdict(careful, commit(1), Verdict::Approve));
        let question: crate::id::CommentId =
            scene.apply(blocker(asking, "which crate?")).id().into();

        let light = scene.submitted("standard");
        scene.apply(verdict(light, commit(1), Verdict::Approve));

        assert_eq!(
            scene.state.inbox(),
            [
                InboxEntry {
                    item: asking,
                    waiting: Waiting::Blocker { comment: question }
                },
                InboxEntry {
                    item: careful,
                    waiting: Waiting::PrincipalReview {
                        commit: Some(commit(1)),
                        stale: None,
                        reason: PrincipalReviewReason::Step,
                    }
                },
            ]
        );
    }
}

#[test]
fn a_review_step_nobody_hosted_can_take_is_noticed() {
    use crate::autonomy::{Autonomy, StartMode};
    use crate::event::{AgentDefined, DeviceProfile};
    use crate::name::{HarnessName, Requirement};

    let mut scene = Scene::new();
    let item = scene.submitted("standard");
    assert_eq!(
        scene.state.unstaffed_reviews(),
        [(item, Vec::new())],
        "nobody is hosted"
    );

    let reviewer = AgentDefined {
        name: AgentName::new("reviewer").unwrap(),
        harness: HarnessName::new("opencode").unwrap(),
        model: None,
        instructions: None,
        capabilities: ["review", "strong-model"]
            .into_iter()
            .map(|name| Requirement::new(name).unwrap())
            .collect(),
        lanes: vec![],
        autonomy: Autonomy {
            start: StartMode::Auto,
            ..Autonomy::default()
        },
    };
    scene.apply(EventBody::AgentDefined(reviewer));
    assert_eq!(
        scene.state.unstaffed_reviews(),
        [(item, Vec::new())],
        "defined, not hosted"
    );

    scene.apply(EventBody::DeviceProfile(DeviceProfile {
        name: "laptop".into(),
        hardware: String::new(),
        agents: [(AgentName::new("reviewer").unwrap(), 1)].into(),
        max_sessions: 1,
    }));
    assert!(scene.state.unstaffed_reviews().is_empty());
}

#[test]
fn work_without_commits_is_reviewed_and_finished_by_its_approval() {
    let mut scene = Scene::new();
    let finding = scene
        .apply(create_reviewed("task", "check the parser", "manual"))
        .id()
        .into();
    let session: SessionId = scene.apply(start_session("coder", finding)).id().into();
    scene.apply(moved(finding, ItemState::Review));
    scene.apply(end_session(session, EndReason::Submitted));
    assert_eq!(scene.status(finding), Some(awaiting_human()));
    assert!(scene.state.finished_reviews().is_empty());

    scene.apply(verdict(finding, None, Verdict::Approve));
    assert_eq!(
        scene.status(finding),
        Some(ReviewStatus::Approved { commit: None })
    );
    assert_eq!(scene.state.finished_reviews(), [finding]);
    assert!(scene.state.pending_merges().is_empty());

    scene.apply(moved(finding, ItemState::Done));
    assert!(scene.state.finished_reviews().is_empty());
}

#[test]
fn merged_work_is_finished_by_the_daemon_and_waits_on_nobody() {
    let mut scene = Scene::new();
    let item = scene
        .apply(create_reviewed("task", "parse durations", "standard"))
        .id()
        .into();
    // As the daemon writes a submission: the item moves to review, then the session ends.
    let session: SessionId = scene.apply(start_session("coder", item)).id().into();
    scene.apply(moved(item, ItemState::Review));
    scene.apply(submit_session(session, commit(1)));
    scene.apply(verdict(item, commit(1), Verdict::Approve));
    assert!(
        scene.state.finished_reviews().is_empty(),
        "it is merged first"
    );

    scene.apply(merged(item, commit(1), commit(9)));
    assert!(
        scene.state.finished_reviews().is_empty(),
        "the merge moves it, not the dispatcher"
    );
    assert_eq!(scene.state.pending_merges().len(), 1);
    assert!(
        scene.state.inbox().is_empty(),
        "merged work waiting to be moved is no proposal"
    );

    scene.apply(moved(item, ItemState::Open));
    scene.apply(moved(item, ItemState::Review));
    assert!(scene.state.pending_merges().is_empty());
    assert_eq!(
        scene.state.inbox(),
        [crate::inbox::InboxEntry {
            item,
            waiting: crate::inbox::Waiting::Proposal
        }],
        "moved back into review by hand, it is the principal's to move on"
    );
}

#[test]
fn approved_work_without_commits_does_not_finish_an_item_that_was_moved_since() {
    let mut scene = Scene::new();
    let finding = scene
        .apply(create_reviewed("task", "check the parser", "manual"))
        .id()
        .into();
    let session: SessionId = scene.apply(start_session("coder", finding)).id().into();
    scene.apply(moved(finding, ItemState::Review));
    scene.apply(end_session(session, EndReason::Submitted));
    scene.apply(verdict(finding, None, Verdict::Approve));
    scene.apply(moved(finding, ItemState::Open));
    scene.apply(moved(finding, ItemState::Review));
    assert!(scene.state.finished_reviews().is_empty());
}

#[test]
fn change_requests_hold_up_approval_and_the_next_submission() {
    let mut scene = Scene::new();
    let item = scene.submitted("standard");
    assert_eq!(
        scene
            .state
            .check_policy(&verdict(item, commit(1), Verdict::RequestChanges)),
        Err(PolicyViolation::NoChangeRequests(item))
    );

    let change: crate::id::CommentId = scene.apply(change_request(item, "add a test")).id().into();
    assert_eq!(
        scene
            .state
            .check_policy(&verdict(item, commit(1), Verdict::Approve)),
        Err(PolicyViolation::OpenChangeRequests(item))
    );
    scene.apply(verdict(item, commit(1), Verdict::RequestChanges));
    scene.apply(moved(item, ItemState::Open));

    let session: SessionId = scene.apply(start_session("coder", item)).id().into();
    let submit = submit_session(session, commit(2));
    assert_eq!(
        scene.state.check_policy(&submit),
        Err(PolicyViolation::OpenChangeRequests(item))
    );
    scene.apply(resolving(item, "added one", &[change]));
    assert_eq!(scene.state.check_policy(&submit), Ok(()));
}

#[test]
fn check_failures_hold_up_approval_and_the_next_submission_like_change_requests() {
    let mut scene = Scene::new();
    let item = scene.submitted("standard");
    let failure: crate::id::CommentId = scene.apply(check_failure(item, "cargo test")).id().into();
    assert_eq!(
        scene
            .state
            .check_policy(&verdict(item, commit(1), Verdict::Approve)),
        Err(PolicyViolation::OpenChangeRequests(item))
    );
    assert_eq!(
        scene
            .state
            .check_policy(&verdict(item, commit(1), Verdict::RequestChanges)),
        Ok(())
    );
    scene.apply(verdict(item, commit(1), Verdict::RequestChanges));
    scene.apply(moved(item, ItemState::Open));

    let session: SessionId = scene.apply(start_session("coder", item)).id().into();
    let submit = submit_session(session, commit(2));
    assert_eq!(
        scene.state.check_policy(&submit),
        Err(PolicyViolation::OpenChangeRequests(item))
    );
    scene.apply(resolving(item, "fixed", &[failure]));
    assert_eq!(scene.state.check_policy(&submit), Ok(()));
}

#[test]
fn check_failures_of_merged_work_count_as_change_requests_of_the_project() {
    let mut scene = Scene::new();
    let item = scene.submitted("standard");
    scene.apply(change_request(item, "rename"));
    scene.apply(check_failure(item, "cargo test"));
    scene.apply(merged(item, commit(1), commit(9)));
    assert_eq!(scene.state.project_stats(0).change_requests, 2);
}

#[test]
fn a_daemon_change_request_after_the_offer_makes_the_principal_review_stale() {
    use crate::identity::Actor;
    use crate::inbox::Waiting;

    let mut scene = Scene::new();
    let item = scene.submitted("critical");
    assert!(matches!(
        scene.state.offered_reviews().as_slice(),
        [(offered, _, ReviewStep::Agent { .. })] if *offered == item
    ));
    scene.agent_verdict(item, Verdict::Approve);
    assert_eq!(
        scene.state.offered_reviews(),
        [(item, commit(1), ReviewStep::Principal)]
    );
    let stale_of = |scene: &Scene| match scene.state.inbox().as_slice() {
        [entry] => {
            let Waiting::PrincipalReview { stale, .. } = entry.waiting else {
                panic!("expected a principal review")
            };
            stale
        }
        other => panic!("expected one entry, found {other:?}"),
    };
    assert_eq!(stale_of(&scene), None);

    scene.clock += 1;
    let daemon = Actor::Daemon(scene.fixture.founder.key());
    let request = scene.fixture.author_as(
        &scene.state,
        0,
        daemon,
        scene.clock,
        change_request(item, "no longer merges: a.txt"),
    );
    scene
        .state
        .receive(request.clone(), crate::state::Trust::Untrusted)
        .unwrap();
    let stale = stale_of(&scene).expect("stale");
    assert_eq!(stale.since_ms, scene.clock);
    assert_eq!(stale.comment, request.id().into());

    scene.submit(item, commit(2));
    let (_, resubmitted) = scene.state.submission(item).unwrap();
    assert_eq!(
        scene.state.stale_since(item, resubmitted.hlc),
        None,
        "a new submission starts over"
    );
}

fn open_names(status: Option<ReviewStatus>) -> Vec<Option<String>> {
    let Some(ReviewStatus::Awaiting { steps, .. }) = status else {
        panic!("{status:?} is not awaiting");
    };
    steps
        .into_iter()
        .map(|step| match step {
            ReviewStep::Agent { name, .. } => name.map(|name| name.to_string()),
            ReviewStep::Principal => Some("principal".to_string()),
        })
        .collect()
}

mod at_once {
    use super::*;

    fn offered(scene: &Scene, agent: &str) -> Vec<Option<String>> {
        scene
            .state
            .reviewable(scene.device(), &scene.agent(agent), &BTreeSet::new())
            .into_iter()
            .map(|(_, _, step)| step.map(|name| name.to_string()))
            .collect()
    }

    #[test]
    fn three_steps_at_once_need_three_agents() {
        let mut scene = Scene::with_wide_level();
        let item = scene.submitted("wide");
        assert_eq!(
            open_names(scene.status(item)),
            [Some("style".into()), Some("security".into()), None]
        );
        assert_eq!(
            offered(&scene, "weak"),
            [Some("style".to_string())],
            "only the step it has the capabilities for"
        );
        assert_eq!(offered(&scene, "reviewer").len(), 3);

        // A session holds the step it was started for, and no other agent is offered it.
        scene.apply(start_review_step(
            "reviewer",
            item,
            commit(1),
            Some("security"),
        ));
        assert_eq!(offered(&scene, "second"), [Some("style".to_string()), None]);

        scene.step_verdict("second", item, Some("style"), Verdict::Approve);
        assert_eq!(
            offered(&scene, "second"),
            [],
            "an agent that gave a verdict is not offered the item again"
        );
        assert_eq!(offered(&scene, "third"), [None]);
        assert_eq!(
            open_names(scene.status(item)),
            [Some("security".into()), None]
        );

        scene.step_verdict("third", item, None, Verdict::Approve);
        assert_eq!(open_names(scene.status(item)), [Some("security".into())]);
        // A second approval of a step that is closed counts for nothing.
        scene.step_verdict("weak", item, Some("style"), Verdict::Approve);
        assert_eq!(open_names(scene.status(item)), [Some("security".into())]);
    }

    #[test]
    fn the_entry_passes_when_each_step_approved() {
        let mut scene = Scene::with_wide_level();
        let item = scene.submitted("wide");
        scene.step_verdict("second", item, Some("style"), Verdict::Approve);
        scene.step_verdict("third", item, Some("security"), Verdict::Approve);
        scene.step_verdict("reviewer", item, None, Verdict::Approve);
        assert_eq!(scene.status(item), Some(awaiting_human()));
        scene.apply(verdict(item, commit(1), Verdict::Approve));
        assert_eq!(
            scene.status(item),
            Some(ReviewStatus::Approved {
                commit: Some(commit(1))
            })
        );
    }

    #[test]
    fn one_step_requesting_changes_sends_the_item_back() {
        let mut scene = Scene::with_wide_level();
        let item = scene.submitted("wide");
        scene.step_verdict("second", item, Some("style"), Verdict::Approve);
        scene.step_verdict("third", item, Some("security"), Verdict::RequestChanges);
        assert_eq!(scene.status(item), Some(ReviewStatus::ChangesRequested));
    }

    #[test]
    fn the_other_steps_finish_after_one_requested_changes() {
        let mut scene = Scene::with_wide_level();
        let item = scene.submitted("wide");
        let security: SessionId = scene
            .apply(start_review_step(
                "third",
                item,
                commit(1),
                Some("security"),
            ))
            .id()
            .into();
        scene.step_verdict("second", item, Some("style"), Verdict::RequestChanges);
        assert_eq!(scene.status(item), Some(ReviewStatus::ChangesRequested));
        assert_eq!(
            scene.state.turn_outcome(security, 0, 3, None),
            TurnOutcome::Continue,
            "its session goes on until it gives a verdict"
        );

        // Its approval is written and counts for nothing.
        let actor = scene.fixture.agent_actor("third", security);
        scene.clock += 1;
        let approval = scene.fixture.author_as(
            &scene.state,
            0,
            actor,
            scene.clock,
            verdict(item, commit(1), Verdict::Approve),
        );
        scene
            .state
            .receive(approval, crate::state::Trust::Untrusted)
            .expect("the approval is valid");
        assert_eq!(scene.status(item), Some(ReviewStatus::ChangesRequested));

        // Once new work replaced the commit, its review has nothing left to judge.
        let late: SessionId = scene
            .apply(start_review_step(
                "reviewer",
                item,
                commit(1),
                Some("security"),
            ))
            .id()
            .into();
        scene.submit(item, commit(2));
        assert_eq!(
            scene.state.turn_outcome(late, 0, 3, None),
            TurnOutcome::Close
        );
    }

    #[test]
    fn a_review_started_by_hand_takes_the_step_asked_for() {
        use crate::review::ReviewStartError;
        use crate::session::SessionRole;

        let mut scene = Scene::with_wide_level();
        let item = scene.submitted("wide");
        let step = |name: &str| Some(crate::name::ReviewStepName::new(name).unwrap());
        let review_of = |scene: &Scene, name: Option<&str>| {
            scene
                .state
                .review_to_start(item, name.and_then(step).as_ref())
        };
        assert_eq!(
            review_of(&scene, Some("security")),
            Some(Ok(SessionRole::Review {
                commit: Some(commit(1)),
                step: step("security")
            }))
        );
        assert_eq!(
            review_of(&scene, None),
            Some(Ok(SessionRole::Review {
                commit: Some(commit(1)),
                step: None
            })),
            "no name is the unnamed step"
        );
        assert!(matches!(
            review_of(&scene, Some("perf")),
            Some(Err(ReviewStartError::NoSuchStep { .. }))
        ));

        scene.apply(start_review_step(
            "reviewer",
            item,
            commit(1),
            Some("security"),
        ));
        assert!(
            matches!(
                review_of(&scene, Some("security")),
                Some(Err(ReviewStartError::NoSuchStep { .. })),
            ),
            "a held step is not offered"
        );
        scene.apply(start_review_step("second", item, commit(1), None));
        assert_eq!(
            review_of(&scene, None),
            Some(Ok(SessionRole::Review {
                commit: Some(commit(1)),
                step: step("style")
            })),
            "the only one open is taken without a name"
        );
    }

    #[test]
    fn a_principal_approval_passes_the_whole_entry() {
        let mut scene = Scene::with_wide_level();
        let item = scene.submitted("wide");
        scene.step_verdict("second", item, Some("style"), Verdict::Approve);
        scene.apply(verdict(item, commit(1), Verdict::Approve));
        assert_eq!(scene.status(item), Some(awaiting_human()));
    }

    #[test]
    fn an_escalation_adds_a_principal_step_after_the_entry() {
        let mut scene = Scene::with_wide_level();
        let item = scene.submitted("wide");
        scene.step_verdict("second", item, Some("style"), Verdict::Escalate);
        scene.step_verdict("third", item, Some("security"), Verdict::Approve);
        scene.step_verdict("reviewer", item, None, Verdict::Approve);
        assert_eq!(scene.status(item), Some(awaiting_human()));
        scene.apply(verdict(item, commit(1), Verdict::Approve));
        assert_eq!(scene.status(item), Some(awaiting_human()), "the added one");
        scene.apply(verdict(item, commit(1), Verdict::Approve));
        assert!(matches!(
            scene.status(item),
            Some(ReviewStatus::Approved { .. })
        ));
    }

    #[test]
    fn approvals_under_a_policy_of_the_old_form_still_fold() {
        let mut scene = Scene::new();
        let old = crate::policy::DEFAULT_POLICY_TOML.replace(
            "steps = [{ requires = [\"review\", \"strong-model\"] }, \"principal\"]",
            "steps = [{ agent = { requires = [\"review\", \"strong-model\"] } }, \"human\"]",
        );
        assert!(old.contains("agent = {"));
        let body = scene.fixture.policy_set(
            &scene.state,
            0,
            scene.clock + 1,
            &scene.fixture.founder,
            &old,
        );
        scene.apply(body);
        let item = scene.submitted("critical");
        assert_eq!(scene.status(item), Some(awaiting_agent()));
        scene.agent_verdict(item, Verdict::Approve);
        assert_eq!(scene.status(item), Some(awaiting_human()));
    }
}

mod round_limit {
    use super::*;

    /// A task under the level `bounded`, of one agent step that sends the work back twice at most,
    /// submitted at `commit(1)`.
    fn bounded() -> (Scene, ItemId) {
        let mut scene = Scene::new();
        scene.set_policy(
            "[[review]]\nname = \"bounded\"\nsteps = [{ requires = [\"review\"] }]\nmax_rounds = 2\n",
        );
        let item = scene
            .apply(create_reviewed("task", "work", "bounded"))
            .id()
            .into();
        resubmit(&mut scene, item, commit(1));
        (scene, item)
    }

    /// As the daemon writes a submission, so that the principal does not act on the item.
    fn resubmit(scene: &mut Scene, item: ItemId, at: CommitId) {
        let session: SessionId = scene.apply(start_session("coder", item)).id().into();
        let daemon = Actor::Daemon(scene.fixture.founder.key());
        scene.apply_as(daemon, moved(item, ItemState::Review));
        scene.apply(submit_session(session, at));
    }

    /// `weak` requests changes on `at`, in a review session for the unnamed step.
    fn request_changes(scene: &mut Scene, item: ItemId, at: CommitId) {
        request_changes_as(scene, "weak", item, at, None);
    }

    /// `agent` writes a change request and then requests changes on `at`, in a review session for
    /// `step`, recording the round limit as `wf review` does.
    fn request_changes_as(
        scene: &mut Scene,
        agent: &str,
        item: ItemId,
        at: CommitId,
        step: Option<&str>,
    ) {
        let session: SessionId = scene
            .apply(start_review_step(agent, item, at, step))
            .id()
            .into();
        request_changes_in(scene, agent, item, at, session);
    }

    /// `agent` writes a change request and then requests changes on `at` in its review `session`.
    fn request_changes_in(
        scene: &mut Scene,
        agent: &str,
        item: ItemId,
        at: CommitId,
        session: SessionId,
    ) {
        let actor = scene.fixture.agent_actor(agent, session);
        scene.apply_as(actor.clone(), change_request(item, "not yet"));
        let body = EventBody::Reviewed(crate::event::Reviewed {
            item,
            commit: Some(at),
            verdict: Verdict::RequestChanges,
            because: None,
            round_limit_reached: scene.state.round_limit_reached(
                item,
                Verdict::RequestChanges,
                &actor,
            ),
        });
        assert_eq!(scene.state.check_policy(&body), Ok(()));
        scene.apply_as(actor, body);
        scene.apply(end_session(session, EndReason::Reviewed));
    }

    fn state_of(scene: &Scene, item: ItemId) -> ItemState {
        scene.state.live_state(item).unwrap()
    }

    #[test]
    fn requests_for_changes_send_the_work_back_until_the_limit_then_the_principal_decides() {
        let (mut scene, item) = bounded();
        for (round, at) in [(1, commit(1)), (2, commit(2))] {
            request_changes(&mut scene, item, at);
            assert_eq!(state_of(&scene, item), ItemState::Open, "round {round}");
            assert_eq!(scene.status(item), Some(ReviewStatus::ChangesRequested));
            assert_eq!(scene.state.review_rounds(item, Rounds::All), round);
            resubmit(&mut scene, item, commit(round as u8 + 1));
        }

        request_changes(&mut scene, item, commit(3));
        assert_eq!(state_of(&scene, item), ItemState::Review);
        assert_eq!(
            scene.status(item),
            Some(ReviewStatus::RoundLimitReached { rounds: 2 })
        );
        assert_eq!(
            scene.state.review_rounds(item, Rounds::All),
            2,
            "a request that waits for the principal is no round"
        );
        let waiting: Vec<_> = scene
            .state
            .inbox()
            .into_iter()
            .filter(|entry| entry.item == item)
            .map(|entry| entry.waiting)
            .collect();
        assert_eq!(
            waiting,
            [crate::inbox::Waiting::PrincipalReview {
                commit: Some(commit(3)),
                stale: None,
                reason: PrincipalReviewReason::RoundLimit { rounds: 2 },
            }]
        );
        assert_eq!(scene.state.offered_reviews(), []);
        let entry = crate::inbox::InboxEntry {
            item,
            waiting: waiting[0].clone(),
        };
        assert_eq!(
            scene.state.what_waits(&entry),
            "changes requested again after 2 rounds, the round limit: approve with \
             `wf review --approve`, send it back with `wf review --request-changes`, or edit or \
             cancel it"
        );

        // A comment of the principal starts the count over, but it is not their decision.
        scene.apply(comment(item, "let me look"));
        assert_eq!(
            scene.status(item),
            Some(ReviewStatus::RoundLimitReached { rounds: 2 })
        );
        assert_eq!(scene.state.review_rounds(item, Rounds::All), 0);
        assert_eq!(
            scene
                .state
                .check_policy(&verdict(item, commit(3), Verdict::Approve)),
            Ok(()),
            "the principal approves over the open change requests"
        );
        let open_by_agents = |scene: &Scene| -> Vec<_> {
            let Some(crate::item::ItemEntry::Live(live)) = scene.state.item(item) else {
                panic!("the item is live");
            };
            let mut open: Vec<_> = live
                .comments()
                .iter()
                .filter(|(_, comment)| comment.kind.is_change_request() && comment.is_open())
                .map(|(id, comment)| (comment.at, *id))
                .collect();
            open.sort();
            open.into_iter().map(|(_, id)| id).collect()
        };
        let principal = scene.fixture.founder_actor();
        let overruled = scene
            .state
            .overruled_change_requests(item, Verdict::Approve, &principal);
        assert_eq!(overruled, open_by_agents(&scene));
        assert!(!overruled.is_empty());
        for (verdict, by) in [
            (Verdict::RequestChanges, principal.clone()),
            (Verdict::Escalate, principal.clone()),
            (Verdict::Approve, Actor::Daemon(scene.fixture.founder.key())),
        ] {
            assert_eq!(
                scene.state.overruled_change_requests(item, verdict, &by),
                [],
                "{verdict:?} by {by:?} overrules nothing"
            );
        }
        scene.apply(verdict(item, commit(3), Verdict::Approve));
        assert_eq!(
            scene
                .state
                .overruled_change_requests(item, Verdict::Approve, &principal),
            [],
            "only at the round limit"
        );
        assert_eq!(
            scene.status(item),
            Some(ReviewStatus::Approved {
                commit: Some(commit(3))
            })
        );
        assert_eq!(
            scene.state.pending_merges(),
            [crate::review::PendingMerge {
                item,
                commit: commit(3)
            }]
        );
    }

    #[test]
    fn the_principal_sends_it_back_for_another_round_of_its_own() {
        let (mut scene, item) = bounded();
        for round in 1..=2u8 {
            request_changes(&mut scene, item, commit(round));
            resubmit(&mut scene, item, commit(round + 1));
        }
        request_changes(&mut scene, item, commit(3));
        assert_eq!(state_of(&scene, item), ItemState::Review);

        scene.apply(verdict(item, commit(3), Verdict::RequestChanges));
        assert_eq!(state_of(&scene, item), ItemState::Open);
        assert_eq!(scene.status(item), Some(ReviewStatus::ChangesRequested));

        resubmit(&mut scene, item, commit(4));
        request_changes(&mut scene, item, commit(4));
        assert_eq!(
            state_of(&scene, item),
            ItemState::Open,
            "the count started over when the principal acted"
        );
    }

    #[test]
    fn several_steps_that_request_changes_on_one_submission_make_one_round() {
        let mut scene = Scene::with_wide_level();
        scene.set_policy(
            "[[review]]\nname = \"pair\"\nmax_rounds = 1\nsteps = [[\n\
             { name = \"style\", requires = [\"review\"] },\n\
             { name = \"security\", requires = [\"review\", \"strong-model\"] },\n]]\n",
        );
        let item = scene
            .apply(create_reviewed("task", "work", "pair"))
            .id()
            .into();
        resubmit(&mut scene, item, commit(1));
        request_changes_as(&mut scene, "weak", item, commit(1), Some("style"));
        request_changes_as(&mut scene, "reviewer", item, commit(1), Some("security"));
        assert_eq!(scene.state.review_rounds(item, Rounds::All), 1);
        assert_eq!(state_of(&scene, item), ItemState::Open);

        resubmit(&mut scene, item, commit(2));
        request_changes(&mut scene, item, commit(2));
        assert_eq!(
            scene.status(item),
            Some(ReviewStatus::RoundLimitReached { rounds: 1 })
        );
    }

    #[test]
    fn another_step_that_requests_changes_after_a_comment_of_the_principal_waits_too() {
        let mut scene = Scene::with_wide_level();
        scene.set_policy(
            "[[review]]\nname = \"pair\"\nmax_rounds = 1\nsteps = [[\n\
             { name = \"style\", requires = [\"review\"] },\n\
             { name = \"security\", requires = [\"review\", \"strong-model\"] },\n]]\n",
        );
        let item = scene
            .apply(create_reviewed("task", "work", "pair"))
            .id()
            .into();
        resubmit(&mut scene, item, commit(1));
        request_changes_as(&mut scene, "weak", item, commit(1), Some("style"));
        resubmit(&mut scene, item, commit(2));

        let security: SessionId = scene
            .apply(start_review_step(
                "reviewer",
                item,
                commit(2),
                Some("security"),
            ))
            .id()
            .into();
        request_changes_as(&mut scene, "weak", item, commit(2), Some("style"));
        assert_eq!(
            scene.status(item),
            Some(ReviewStatus::RoundLimitReached { rounds: 1 })
        );
        scene.apply(comment(item, "let me look"));
        assert_eq!(scene.state.review_rounds(item, Rounds::All), 0);

        request_changes_in(&mut scene, "reviewer", item, commit(2), security);
        assert_eq!(state_of(&scene, item), ItemState::Review);
        assert_eq!(
            scene.status(item),
            Some(ReviewStatus::RoundLimitReached { rounds: 1 })
        );
    }

    #[test]
    fn the_verdict_has_to_say_what_the_round_limit_makes_of_it() {
        let (mut scene, item) = bounded();
        for round in 1..=2u8 {
            request_changes(&mut scene, item, commit(round));
            resubmit(&mut scene, item, commit(round + 1));
        }
        let session: SessionId = scene
            .apply(start_review_step("weak", item, commit(3), None))
            .id()
            .into();
        let actor = scene.fixture.agent_actor("weak", session);
        let reviewed = |round_limit_reached| {
            EventBody::Reviewed(crate::event::Reviewed {
                item,
                commit: Some(commit(3)),
                verdict: Verdict::RequestChanges,
                because: None,
                round_limit_reached,
            })
        };
        let device = scene.device();
        assert_eq!(
            scene.state.check_actor(device, &actor, &reviewed(None)),
            Err(PolicyViolation::WrongRoundLimit(item))
        );
        assert_eq!(
            scene.state.check_actor(device, &actor, &reviewed(Some(1))),
            Err(PolicyViolation::WrongRoundLimit(item))
        );
        assert_eq!(
            scene.state.check_actor(device, &actor, &reviewed(Some(2))),
            Ok(())
        );
        assert_eq!(
            scene
                .state
                .check_actor(device, &scene.fixture.founder_actor(), &reviewed(Some(2))),
            Err(PolicyViolation::WrongRoundLimit(item)),
            "the principal's requests always send the work back"
        );
    }

    #[test]
    fn without_max_rounds_every_request_sends_the_work_back() {
        let mut scene = Scene::new();
        let item = scene
            .apply(create_reviewed("task", "work", "light"))
            .id()
            .into();
        for round in 1..=5u8 {
            resubmit(&mut scene, item, commit(round));
            request_changes(&mut scene, item, commit(round));
            assert_eq!(state_of(&scene, item), ItemState::Open);
        }
    }
}

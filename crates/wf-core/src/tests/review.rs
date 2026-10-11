use crate::event::EventBody;
use crate::id::{ItemId, SessionId};
use crate::identity::AgentId;
use crate::name::AgentName;
use crate::policy::ReviewStep;
use crate::review::ReviewStatus;
use crate::session::{CommitId, EndReason, Verdict};
use crate::state::State;
use crate::testing::*;
use crate::validate::PolicyViolation;

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

    fn submit(&mut self, item: ItemId, commit: CommitId) {
        let session: SessionId = self.apply(start_session("coder", item)).id().into();
        self.apply(end_session(
            session,
            EndReason::Submitted {
                commit: Some(commit),
            },
        ));
        self.apply(transition(item, "in_review"));
    }

    fn status(&self, item: ItemId) -> Option<ReviewStatus> {
        self.state.review_status(item)
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
        step: ReviewStep::Agent {
            requires: ["review", "strong-model"]
                .into_iter()
                .map(|name| crate::name::Requirement::new(name).unwrap())
                .collect(),
        },
    }
}

fn awaiting_human() -> ReviewStatus {
    ReviewStatus::Awaiting {
        step: ReviewStep::Human,
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
fn a_level_is_walked_one_approval_at_a_time() {
    let mut scene = Scene::new();
    let item = scene.submitted("critical");
    assert_eq!(scene.status(item), Some(awaiting_agent()));
    scene.apply(verdict(item, commit(1), Verdict::Approve));
    assert_eq!(scene.status(item), Some(awaiting_human()));
    scene.apply(verdict(item, commit(1), Verdict::Approve));
    assert_eq!(
        scene.status(item),
        Some(ReviewStatus::Approved { commit: commit(1) })
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
    scene.apply(verdict(item, commit(1), Verdict::Escalate));
    assert_eq!(scene.status(item), Some(awaiting_human()));
    scene.apply(verdict(item, commit(1), Verdict::Approve));
    assert_eq!(
        scene.status(item),
        Some(ReviewStatus::Approved { commit: commit(1) })
    );

    let item = scene.submitted("critical");
    scene.apply(verdict(item, commit(1), Verdict::Escalate));
    scene.apply(verdict(item, commit(1), Verdict::Approve));
    assert_eq!(
        scene.status(item),
        Some(awaiting_human()),
        "the original human step is still to come"
    );
    scene.apply(verdict(item, commit(1), Verdict::Approve));
    assert_eq!(
        scene.status(item),
        Some(ReviewStatus::Approved { commit: commit(1) })
    );
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
        Some(ReviewStatus::Approved { commit: commit(2) })
    );
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

    assert_eq!(scene.state.pending_merges(), [(unreviewed, commit(3))]);
    scene.apply(verdict(reviewed, commit(1), Verdict::Approve));
    let mut pending = scene.state.pending_merges();
    pending.sort();
    let mut expected = vec![(reviewed, commit(1)), (unreviewed, commit(3))];
    expected.sort();
    assert_eq!(pending, expected);

    scene.apply(merged(reviewed, commit(1), commit(9)));
    assert_eq!(scene.state.pending_merges(), [(unreviewed, commit(3))]);
}

#[test]
fn reviewers_are_offered_what_they_are_capable_of_and_did_not_work_on() {
    let mut scene = Scene::new();
    let item = scene.submitted("standard");
    let offered = |scene: &Scene, name: &str| scene.state.reviewable(&scene.agent(name));
    assert_eq!(offered(&scene, "reviewer"), [(item, commit(1))]);
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
        [(item, commit(1))],
        "after changes a reviewer looks again"
    );
}

#[test]
fn human_steps_are_not_offered_to_agents() {
    let mut scene = Scene::new();
    let item = scene.submitted("critical");
    scene.apply(verdict(item, commit(1), Verdict::Approve));
    assert!(scene.state.reviewable(&scene.agent("reviewer")).is_empty());
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
            Err(PolicyViolation::PersonCannotEscalate)
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
            scene.state.turn_outcome(review, 0, 3),
            crate::lifecycle::TurnOutcome::Continue
        );
    }
}

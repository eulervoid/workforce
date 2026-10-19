use crate::event::EventBody;
use crate::id::{ItemId, SessionId};
use crate::identity::AgentId;
use crate::item::ItemState;
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

    /// As the daemon writes a submission: the item moves to review, then the session ends.
    fn submit(&mut self, item: ItemId, commit: CommitId) {
        let session: SessionId = self.apply(start_session("coder", item)).id().into();
        self.apply(moved(item, ItemState::Review));
        self.apply(end_session(
            session,
            EndReason::Submitted {
                commit: Some(commit),
            },
        ));
    }

    /// A verdict given by `reviewer` from a review session of `commit(1)`, which it then ends.
    fn agent_verdict(&mut self, item: ItemId, verdict_given: Verdict) {
        let session: SessionId = self
            .apply(start_review("reviewer", item, commit(1)))
            .id()
            .into();
        self.clock += 1;
        let actor = self.fixture.agent_actor("reviewer", session);
        let body = verdict(item, commit(1), verdict_given);
        let event = self
            .fixture
            .author_as(&self.state, 0, actor, self.clock, body);
        self.state
            .receive(event, crate::state::Trust::Untrusted)
            .expect("the verdict is valid");
        self.apply(end_session(session, EndReason::Reviewed));
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
        step: ReviewStep::Principal,
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
    let offered =
        |scene: &Scene, name: &str| scene.state.reviewable(scene.device(), &scene.agent(name));
    assert_eq!(offered(&scene, "reviewer"), [(item, Some(commit(1)))]);
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
        [(item, Some(commit(1)))],
        "after changes a reviewer looks again"
    );
}

#[test]
fn the_hardware_of_the_device_counts_for_a_review_step() {
    use crate::event::DeviceProfile;

    let mut scene = Scene::new();
    let item = scene.submitted("standard");
    let offered = |scene: &Scene| scene.state.reviewable(scene.device(), &scene.agent("weak"));
    assert!(offered(&scene).is_empty(), "missing strong-model");
    scene.apply(EventBody::DeviceProfile(DeviceProfile {
        name: "laptop".into(),
        hardware: "gpu, strong-model".into(),
        agents: [(AgentName::new("weak").unwrap(), 1)].into(),
        max_sessions: 1,
    }));
    assert_eq!(offered(&scene), [(item, Some(commit(1)))]);
}

#[test]
fn reviewers_that_do_not_finish_are_retried_until_the_principal_decides() {
    let mut scene = Scene::new();
    let item = scene.submitted("standard");
    let offered = |scene: &Scene| {
        scene
            .state
            .reviewable(scene.device(), &scene.agent("reviewer"))
    };
    for attempt in 1..=3 {
        assert_eq!(
            offered(&scene),
            [(item, Some(commit(1)))],
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
            .reviewable(scene.device(), &scene.agent("reviewer"))
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
                    item: light,
                    waiting: Waiting::Merge { commit: commit(1) }
                },
                InboxEntry {
                    item: asking,
                    waiting: Waiting::Blocker { comment: question }
                },
                InboxEntry {
                    item: careful,
                    waiting: Waiting::PrincipalReview {
                        commit: Some(commit(1))
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
    assert_eq!(scene.state.unstaffed_reviews(), [item], "nobody is hosted");

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
        [item],
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
    scene.apply(end_session(session, EndReason::Submitted { commit: None }));
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
    scene.apply(end_session(
        session,
        EndReason::Submitted {
            commit: Some(commit(1)),
        },
    ));
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
    scene.apply(end_session(session, EndReason::Submitted { commit: None }));
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
    let submit = end_session(
        session,
        EndReason::Submitted {
            commit: Some(commit(2)),
        },
    );
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
    let submit = end_session(
        session,
        EndReason::Submitted {
            commit: Some(commit(2)),
        },
    );
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

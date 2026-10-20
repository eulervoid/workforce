use std::collections::BTreeMap;

use crate::autonomy::{Autonomy, StartMode};
use crate::event::{AgentDefined, DeviceProfile, EventBody, ItemCreated};
use crate::id::ItemId;
use crate::identity::AgentId;
use crate::name::{AgentName, HarnessName, LaneName, Requirement};
use crate::recovery::Recovery;
use crate::session::{EndReason, MachineFault, SessionRole, Verdict};
use crate::state::State;
use crate::testing::*;

fn agent(name: &str, lanes: &[&str], capabilities: &[&str], start: StartMode) -> EventBody {
    let autonomy = Autonomy {
        start,
        ..Autonomy::default()
    };
    EventBody::AgentDefined(AgentDefined {
        name: AgentName::new(name).unwrap(),
        harness: HarnessName::new("opencode").unwrap(),
        model: None,
        instructions: None,
        capabilities: capabilities
            .iter()
            .map(|name| Requirement::new(*name).unwrap())
            .collect(),
        lanes: lanes
            .iter()
            .map(|name| LaneName::new(*name).unwrap())
            .collect(),
        autonomy,
    })
}

fn profile(hardware: &str, agents: &[(&str, u32)], max_sessions: u32) -> EventBody {
    EventBody::DeviceProfile(DeviceProfile {
        name: "laptop".into(),
        hardware: hardware.into(),
        agents: agents
            .iter()
            .map(|(name, sessions)| (AgentName::new(*name).unwrap(), *sessions))
            .collect::<BTreeMap<_, _>>(),
        max_sessions,
    })
}

fn task_requiring(title: &str, requires: &[&str]) -> EventBody {
    let EventBody::ItemCreated(created) = create("task", title) else {
        unreachable!()
    };
    EventBody::ItemCreated(ItemCreated {
        requires: requires
            .iter()
            .map(|name| Requirement::new(*name).unwrap())
            .collect(),
        ..created
    })
}

struct Scene {
    fixture: Fixture,
    state: State,
    clock: u64,
    coder: AgentId,
}

impl Scene {
    fn new() -> Self {
        let fixture = Fixture::new(1);
        let state = fixture.state();
        let coder = AgentId {
            principal: fixture.founder.key(),
            name: AgentName::new("coder").unwrap(),
        };
        Self {
            fixture,
            state,
            clock: 2_000,
            coder,
        }
    }

    fn apply(&mut self, body: EventBody) -> crate::event::Event {
        self.clock += 1;
        self.fixture.apply(&mut self.state, 0, self.clock, body)
    }

    fn device(&self) -> crate::identity::DeviceKey {
        self.fixture.devices[0].key()
    }

    fn offered(&self) -> Vec<String> {
        self.state
            .dispatchable(self.device(), &self.coder)
            .iter()
            .map(|item| item.title().to_string())
            .collect()
    }
}

#[test]
fn an_agent_is_offered_the_ready_items_of_its_lanes_most_urgent_first() {
    let mut scene = Scene::new();
    scene.apply(agent("coder", &["default"], &[], StartMode::Auto));
    scene.apply(create("task", "first"));
    scene.apply(create("task", "second"));
    let mut urgent = create("task", "urgent");
    if let EventBody::ItemCreated(created) = &mut urgent {
        created.priority = Some(crate::item::Priority::P0);
    }
    scene.apply(urgent);
    scene.apply(create("distill", "in the meta lane"));
    assert_eq!(scene.offered(), ["urgent", "first", "second"]);
}

#[test]
fn requirements_are_met_by_capabilities_or_hardware() {
    let mut scene = Scene::new();
    scene.apply(agent("coder", &["default"], &["rust"], StartMode::Auto));
    scene.apply(profile("macos aarch64, 8 cpus", &[("coder", 1)], 1));
    scene.apply(task_requiring("needs rust", &["rust"]));
    scene.apply(task_requiring("needs a gpu", &["gpu"]));
    scene.apply(task_requiring("needs macos", &["macos"]));
    scene.apply(task_requiring("needs both", &["rust", "macos"]));
    scene.apply(task_requiring("needs nothing", &[]));
    let mut offered = scene.offered();
    offered.sort();
    assert_eq!(
        offered,
        ["needs both", "needs macos", "needs nothing", "needs rust"]
    );

    scene.apply(profile("linux x86_64, gpu", &[("coder", 1)], 1));
    let mut offered = scene.offered();
    offered.sort();
    assert_eq!(offered, ["needs a gpu", "needs nothing", "needs rust"]);
}

#[test]
fn an_item_a_session_holds_is_not_offered() {
    let mut scene = Scene::new();
    scene.apply(agent("coder", &["default"], &[], StartMode::Auto));
    let task = scene.apply(create("task", "work"));
    scene.apply(start_session("coder", task.id().into()));
    assert!(scene.offered().is_empty());
}

#[test]
fn sessions_that_do_not_finish_are_retried_as_the_recovery_level_says() {
    let mut scene = Scene::new();
    scene.apply(agent("coder", &["default"], &[], StartMode::Auto));
    let twice: ItemId = scene.apply(create("task", "released twice")).id().into();
    let thrice: ItemId = scene.apply(create("task", "fails three times")).id().into();
    let interrupted: ItemId = scene.apply(create("task", "interrupted")).id().into();
    let fragile: ItemId = scene.apply(create("task", "fragile")).id().into();
    scene.apply(edit(
        fragile,
        crate::event::Change::RecoveryLevel(Some("none".parse().unwrap())),
    ));
    scene.apply(create("task", "untouched"));

    let end = |scene: &mut Scene, item: ItemId, reason: EndReason| {
        let session = scene.apply(start_session("coder", item)).id().into();
        scene.apply(end_session(session, reason));
    };
    end(&mut scene, twice, EndReason::Released);
    end(&mut scene, twice, EndReason::TimedOut);
    for _ in 0..3 {
        end(&mut scene, thrice, EndReason::Failed);
        end(&mut scene, interrupted, EndReason::Interrupted);
    }
    end(&mut scene, fragile, EndReason::Failed);

    let mut offered = scene.offered();
    offered.sort();
    assert_eq!(offered, ["interrupted", "released twice", "untouched"]);
    assert_eq!(
        scene.state.recovery(twice, &SessionRole::Work),
        Recovery::Retry { failures: 2 }
    );
    assert!(matches!(
        scene.state.recovery(thrice, &SessionRole::Work),
        Recovery::WaitsForPrincipal { failures: 3, .. }
    ));

    // Anything the principal does to the item starts its level again.
    scene.apply(comment(
        thrice,
        "the tests need a database; use the docker one",
    ));
    assert_eq!(
        scene.state.recovery(thrice, &SessionRole::Work),
        Recovery::Fresh
    );
    assert!(scene.offered().contains(&"fails three times".to_string()));
}

#[test]
fn sessions_the_machine_ended_are_not_attempts() {
    let mut scene = Scene::new();
    scene.apply(agent("coder", &["default"], &[], StartMode::Auto));
    let task: ItemId = scene.apply(create("task", "stranded")).id().into();
    scene.apply(edit(
        task,
        crate::event::Change::RecoveryLevel(Some("none".parse().unwrap())),
    ));
    for fault in [
        MachineFault::LoginLapsed,
        MachineFault::NoSpace,
        MachineFault::HarnessUnreachable,
    ] {
        let session = scene.apply(start_session("coder", task)).id().into();
        scene.apply(end_session(session, EndReason::MachineFault(fault)));
        assert_eq!(
            scene.state.recovery(task, &SessionRole::Work),
            Recovery::Fresh,
            "{fault:?}"
        );
    }
    assert_eq!(scene.offered(), ["stranded"]);
}

#[test]
fn sessions_that_lost_their_claim_are_not_attempts() {
    let mut scene = Scene::new();
    scene.apply(agent("coder", &["default"], &[], StartMode::Auto));
    let task: ItemId = scene.apply(create("task", "contested")).id().into();
    scene.apply(edit(
        task,
        crate::event::Change::RecoveryLevel(Some("none".parse().unwrap())),
    ));
    for reason in [EndReason::LeaseExpired, EndReason::Outclaimed] {
        let session = scene.apply(start_session("coder", task)).id().into();
        scene.apply(end_session(session, reason));
        assert_eq!(
            scene.state.recovery(task, &SessionRole::Work),
            Recovery::Fresh,
            "{reason:?}"
        );
    }
    assert_eq!(scene.offered(), ["contested"]);
}

#[test]
fn a_slot_is_free_while_neither_the_device_nor_the_agent_is_full() {
    let mut scene = Scene::new();
    scene.apply(agent("coder", &["default"], &[], StartMode::Auto));
    let device = scene.device();
    let coder = scene.coder.clone();
    assert!(
        !scene.state.has_free_slot(device, &coder),
        "no profile yet, so no agents are hosted"
    );
    scene.apply(profile("", &[("coder", 2)], 2));
    assert!(scene.state.has_free_slot(device, &coder));
    let first: ItemId = scene.apply(create("task", "one")).id().into();
    let second: ItemId = scene.apply(create("task", "two")).id().into();
    scene.apply(start_session("coder", first));
    assert!(scene.state.has_free_slot(device, &coder));
    scene.apply(start_session("coder", second));
    assert!(!scene.state.has_free_slot(device, &coder));

    scene.apply(profile("", &[("coder", 5)], 2));
    assert!(
        !scene.state.has_free_slot(device, &coder),
        "the device is full"
    );
}

#[test]
fn only_hosted_auto_agents_are_started_by_the_dispatcher() {
    let mut scene = Scene::new();
    scene.apply(agent("coder", &["default"], &[], StartMode::Auto));
    scene.apply(agent("manual", &["default"], &[], StartMode::Manual));
    scene.apply(agent("elsewhere", &["default"], &[], StartMode::Auto));
    scene.apply(profile("", &[("coder", 1), ("manual", 1)], 2));
    let hosted: Vec<String> = scene
        .state
        .auto_agents(scene.device())
        .iter()
        .map(|(id, _)| id.name.to_string())
        .collect();
    assert_eq!(hosted, ["coder"]);
}

#[test]
fn a_diagnosis_comes_between_a_failure_and_the_next_attempt() {
    let mut scene = Scene::new();
    scene.apply(agent("coder", &["default"], &[], StartMode::Auto));
    scene.apply(agent(
        "doctor",
        &["supervision"],
        &["review"],
        StartMode::Auto,
    ));
    let doctor = AgentId {
        principal: scene.fixture.founder.key(),
        name: AgentName::new("doctor").unwrap(),
    };
    let item: ItemId = scene.apply(create("task", "flaky")).id().into();
    scene.apply(edit(
        item,
        crate::event::Change::RecoveryLevel(Some("diagnose".parse().unwrap())),
    ));
    let work: crate::id::SessionId = scene.apply(start_session("coder", item)).id().into();
    scene.apply(end_session(work, EndReason::Failed));

    assert!(matches!(
        scene.state.recovery(item, &SessionRole::Work),
        Recovery::Diagnose { failures: 1, .. }
    ));
    assert!(scene.offered().is_empty(), "no work before the diagnosis");
    assert_eq!(scene.state.diagnosable(scene.device(), &doctor), [item]);
    assert!(
        scene
            .state
            .diagnosable(scene.device(), &scene.coder)
            .is_empty(),
        "the coder lacks the capability"
    );

    let diagnosis: crate::id::SessionId = scene
        .apply(start_session_as("doctor", item, SessionRole::Diagnose))
        .id()
        .into();
    assert!(
        scene.state.diagnosable(scene.device(), &doctor).is_empty(),
        "one diagnosis at a time"
    );
    scene.apply(end_session(diagnosis, EndReason::Diagnosed));
    assert_eq!(
        scene.state.recovery(item, &SessionRole::Work),
        Recovery::Retry { failures: 1 }
    );
    assert_eq!(scene.offered(), ["flaky"]);

    let again: crate::id::SessionId = scene.apply(start_session("coder", item)).id().into();
    scene.apply(end_session(again, EndReason::Released));
    assert!(matches!(
        scene.state.recovery(item, &SessionRole::Work),
        Recovery::WaitsForPrincipal { failures: 2, .. }
    ));
}

fn with_priority(body: EventBody, priority: crate::item::Priority) -> EventBody {
    let EventBody::ItemCreated(created) = body else {
        unreachable!()
    };
    EventBody::ItemCreated(ItemCreated {
        priority: Some(priority),
        ..created
    })
}

impl Scene {
    /// Works on the item in a session that ends as `reason`.
    fn work(&mut self, item: ItemId, reason: EndReason) {
        let session = self.apply(start_session("coder", item)).id().into();
        self.apply(end_session(session, reason));
    }

    /// Submits the item and has its review send it back with an open change request.
    fn send_back(&mut self, item: ItemId) {
        self.send_back_with(item, change_request(item, "add a test"));
    }

    fn send_back_with(&mut self, item: ItemId, open_comment: EventBody) {
        self.work(item, EndReason::Submitted);
        self.apply(open_comment);
        self.apply(verdict(item, None, Verdict::RequestChanges));
    }
}

#[test]
fn work_sent_back_by_review_goes_before_older_untouched_work() {
    let mut scene = Scene::new();
    scene.apply(agent("coder", &["default"], &[], StartMode::Auto));
    scene.apply(create_reviewed("task", "untouched", "standard"));
    let sent_back: ItemId = scene
        .apply(create_reviewed("task", "sent back", "standard"))
        .id()
        .into();
    scene.send_back(sent_back);
    assert_eq!(scene.offered(), ["sent back", "untouched"]);
}

#[test]
fn started_work_goes_before_older_untouched_work_and_after_work_sent_back() {
    let mut scene = Scene::new();
    scene.apply(agent("coder", &["default"], &[], StartMode::Auto));
    scene.apply(create_reviewed("task", "untouched", "standard"));
    let started: ItemId = scene
        .apply(create_reviewed("task", "released", "standard"))
        .id()
        .into();
    let sent_back: ItemId = scene
        .apply(create_reviewed("task", "sent back", "standard"))
        .id()
        .into();
    scene.work(started, EndReason::Released);
    scene.send_back(sent_back);
    assert_eq!(scene.offered(), ["sent back", "released", "untouched"]);
}

#[test]
fn within_a_group_fewer_unchecked_steps_go_first_then_more_checked_ones() {
    let mut scene = Scene::new();
    scene.apply(agent("coder", &["default"], &[], StartMode::Auto));
    scene.apply(create_with_body(
        "task",
        "three to do",
        "- [ ] a\n- [ ] b\n- [ ] c\n",
    ));
    scene.apply(create_with_body("task", "one to do", "- [ ] a\n"));
    let barely: ItemId = scene
        .apply(create_with_body("task", "started, one to do", "- [ ] a\n"))
        .id()
        .into();
    let mostly: ItemId = scene
        .apply(create_with_body(
            "task",
            "started, one to do of three",
            "- [x] a\n- [x] b\n- [ ] c\n",
        ))
        .id()
        .into();
    let long: ItemId = scene
        .apply(create_with_body(
            "task",
            "started, two to do",
            "- [ ] a\n- [ ] b\n",
        ))
        .id()
        .into();
    for item in [barely, mostly, long] {
        scene.work(item, EndReason::Released);
    }
    assert_eq!(
        scene.offered(),
        [
            "started, one to do of three",
            "started, one to do",
            "started, two to do",
            "one to do",
            "three to do",
        ]
    );
}

#[test]
fn priority_goes_before_how_far_the_work_got() {
    let mut scene = Scene::new();
    scene.apply(agent("coder", &["default"], &[], StartMode::Auto));
    let sent_back: ItemId = scene
        .apply(with_priority(
            create_reviewed("task", "sent back", "standard"),
            crate::item::Priority::P3,
        ))
        .id()
        .into();
    scene.send_back(sent_back);
    scene.apply(with_priority(
        create("task", "urgent"),
        crate::item::Priority::P0,
    ));
    assert_eq!(scene.offered(), ["urgent", "sent back"]);
}

#[test]
fn work_sent_back_with_an_open_check_failure_goes_before_older_untouched_work() {
    let mut scene = Scene::new();
    scene.apply(agent("coder", &["default"], &[], StartMode::Auto));
    scene.apply(create_reviewed("task", "untouched", "standard"));
    let sent_back: ItemId = scene
        .apply(create_reviewed("task", "sent back", "standard"))
        .id()
        .into();
    scene.send_back_with(sent_back, check_failure(sent_back, "cargo test"));
    assert_eq!(scene.offered(), ["sent back", "untouched"]);
}

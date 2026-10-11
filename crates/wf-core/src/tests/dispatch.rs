use std::collections::BTreeMap;

use crate::autonomy::{Autonomy, StartMode};
use crate::event::{AgentDefined, DeviceProfile, EventBody, ItemCreated};
use crate::id::ItemId;
use crate::identity::AgentId;
use crate::name::{AgentName, HarnessName, LaneName, Requirement};
use crate::session::EndReason;
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
fn an_agent_does_not_retry_what_it_gave_up_on() {
    let mut scene = Scene::new();
    scene.apply(agent("coder", &["default"], &[], StartMode::Auto));
    let task: ItemId = scene.apply(create("task", "released")).id().into();
    let timed_out: ItemId = scene.apply(create("task", "too slow")).id().into();
    let crashing: ItemId = scene.apply(create("task", "crashes")).id().into();
    let submitted: ItemId = scene.apply(create("task", "changes requested")).id().into();
    scene.apply(create("task", "untouched"));

    let end = |scene: &mut Scene, item: ItemId, reason: EndReason| {
        let session = scene.apply(start_session("coder", item)).id().into();
        scene.apply(end_session(session, reason));
    };
    end(&mut scene, task, EndReason::Released);
    end(&mut scene, timed_out, EndReason::TimedOut);
    end(&mut scene, submitted, EndReason::Cancelled);
    for _ in 0..2 {
        end(&mut scene, crashing, EndReason::Failed);
    }
    assert_eq!(
        scene.offered().len(),
        3,
        "crashing twice is still tried again: {:?}",
        scene.offered()
    );
    end(&mut scene, crashing, EndReason::Failed);
    let mut offered = scene.offered();
    offered.sort();
    assert_eq!(offered, ["changes requested", "untouched"]);
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

use std::collections::BTreeMap;

use crate::autonomy::{Autonomy, StartMode};
use crate::claims::Presence;
use crate::event::{AgentDefined, DeviceProfile, EventBody, ItemCreated};
use crate::id::{EventId, ItemId, SessionId};
use crate::identity::DeviceKey;
use crate::item::ItemState;
use crate::name::{AgentName, HarnessName, LaneName, Requirement};
use crate::placement::{Placement, RANK_DELAY, Turn, rendezvous};
use crate::session::{CommitId, EndReason, SessionRole};
use crate::state::State;
use crate::testing::*;

fn auto_agent(name: &str, lanes: &[&str], capabilities: &[&str]) -> EventBody {
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
        autonomy: Autonomy {
            start: StartMode::Auto,
            ..Autonomy::default()
        },
    })
}

fn profile(hardware: &str, max_sessions: u32) -> EventBody {
    EventBody::DeviceProfile(DeviceProfile {
        name: "laptop".into(),
        hardware: hardware.into(),
        agents: ["coder", "reviewer", "boss"]
            .into_iter()
            .map(|name| (AgentName::new(name).unwrap(), 1))
            .collect::<BTreeMap<_, _>>(),
        max_sessions,
    })
}

fn commit(byte: u8) -> CommitId {
    let text: String = std::iter::repeat_n(format!("{byte:02x}"), 20).collect();
    CommitId::Git(text.parse().unwrap())
}

/// Three devices of one principal, each hosting `coder`, `reviewer` and the supervisor `boss`
/// with a session each.
struct Scene {
    fixture: Fixture,
    state: State,
    clock: u64,
}

impl Scene {
    fn new() -> Self {
        let fixture = Fixture::new(3);
        let state = fixture.state();
        let mut scene = Self {
            fixture,
            state,
            clock: 2_000,
        };
        scene.apply(0, auto_agent("coder", &["default"], &[]));
        scene.apply(0, auto_agent("reviewer", &[], &["review"]));
        scene.apply(0, auto_agent("boss", &["supervision"], &[]));
        for device in 0..3 {
            scene.apply(device, profile("linux x86_64", 2));
        }
        scene
    }

    fn apply(&mut self, device: usize, body: EventBody) -> crate::event::Event {
        self.clock += 1;
        self.fixture
            .apply(&mut self.state, device, self.clock, body)
    }

    fn device(&self, index: usize) -> DeviceKey {
        self.fixture.devices[index].key()
    }

    /// The placement as the daemon on device `here` sees it, with every device present.
    fn placement(&self, here: usize) -> Placement {
        let presence = Presence::new(self.clock, []);
        self.state
            .placement(self.device(here), &presence, self.clock)
    }

    /// The devices that could take `item` in `role`, best first, after checking that every
    /// device ranks them alike.
    fn ranked(&self, item: ItemId, role: &SessionRole) -> Vec<DeviceKey> {
        let ranked = self.placement(0).ranked(item, role).to_vec();
        for here in 1..3 {
            assert_eq!(
                self.placement(here).ranked(item, role),
                ranked,
                "device {here}"
            );
        }
        ranked
    }

    /// A work session of `coder` on device `device` that is interrupted, which costs no attempt.
    fn interrupted_on(&mut self, device: usize, item: ItemId) {
        let session: SessionId = self.apply(device, start_session("coder", item)).id().into();
        self.apply(device, end_session(session, EndReason::Interrupted));
    }
}

#[test]
fn new_work_is_ranked_by_hash_alike_on_every_device() {
    let mut scene = Scene::new();
    let task: ItemId = scene.apply(0, create("task", "new")).id().into();
    let ranked = scene.ranked(task, &SessionRole::Work);
    let mut by_hash: Vec<DeviceKey> = (0..3).map(|index| scene.device(index)).collect();
    by_hash.sort_by_key(|device| std::cmp::Reverse(rendezvous(task, &SessionRole::Work, *device)));
    assert_eq!(ranked, by_hash);
    let placement = scene.placement(1);
    for (rank, device) in ranked.iter().enumerate() {
        assert_eq!(placement.rank(task, &SessionRole::Work, *device), rank);
    }
}

#[test]
fn work_goes_first_to_the_device_of_the_latest_work_session() {
    let mut scene = Scene::new();
    let task: ItemId = scene.apply(0, create("task", "started")).id().into();
    let by_hash = scene.ranked(task, &SessionRole::Work);
    // Each device in turn is where the work last was, whatever the hash says.
    for home in [by_hash[2], by_hash[1], by_hash[0]] {
        let index = (0..3).find(|index| scene.device(*index) == home).unwrap();
        scene.interrupted_on(index, task);
        let ranked = scene.ranked(task, &SessionRole::Work);
        assert_eq!(ranked[0], home);
        let rest: Vec<_> = by_hash
            .iter()
            .copied()
            .filter(|device| *device != home)
            .collect();
        assert_eq!(ranked[1..], rest, "the rest keep the order of the hash");
    }
}

#[test]
fn work_does_not_go_to_the_device_that_lost_the_race_for_it() {
    let mut scene = Scene::new();
    let task: ItemId = scene.apply(0, create("task", "raced")).id().into();
    scene.apply(0, start_session("coder", task));
    let lost: SessionId = scene.apply(1, start_session("coder", task)).id().into();
    scene.apply(1, end_session(lost, EndReason::Outclaimed));
    assert_eq!(
        scene.state.home(task, &SessionRole::Work),
        Some(scene.device(0))
    );
}

#[test]
fn a_review_goes_first_to_the_device_that_submitted_the_work() {
    for submitter in 0..3 {
        let mut scene = Scene::new();
        let task: ItemId = scene
            .apply(0, create_reviewed("task", "reviewed", "light"))
            .id()
            .into();
        // Worked on elsewhere before, which says nothing about the review.
        scene.interrupted_on((submitter + 1) % 3, task);
        let session: SessionId = scene
            .apply(submitter, start_session("coder", task))
            .id()
            .into();
        scene.apply(submitter, moved(task, ItemState::Review));
        scene.apply(submitter, submit_session(session, commit(1)));
        let role = SessionRole::Review {
            commit: Some(commit(1)),
            step: None,
        };
        let ranked = scene.ranked(task, &role);
        assert_eq!(ranked.len(), 3);
        assert_eq!(ranked[0], scene.device(submitter));
    }
}

#[test]
fn supervision_goes_first_to_the_device_of_the_session_that_waits() {
    for waiting in 0..3 {
        let mut scene = Scene::new();
        let task: ItemId = scene.apply(0, create("task", "blocked")).id().into();
        let session: SessionId = scene
            .apply(waiting, start_session("coder", task))
            .id()
            .into();
        let actor = scene.fixture.agent_actor("coder", session);
        let asked = scene.fixture.author_as(
            &scene.state,
            waiting,
            actor,
            scene.clock + 1,
            blocker(task, "which crate?"),
        );
        scene
            .state
            .receive(asked, crate::state::Trust::Untrusted)
            .unwrap();
        scene.clock += 1;
        let ranked = scene.ranked(task, &SessionRole::Supervise);
        assert_eq!(ranked.len(), 3);
        assert_eq!(ranked[0], scene.device(waiting));
    }
}

#[test]
fn a_diagnosis_has_no_home_and_goes_by_hash() {
    let mut scene = Scene::new();
    let task: ItemId = scene.apply(0, create("task", "fragile")).id().into();
    scene.interrupted_on(1, task);
    assert_eq!(scene.state.home(task, &SessionRole::Diagnose), None);
    assert_eq!(
        scene.state.home(task, &SessionRole::Work),
        Some(scene.device(1))
    );
}

#[test]
fn left_out_are_devices_without_a_free_slot_or_what_is_required_or_a_lease() {
    let mut scene = Scene::new();
    scene.apply(1, profile("linux x86_64, gpu", 2));
    scene.apply(2, profile("linux x86_64, gpu", 1));
    let EventBody::ItemCreated(created) = create("task", "needs a gpu") else {
        unreachable!()
    };
    let needs_gpu: ItemId = scene
        .apply(
            0,
            EventBody::ItemCreated(ItemCreated {
                requires: vec![Requirement::new("gpu").unwrap()],
                ..created
            }),
        )
        .id()
        .into();
    assert_eq!(
        scene.ranked(needs_gpu, &SessionRole::Work).len(),
        2,
        "only devices with a gpu"
    );

    // Device 2 runs one session at most, and now runs one.
    let busy: ItemId = scene.apply(0, create("task", "other")).id().into();
    scene.apply(2, start_session("coder", busy));
    assert_eq!(
        scene.ranked(needs_gpu, &SessionRole::Work),
        [scene.device(1)]
    );

    // Device 1 has not been heard from for longer than the lease: device 0 leaves it out, and
    // device 1 never leaves itself out.
    let lease_ms = u64::try_from(scene.state.lease().as_millis()).unwrap();
    let now_ms = scene.clock + lease_ms + 1;
    let presence = Presence::new(scene.clock, [(scene.device(0), now_ms)]);
    let task: ItemId = scene.apply(0, create("task", "anywhere")).id().into();
    let from_device_0 = scene.state.placement(scene.device(0), &presence, now_ms);
    assert!(
        from_device_0
            .ranked(needs_gpu, &SessionRole::Work)
            .is_empty()
    );
    assert_eq!(
        from_device_0.ranked(task, &SessionRole::Work),
        [scene.device(0)]
    );
    let from_device_1 = scene.state.placement(scene.device(1), &presence, now_ms);
    assert_eq!(
        from_device_1.ranked(needs_gpu, &SessionRole::Work),
        [scene.device(1)]
    );
    assert_eq!(
        from_device_0.rank(needs_gpu, &SessionRole::Work, scene.device(0)),
        0,
        "a device that could not take it comes after all that could"
    );
}

#[test]
fn a_device_takes_its_turn_only_once_it_heard_from_every_device_ranked_ahead() {
    let mut scene = Scene::new();
    let task: ItemId = scene.apply(0, create("task", "new")).id().into();
    let ranked = scene.ranked(task, &SessionRole::Work);
    let placement = scene.placement(0);
    let turn = |heard: &[DeviceKey], device: DeviceKey| {
        let presence = Presence::new(scene.clock, heard.iter().map(|key| (*key, scene.clock)));
        placement.turn(task, &SessionRole::Work, device, &presence)
    };

    // The first has no one ahead to hear from.
    assert_eq!(turn(&[], ranked[0]), Turn::Rank(0));
    // Before it heard from a device ahead, a claim of that device could not have reached it.
    assert_eq!(turn(&[], ranked[2]), Turn::Unheard(ranked[0]));
    assert_eq!(turn(&[ranked[0]], ranked[2]), Turn::Unheard(ranked[1]));
    assert_eq!(turn(&[ranked[0], ranked[1]], ranked[2]), Turn::Rank(2));
    // Who comes after it does not matter.
    assert_eq!(turn(&[ranked[0]], ranked[1]), Turn::Rank(1));
}

#[test]
fn one_device_is_always_first() {
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    fixture.apply(&mut state, 0, 2_000, auto_agent("coder", &["default"], &[]));
    fixture.apply(&mut state, 0, 2_001, profile("linux", 1));
    let task: ItemId = fixture
        .apply(&mut state, 0, 2_002, create("task", "alone"))
        .id()
        .into();
    let device = fixture.devices[0].key();
    let placement = state.placement(device, &Presence::new(2_002, []), 2_002);
    assert_eq!(placement.rank(task, &SessionRole::Work, device), 0);
    assert_eq!(Placement::delay(0), std::time::Duration::ZERO);
    assert_eq!(Placement::delay(2), RANK_DELAY * 2);
}

#[test]
fn the_hash_spreads_things_evenly_and_a_device_that_leaves_takes_only_its_own() {
    let fixture = Fixture::new(4);
    let devices: Vec<DeviceKey> = fixture.devices.iter().map(|device| device.key()).collect();
    let items: Vec<ItemId> = (0u32..4_000)
        .map(|index| EventId::of(&index.to_le_bytes()).into())
        .collect();
    let first = |item: ItemId, devices: &[DeviceKey]| {
        *devices
            .iter()
            .max_by_key(|device| rendezvous(item, &SessionRole::Work, **device))
            .unwrap()
    };
    let mut taken: BTreeMap<DeviceKey, usize> = BTreeMap::new();
    for item in &items {
        *taken.entry(first(*item, &devices)).or_default() += 1;
    }
    // 1 000 each is expected; the standard deviation is about 27.
    for (device, count) in &taken {
        assert!((880..=1_120).contains(count), "{device:?} takes {count}");
    }
    assert_eq!(taken.len(), 4);

    let without_last = &devices[..3];
    for item in &items {
        let before = first(*item, &devices);
        if before != devices[3] {
            assert_eq!(first(*item, without_last), before);
        }
    }

    // A review step is a thing of its own.
    let review = SessionRole::Review {
        commit: Some(commit(1)),
        step: None,
    };
    let differs = items
        .iter()
        .filter(|item| {
            rendezvous(**item, &review, devices[0])
                != rendezvous(**item, &SessionRole::Work, devices[0])
        })
        .count();
    assert_eq!(differs, items.len());
}

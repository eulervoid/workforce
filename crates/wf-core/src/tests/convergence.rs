//! The core claim of the design: any permutation of an event set, with any duplication, folds to
//! the same state.

use std::collections::BTreeSet;

use proptest::prelude::*;

use crate::crdt::SetChange;
use crate::event::{Change, Event, EventBody};
use crate::id::{EventId, ItemId};
use crate::item::{ItemView, Priority};
use crate::name::{Label, Requirement};
use crate::session::EndReason;
use crate::state::{Disposition, State, Trust};
use crate::testing::*;

const DEVICES: usize = 3;

const CUSTOM_POLICY: &str = concat!(
    include_str!("../default_policy.toml"),
    "\n[types.bug]\nlane = \"default\"\nfields = [\"labels\"]\ndepends_on = [\"task\"]\ninitial = \"new\"\n\
     states = [{ name = \"new\", category = \"open\" }, { name = \"fixed\", category = \"done\" }]\n"
);

struct Simulation {
    fixture: Fixture,
    replicas: Vec<State>,
    authored: Vec<Event>,
    clock: u64,
}

impl Simulation {
    fn new() -> Self {
        let fixture = Fixture::new(DEVICES);
        let replicas = (0..DEVICES).map(|_| fixture.state()).collect();
        Self {
            authored: vec![fixture.genesis.clone()],
            fixture,
            replicas,
            clock: 2_000,
        }
    }

    fn pick_item(&self, device: usize, choice: u8) -> Option<ItemId> {
        let ids: Vec<ItemId> = self.replicas[device]
            .items()
            .map(|view| view.id())
            .collect();
        (!ids.is_empty()).then(|| ids[usize::from(choice) % ids.len()])
    }

    fn submit(&mut self, device: usize, body: EventBody) {
        if self.replicas[device].check_policy(&body).is_err() {
            return;
        }
        // Wall clocks of different devices drift: sometimes the clock does not advance at all.
        self.clock += 1;
        let event = self
            .fixture
            .author(&self.replicas[device], device, self.clock / 2, body);
        self.replicas[device]
            .receive(event.clone(), Trust::Stored)
            .expect("an event authored on a valid view is valid");
        self.authored.push(event);
    }

    fn sync(&mut self, from: usize, to: usize) {
        if from == to {
            return;
        }
        let missing: Vec<Event> = self
            .authored
            .iter()
            .filter(|event| self.replicas[from].contains(&event.id()))
            .cloned()
            .collect();
        for event in missing {
            self.replicas[to].receive(event, Trust::Stored).unwrap();
        }
    }

    fn step(&mut self, (device, operation, first, second): (u8, u8, u8, u8)) {
        let device = usize::from(device) % DEVICES;
        let item = self.pick_item(device, first);
        let other = self.pick_item(device, second);
        let types = ["task", "task", "distill", "decision", "bug"];
        let states = [
            "backlog",
            "open",
            "in_progress",
            "in_review",
            "done",
            "dropped",
            "answered",
            "proposed",
            "ratified",
            "new",
            "fixed",
        ];
        let label = Label::new(format!("l{}", second % 3)).unwrap();
        match (operation % 15, item) {
            (0 | 1, _) => {
                let kind = types[usize::from(first) % types.len()];
                let body = match other {
                    Some(target) => format!("see #{}", crate::id::HashId::short(&target)),
                    None => String::new(),
                };
                self.submit(
                    device,
                    create_with_body(kind, &format!("item {first}"), &body),
                );
            }
            (2, Some(item)) => {
                self.submit(device, edit(item, Change::Title(format!("title {second}"))))
            }
            (3, Some(item)) => self.submit(
                device,
                edit(
                    item,
                    Change::Body(format!(
                        "body {second} @coder\n\n- [ ] step {}\n- [x] step {}",
                        second % 2,
                        2 + second % 2
                    )),
                ),
            ),
            (4, Some(item)) => {
                let priority = Some([Priority::P0, Priority::P2][usize::from(second) % 2]);
                self.submit(device, edit(item, Change::Priority(priority)));
            }
            (5, Some(item)) => {
                self.submit(device, edit(item, Change::Labels(SetChange::Add(label))))
            }
            (6, Some(item)) => {
                let removal = match self.replicas[device].item(item) {
                    Some(ItemView::Live(live)) => live.labels().removal(&label),
                    _ => return,
                };
                self.submit(device, edit(item, Change::Labels(removal)));
            }
            (7, Some(item)) => {
                let requirement = Requirement::new(format!("r{}", second % 2)).unwrap();
                self.submit(
                    device,
                    edit(item, Change::Requires(SetChange::Add(requirement))),
                );
            }
            (8 | 9, Some(item)) => {
                let policy = &self.replicas[device].policy().unwrap().policy;
                let own_states: Vec<String> = policy
                    .type_config(self.replicas[device].item(item).unwrap().item_type())
                    .map(|config| {
                        config
                            .states
                            .iter()
                            .map(|state| state.name.to_string())
                            .collect()
                    })
                    .unwrap_or_default();
                let state = if own_states.is_empty() || second % 11 == 0 {
                    states[usize::from(second) % states.len()].to_string()
                } else {
                    own_states[usize::from(second) % own_states.len()].clone()
                };
                self.submit(device, moved(item, &state));
            }
            (10, Some(item)) => {
                if let Some(target) = other {
                    self.submit(device, depends_on(item, target));
                }
            }
            (11, Some(item)) if second % 2 == 1 => {
                if let Some(target) = other {
                    let removal = match self.replicas[device].item(item) {
                        Some(ItemView::Live(live)) => live.depends_on().removal(&target),
                        _ => return,
                    };
                    self.submit(
                        device,
                        EventBody::ItemLinked(crate::event::ItemLinked {
                            item,
                            link: crate::event::Link::DependsOn(removal),
                        }),
                    );
                }
            }
            (11, _) => {
                // Link the reverse of an edge another device made: a concurrent cycle once merged.
                let other_device = (device + 1 + usize::from(first) % (DEVICES - 1)) % DEVICES;
                let reverse = self.replicas[other_device]
                    .live_items()
                    .flat_map(|item| {
                        item.depends_on()
                            .values()
                            .map(|target| (item.id(), *target))
                            .collect::<Vec<_>>()
                    })
                    .find(|(from, to)| {
                        self.replicas[device].item(*from).is_some()
                            && self.replicas[device].item(*to).is_some()
                            && !self.replicas[device].dependency_graph().reaches(*from, *to)
                    });
                if let Some((from, to)) = reverse {
                    self.submit(device, depends_on(to, from));
                }
            }
            (12, Some(_)) => {
                let policy = &self.replicas[device].policy().unwrap().policy;
                let eligible: Vec<ItemId> = self.replicas[device]
                    .items()
                    .filter(|view| match view {
                        ItemView::Live(item) if second % 2 == 0 => item
                            .standing(policy)
                            .state_category()
                            .is_some_and(|state_category| state_category.is_terminal()),
                        ItemView::Archived(_) => second % 2 == 1,
                        ItemView::Live(_) => false,
                    })
                    .map(|view| view.id())
                    .collect();
                if let Some(item) = eligible.get(usize::from(first) % eligible.len().max(1)) {
                    let body = if second % 2 == 0 {
                        archive(*item)
                    } else {
                        unarchive(*item)
                    };
                    self.submit(device, body);
                }
            }
            (13, Some(item)) if second % 2 == 0 => {
                let steps: Vec<_> = match self.replicas[device].item(item) {
                    Some(ItemView::Live(live)) => live.body().steps().map(|step| step.id).collect(),
                    _ => return,
                };
                if let Some(step) = steps.get(usize::from(first) % steps.len().max(1)) {
                    let change = Change::StepDone {
                        step: *step,
                        done: second % 4 == 0,
                    };
                    self.submit(device, edit(item, change));
                }
            }
            (13, Some(item)) => self.submit(device, comment(item, &format!("note {second} @ada"))),
            (14, Some(item)) if second % 4 == 3 => {
                let coder = crate::name::AgentName::new("coder").unwrap();
                if self.replicas[device]
                    .agent(&coder, self.fixture.founder.key())
                    .is_some()
                {
                    self.submit(device, start_session("coder", item));
                }
            }
            (14, _) if second % 4 == 2 => {
                let live: Vec<_> = self.replicas[device]
                    .live_sessions()
                    .map(|session| session.id())
                    .collect();
                if !live.is_empty() {
                    let session = live[usize::from(first) % live.len()];
                    let reason = [EndReason::Released, EndReason::Failed, EndReason::Cancelled]
                        [usize::from(first) % 3];
                    self.submit(device, end_session(session, reason));
                }
            }
            (14, _) if second % 4 == 1 => {
                let items: Vec<_> = self.replicas[device]
                    .sessions()
                    .map(|session| session.item())
                    .collect();
                if !items.is_empty() {
                    let item = items[usize::from(first) % items.len()];
                    self.submit(device, comment(item, &format!("report {second}")));
                }
            }
            (14, _) => match second % 3 {
                0 => self.submit(device, define_agent("coder")),
                1 => {
                    let source = if first % 2 == 0 {
                        CUSTOM_POLICY
                    } else {
                        crate::policy::DEFAULT_POLICY_TOML
                    };
                    let body = self.fixture.policy_set(&self.fixture.founder, source);
                    self.submit(device, body);
                }
                _ => self.sync(usize::from(first) % DEVICES, device),
            },
            _ => {}
        }
        if operation % 16 == 15 {
            self.sync(usize::from(first) % DEVICES, usize::from(second) % DEVICES);
        }
    }
}

/// Deterministic Fisher-Yates driven by xorshift, so the shuffle is reproducible from a seed.
fn shuffled<T: Clone>(items: &[T], mut seed: u64) -> Vec<T> {
    let mut result = items.to_vec();
    seed |= 1;
    for index in (1..result.len()).rev() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        result.swap(index, (seed % (index as u64 + 1)) as usize);
    }
    result
}

fn fold(events: impl IntoIterator<Item = Event>) -> State {
    let mut state = State::new();
    for event in events {
        state
            .receive(event, Trust::Stored)
            .expect("generated events are valid in any order");
    }
    assert_eq!(
        state.pending_events().count(),
        0,
        "nothing may stay pending"
    );
    state
}

fn causal_order(events: &[Event]) -> Vec<Event> {
    let mut sorted = events.to_vec();
    sorted.sort_by_key(|event| (event.content.hlc, event.id()));
    sorted
}

/// `PROPTEST_CASES=10000 cargo test --release` for the exit-criterion run.
fn cases() -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(128)
}

fn operations() -> impl Strategy<Value = Vec<(u8, u8, u8, u8)>> {
    prop::collection::vec((any::<u8>(), any::<u8>(), any::<u8>(), any::<u8>()), 1..60)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]

    #[test]
    fn every_permutation_folds_to_the_same_state(operations in operations(), seeds in prop::array::uniform3(any::<u64>())) {
        let mut simulation = Simulation::new();
        for operation in operations {
            simulation.step(operation);
        }
        let reference = fold(causal_order(&simulation.authored));

        for seed in seeds {
            let permuted = fold(shuffled(&simulation.authored, seed));
            prop_assert_eq!(&permuted, &reference);
        }

        // Duplicate delivery changes nothing.
        let mut doubled = shuffled(&simulation.authored, seeds[0]);
        doubled.extend(shuffled(&simulation.authored, seeds[1]));
        let mut state = State::new();
        for event in doubled {
            let received = state.receive(event, Trust::Stored).unwrap();
            prop_assert!(received.rejected.is_empty());
        }
        prop_assert_eq!(&state, &reference);
        for event in &simulation.authored {
            prop_assert_eq!(
                state.receive(event.clone(), Trust::Stored).unwrap().disposition,
                Disposition::Duplicate
            );
        }

        // Replicas that exchange everything converge on the same state.
        for from in 0..DEVICES {
            for to in 0..DEVICES {
                simulation.sync(from, to);
            }
        }
        for from in 0..DEVICES {
            for to in 0..DEVICES {
                simulation.sync(from, to);
            }
        }
        for replica in &simulation.replicas {
            prop_assert_eq!(replica, &reference);
        }

        // The derived views agree as well.
        let graph = reference.dependency_graph();
        let other = simulation.replicas[1].dependency_graph();
        prop_assert_eq!(graph.ignored(), other.ignored());
        let ids: BTreeSet<EventId> = simulation.authored.iter().map(Event::id).collect();
        prop_assert_eq!(ids.len(), simulation.authored.len());
    }

    #[test]
    fn snapshots_round_trip(operations in operations()) {
        let mut simulation = Simulation::new();
        for operation in operations {
            simulation.step(operation);
        }
        let state = fold(causal_order(&simulation.authored));
        let bytes = postcard::to_stdvec(&state).unwrap();
        let restored: State = postcard::from_bytes(&bytes).unwrap();
        prop_assert_eq!(restored, state);
    }
}

#[test]
fn the_generator_exercises_real_histories() {
    let mut simulation = Simulation::new();
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed >> 24) as u8
    };
    for _ in 0..600 {
        let operation = (next(), next(), next(), next());
        simulation.step(operation);
    }
    for from in 0..DEVICES {
        for to in 0..DEVICES {
            simulation.sync(from, to);
        }
    }
    let state = &simulation.replicas[0];
    let archive_events = simulation
        .authored
        .iter()
        .filter(|event| matches!(event.content.body, EventBody::ItemArchived(_)))
        .count();
    let transitions = simulation
        .authored
        .iter()
        .filter(|event| matches!(event.content.body, EventBody::ItemMoved(_)))
        .count();
    assert!(transitions > 30);
    let archived = state
        .items()
        .filter(|view| matches!(view, ItemView::Archived(_)))
        .count();
    let graph = state.dependency_graph();
    assert!(
        simulation.authored.len() > 250,
        "{} events",
        simulation.authored.len()
    );
    assert!(state.items().count() > 20);
    let unarchive_events = simulation
        .authored
        .iter()
        .filter(|event| matches!(event.content.body, EventBody::ItemUnarchived(_)))
        .count();
    assert!(
        archive_events > 3 && unarchive_events > 0,
        "{archive_events} archives, {unarchive_events} unarchives"
    );
    assert!(
        state
            .live_items()
            .any(|item| item.depends_on().values().count() > 0)
    );
    assert!(
        state
            .live_items()
            .any(|item| item.labels().values().count() > 0)
    );
    assert!(!graph.ignored().is_empty(), "concurrent cycles occur");
    assert!(
        state.sessions().any(|session| session.is_live())
            && state.sessions().any(|session| !session.is_live()),
        "{} sessions",
        state.sessions().count()
    );
    eprintln!(
        "events={} items={} archived={} ignored_edges={}",
        simulation.authored.len(),
        state.items().count(),
        archived,
        graph.ignored().len()
    );
}

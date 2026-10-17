//! The core claim of the design: any permutation of an event set, with any duplication, folds to
//! the same state.

use std::collections::BTreeSet;

use proptest::prelude::*;

use crate::crdt::SetChange;
use crate::event::{Change, CommentKind, Event, EventBody};
use crate::id::{EventId, ItemId};
use crate::identity::{DeviceSigner, InviteSigner};
use crate::item::{ItemState, ItemView, Priority};
use crate::name::{Label, Requirement};
use crate::session::{CommitId, EndReason, Verdict};
use crate::state::{Disposition, State, Trust};
use crate::testing::*;

const DEVICES: usize = 3;

const CUSTOM_POLICY: &str = concat!(
    include_str!("../default_policy.toml"),
    "\n[types.bug]\nlane = \"default\"\nfields = [\"labels\"]\ndepends_on = [\"task\"]\ninitial = \"open\"\n\
     states = [\"open\", \"done\"]\n"
);

/// The default policy, except that tasks never use `open`: a request for changes meets it or
/// not, depending on which policy a replica has folded.
fn policy_without_open_tasks() -> String {
    let default = crate::policy::DEFAULT_POLICY_TOML;
    let with_open = "initial = \"open\"\nstates = [\"draft\", \"open\", \"active\", \"review\", \"done\", \"cancelled\"]";
    assert!(default.contains(with_open));
    default.replace(
        with_open,
        "initial = \"draft\"\nstates = [\"draft\", \"active\", \"review\", \"done\", \"cancelled\"]",
    )
}

fn commit(byte: u8) -> CommitId {
    let text: String = std::iter::repeat_n(format!("{byte:02x}"), 20).collect();
    CommitId::Git(text.parse().unwrap())
}

struct Simulation {
    fixture: Fixture,
    /// Devices that are not bound yet, and the invites that can bind them.
    joiners: Vec<DeviceSigner>,
    invites: Vec<(Event, InviteSigner)>,
    replicas: Vec<State>,
    authored: Vec<Event>,
    clock: u64,
}

impl Simulation {
    fn new() -> Self {
        let fixture = Fixture::new(DEVICES);
        let replicas = (0..DEVICES).map(|_| fixture.state()).collect();
        let joiners = (0..2)
            .map(|index| DeviceSigner::from_bytes(&[200 + index; 32]).unwrap())
            .collect();
        Self {
            authored: vec![fixture.genesis.clone()],
            fixture,
            joiners,
            invites: Vec::new(),
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
        self.submit_event(device, body);
    }

    /// Writes the event on the replica if the policy lets it, and returns its id.
    fn submit_event(&mut self, device: usize, body: EventBody) -> Option<EventId> {
        if self.replicas[device].check_policy(&body).is_err() {
            return None;
        }
        // Wall clocks of different devices drift: sometimes the clock does not advance at all.
        self.clock += 1;
        let event = self
            .fixture
            .author(&self.replicas[device], device, self.clock / 2, body);
        self.replicas[device]
            .receive(event.clone(), Trust::Stored)
            .expect("an event authored on a valid view is valid");
        let id = event.id();
        self.authored.push(event);
        Some(id)
    }

    /// Takes in an event that is valid whatever the policy says, as an invite or a binding.
    fn receive_unchecked(&mut self, device: usize, event: Event) {
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
            "draft",
            "active",
            "review",
            "cancelled",
        ];
        let label = Label::new(format!("l{}", second % 3)).unwrap();
        match (operation % 20, item) {
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
                            .map(|state| state.name().to_string())
                            .collect()
                    })
                    .unwrap_or_default();
                let state = if own_states.is_empty() || second % 11 == 0 {
                    states[usize::from(second) % states.len()].to_string()
                } else {
                    own_states[usize::from(second) % own_states.len()].clone()
                };
                // Old names as well, as older peers write them.
                self.submit(device, moved_as_written(item, &state));
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
                        ItemView::Live(item) if second % 2 == 0 => item.state(policy).is_terminal(),
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
                    let reason = [
                        EndReason::Released,
                        EndReason::Failed,
                        EndReason::Cancelled,
                        EndReason::Submitted {
                            commit: Some(commit(first % 2)),
                        },
                    ][usize::from(first) % 4];
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
                    let source = match first % 3 {
                        0 => CUSTOM_POLICY.to_string(),
                        1 => policy_without_open_tasks(),
                        _ => crate::policy::DEFAULT_POLICY_TOML.to_string(),
                    };
                    let body = self.fixture.policy_set(&self.fixture.founder, &source);
                    self.submit(device, body);
                }
                _ => self.sync(usize::from(first) % DEVICES, device),
            },
            (15, Some(item)) => {
                let body = if second % 2 == 0 {
                    blocker(item, &format!("blocked {second}"))
                } else {
                    change_request(item, &format!("change {second}"))
                };
                self.submit(device, body);
            }
            (16, Some(item)) => {
                let open: Vec<_> = match self.replicas[device].item(item) {
                    Some(ItemView::Live(live)) => {
                        live.open_comments().into_iter().map(|(id, _)| id).collect()
                    }
                    _ => return,
                };
                if !open.is_empty() {
                    let target = open[usize::from(first) % open.len()];
                    self.submit(device, resolving(item, "done", &[target]));
                }
            }
            (17, Some(item)) => {
                // As the daemon writes a submission: the item moves to review, then the session ends.
                let coder = crate::name::AgentName::new("coder").unwrap();
                if self.replicas[device]
                    .agent(&coder, self.fixture.founder.key())
                    .is_none()
                {
                    return;
                }
                let Some(session) = self.submit_event(device, start_session("coder", item)) else {
                    return;
                };
                self.submit(device, moved(item, ItemState::Review));
                let ended = EndReason::Submitted {
                    commit: Some(commit(second % 2)),
                };
                self.submit(device, end_session(session.into(), ended));
            }
            (18, Some(item)) => {
                let Some((submitted, _)) = self.replicas[device].submission(item) else {
                    return;
                };
                // Only some verdicts are valid at a time: ask for changes when there are requests.
                let changes_requested = match self.replicas[device].item(item) {
                    Some(ItemView::Live(live)) => live
                        .open_comments()
                        .iter()
                        .any(|(_, comment)| comment.kind == CommentKind::ChangeRequest),
                    _ => return,
                };
                let kind = match (changes_requested, second % 4) {
                    (_, 0) => Verdict::Escalate,
                    (true, _) => Verdict::RequestChanges,
                    (false, _) => Verdict::Approve,
                };
                self.submit(device, verdict(item, submitted, kind));
            }
            (19, _) if second % 2 == 0 && device == 0 => {
                let (invite, secret) =
                    self.fixture
                        .invite(&self.replicas[0], self.clock / 2, 99_000);
                self.clock += 1;
                self.receive_unchecked(0, invite.clone());
                self.invites.push((invite, secret));
            }
            (19, _) => {
                // Devices redeem invites on whichever replica knows them: concurrently, one
                // device may be bound twice, and one invite may bind two devices.
                let joiner = &self.joiners[usize::from(first) % self.joiners.len()];
                let known: Vec<_> = self
                    .invites
                    .iter()
                    .filter(|(invite, _)| self.replicas[device].contains(&invite.id()))
                    .collect();
                if known.is_empty() || self.replicas[device].principal_of(&joiner.key()).is_some() {
                    return;
                }
                let (invite, secret) = known[usize::from(second) % known.len()];
                self.clock += 1;
                let bound = self.fixture.bind(
                    &self.replicas[device],
                    joiner,
                    invite,
                    secret,
                    self.clock / 2,
                );
                self.receive_unchecked(device, bound);
            }
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
    for _ in 0..3000 {
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
    let count = |matches: fn(&EventBody) -> bool| {
        simulation
            .authored
            .iter()
            .filter(|event| matches(&event.content.body))
            .count()
    };
    let reviews = count(|body| matches!(body, EventBody::Reviewed(_)));
    let requests = count(
        |body| matches!(body, EventBody::Reviewed(review) if review.verdict == Verdict::RequestChanges),
    );
    let resolving =
        count(|body| matches!(body, EventBody::Commented(comment) if !comment.resolves.is_empty()));
    let bindings = count(|body| matches!(body, EventBody::DeviceBound(_)));
    let submissions = count(
        |body| matches!(body, EventBody::SessionEnded(ended) if matches!(ended.reason, EndReason::Submitted { .. })),
    );
    assert!(
        requests > 0 && reviews > requests && resolving > 0 && bindings > 0 && submissions > 0,
        "{reviews} reviews, {requests} requesting changes, {resolving} resolving, {bindings} bindings, {submissions} submissions"
    );
    eprintln!(
        "events={} items={} archived={} ignored_edges={}",
        simulation.authored.len(),
        state.items().count(),
        archived,
        graph.ignored().len()
    );
}

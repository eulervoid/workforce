//! Several peers of one workforce write, partition, lose and duplicate messages, get restored
//! from old snapshots and reconnect. Once everyone can talk, every peer has the same state.

mod common;

use common::SharedReplica;

use proptest::prelude::*;
use wf_core::event::{Change, Event};
use wf_core::id::ItemId;
use wf_core::item::Priority;
use wf_core::state::State;
use wf_core::testing::*;
use wf_net::{initiate, read_open, respond};
use wf_store::{AuthorError, EventStore, RedbStore, Replica};

const MAX_PEERS: usize = 5;
const MAX_ROUNDS: usize = 8;

#[derive(Clone, Debug)]
enum Op {
    Write { peer: usize, kind: u8, choice: u8 },
    Sync { a: usize, b: usize },
    Partition(Vec<u8>),
    Leak { from: usize, to: usize, seed: u8 },
    Checkpoint { peer: usize },
    Restore { peer: usize },
}

fn op() -> impl Strategy<Value = Op> {
    let peer = 0..MAX_PEERS;
    prop_oneof![
        6 => (peer.clone(), any::<u8>(), any::<u8>())
            .prop_map(|(peer, kind, choice)| Op::Write { peer, kind, choice }),
        4 => (peer.clone(), peer.clone()).prop_map(|(a, b)| Op::Sync { a, b }),
        1 => prop::collection::vec(0u8..3, MAX_PEERS).prop_map(Op::Partition),
        2 => (peer.clone(), peer.clone(), any::<u8>())
            .prop_map(|(from, to, seed)| Op::Leak { from, to, seed }),
        1 => peer.clone().prop_map(|peer| Op::Checkpoint { peer }),
        1 => peer.prop_map(|peer| Op::Restore { peer }),
    ]
}

struct Peer {
    replica: SharedReplica<RedbStore>,
    checkpoint: Vec<Event>,
}

struct Cluster {
    fixture: Fixture,
    peers: Vec<Peer>,
    groups: Vec<u8>,
    clock: u64,
}

fn fresh_replica(fixture: &Fixture, events: &[Event]) -> SharedReplica<RedbStore> {
    let mut replica = Replica::open(RedbStore::in_memory().unwrap(), fixture.workforce()).unwrap();
    replica.receive(fixture.genesis.clone()).unwrap();
    for event in events {
        let _ = replica.receive(event.clone());
    }
    SharedReplica::new(replica)
}

impl Cluster {
    fn new(peers: usize) -> Self {
        let fixture = Fixture::new(peers);
        Self {
            peers: (0..peers)
                .map(|_| Peer {
                    replica: fresh_replica(&fixture, &[]),
                    checkpoint: Vec::new(),
                })
                .collect(),
            groups: vec![0; peers],
            clock: 2_000,
            fixture,
        }
    }

    fn write(&mut self, peer: usize, kind: u8, choice: u8) {
        let mut replica = self.peers[peer].replica.lock();
        let items: Vec<ItemId> = replica.state().items().map(|view| view.id()).collect();
        let item = (!items.is_empty()).then(|| items[usize::from(choice) % items.len()]);
        let body = match (kind % 4, item) {
            (1, Some(item)) => comment(item, "noted"),
            (2, Some(item)) => edit(
                item,
                Change::Priority(Some(if choice.is_multiple_of(2) {
                    Priority::P1
                } else {
                    Priority::P3
                })),
            ),
            (3, Some(item)) => moved(item, "done"),
            _ => create("task", &format!("task {}", self.clock)),
        };
        self.clock += 1;
        let result = replica.author(
            &self.fixture.devices[peer],
            self.fixture.founder_actor(),
            self.clock,
            body,
        );
        match result {
            Ok(_) | Err(AuthorError::OwnFeedEquivocated) | Err(AuthorError::Policy(_)) => {}
            Err(error) => panic!("unexpected authoring failure: {error}"),
        }
    }

    async fn sync(&self, a: usize, b: usize) {
        if a == b {
            return;
        }
        let (left, right) = tokio::io::duplex(4096);
        let workforce = self.fixture.workforce();
        let (first, second) =
            tokio::join!(initiate(left, workforce, &self.peers[a].replica), async {
                let mut right = right;
                assert_eq!(read_open(&mut right).await.unwrap(), workforce);
                respond(right, &self.peers[b].replica).await
            });
        first.unwrap();
        second.unwrap();
    }

    /// Delivers some of the events of one peer to another in a scrambled order, some twice.
    fn leak(&self, from: usize, to: usize, seed: u8) {
        if from == to {
            return;
        }
        let mut events: Vec<Event> = self.peers[from]
            .replica
            .lock()
            .store()
            .log_since(0)
            .unwrap();
        events.retain(|event| !(usize::from(seed) + event.content.seq as usize).is_multiple_of(3));
        events.reverse();
        let duplicates: Vec<Event> = events.iter().step_by(2).cloned().collect();
        events.extend(duplicates);
        let mut target = self.peers[to].replica.lock();
        for event in events {
            let _ = target.receive(event);
        }
    }

    fn restore(&mut self, peer: usize) {
        let events = std::mem::take(&mut self.peers[peer].checkpoint);
        self.peers[peer].replica = fresh_replica(&self.fixture, &events);
        self.peers[peer].checkpoint = events;
    }

    async fn run(&mut self, operation: Op) {
        let count = self.peers.len();
        match operation {
            Op::Write { peer, kind, choice } => self.write(peer % count, kind, choice),
            Op::Sync { a, b } => {
                let (a, b) = (a % count, b % count);
                if self.groups[a] == self.groups[b] {
                    self.sync(a, b).await;
                }
            }
            Op::Partition(groups) => self.groups = groups[..count].to_vec(),
            Op::Leak { from, to, seed } => self.leak(from % count, to % count, seed),
            Op::Checkpoint { peer } => {
                let peer = &mut self.peers[peer % count];
                peer.checkpoint = peer.replica.lock().store().log_since(0).unwrap();
            }
            Op::Restore { peer } => self.restore(peer % count),
        }
    }

    /// Syncs every pair until nothing changes. A peer that learns of a fork only offers its
    /// other branch on the next sync, so a few rounds can be needed.
    async fn reconnect_everyone(&self) {
        for _ in 0..MAX_ROUNDS {
            let before = self.states();
            for a in 0..self.peers.len() {
                for b in a + 1..self.peers.len() {
                    self.sync(a, b).await;
                }
            }
            if self.encoded(&before) == self.encoded(&self.states()) {
                return;
            }
        }
        panic!("peers keep changing after {MAX_ROUNDS} rounds of syncing");
    }

    fn encoded(&self, states: &[State]) -> Vec<Vec<u8>> {
        states
            .iter()
            .map(|state| wf_core::encoding::encode(state).unwrap())
            .collect()
    }

    fn states(&self) -> Vec<State> {
        self.peers
            .iter()
            .map(|peer| peer.replica.lock().state().clone())
            .collect()
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
}

/// Parked events are local: one whose parent exists nowhere (the only peer holding it was
/// restored from an old snapshot) stays parked, and does not take part in the state. The
/// snapshot encoding leaves them out.
fn assert_converged(states: &[State]) {
    let encoded: Vec<Vec<u8>> = states
        .iter()
        .map(|state| wf_core::encoding::encode(state).unwrap())
        .collect();
    for (index, state) in states.iter().enumerate() {
        assert!(
            encoded[index] == encoded[0],
            "peer {index} has another state"
        );
        assert_eq!(state.heads(), states[0].heads());
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 1000, ..ProptestConfig::default() })]

    #[test]
    fn every_peer_ends_with_the_same_state(
        peers in 3usize..=MAX_PEERS,
        operations in prop::collection::vec(op(), 1..40),
    ) {
        runtime().block_on(async {
            let mut cluster = Cluster::new(peers);
            for operation in operations {
                cluster.run(operation).await;
            }
            cluster.reconnect_everyone().await;
            assert_converged(&cluster.states());
        });
    }
}

#[test]
fn a_device_restored_from_a_snapshot_splits_nobody() {
    runtime().block_on(async {
        let mut cluster = Cluster::new(3);
        cluster.write(0, 0, 0);
        cluster.run(Op::Checkpoint { peer: 0 }).await;
        cluster.write(0, 0, 0);
        cluster.write(0, 0, 0);
        cluster.sync(0, 1).await;

        cluster.run(Op::Restore { peer: 0 }).await;
        cluster.write(0, 0, 0);
        cluster.reconnect_everyone().await;

        let states = cluster.states();
        assert_converged(&states);
        assert!(
            states
                .iter()
                .all(|state| state.pending_events().count() == 0)
        );
        let device = cluster.fixture.devices[0].key();
        let flagged: Vec<_> = states[0].equivocations().collect();
        assert_eq!(flagged.len(), 1);
        assert_eq!(flagged[0].0, device);
        assert_eq!(states[0].feeds()[&device].tips().count(), 2);

        // The cloned device stops writing once it has seen the other branch.
        let before = states[0].event_count();
        cluster.write(0, 0, 0);
        assert_eq!(cluster.states()[0].event_count(), before);
    });
}

#[test]
fn missing_parents_are_fetched_by_id() {
    runtime().block_on(async {
        let cluster = Cluster::new(2);
        let mut author = cluster.fixture.state();
        let events: Vec<Event> = (0..5)
            .map(|index| {
                cluster
                    .fixture
                    .apply(&mut author, 0, 2_000 + index, create("task", "t"))
            })
            .collect();
        // Peer 1 forked off after the first event with an event of its own, so it is level with
        // peer 0 at seq 2 but lacks everything on the other branch.
        for event in &events[..2] {
            cluster.peers[0]
                .replica
                .lock()
                .receive(event.clone())
                .unwrap();
        }
        cluster.peers[1]
            .replica
            .lock()
            .receive(events[0].clone())
            .unwrap();
        let mut other = cluster.fixture.state();
        other
            .receive(events[0].clone(), wf_core::state::Trust::Untrusted)
            .unwrap();
        let branch = cluster
            .fixture
            .apply(&mut other, 0, 3_000, create("task", "branch"));
        cluster.peers[1].replica.lock().receive(branch).unwrap();

        cluster.reconnect_everyone().await;
        let states = cluster.states();
        assert_converged(&states);
        assert!(
            states
                .iter()
                .all(|state| state.pending_events().count() == 0)
        );
        assert_eq!(states[0].equivocations().count(), 1);
    });
}

#[test]
fn a_leaked_invite_redeemed_through_two_partitioned_members_flags_both_devices() {
    use wf_core::event::{DeviceBound, EventBody};
    use wf_core::identity::DeviceSigner;

    runtime().block_on(async {
        let mut cluster = Cluster::new(3);
        let (invite, secret) = {
            let replica = cluster.peers[0].replica.lock();
            cluster.fixture.invite(replica.state(), 5_000, u64::MAX / 2)
        };
        cluster.peers[0]
            .replica
            .lock()
            .receive(invite.clone())
            .unwrap();
        cluster.reconnect_everyone().await;

        cluster.run(Op::Partition(vec![0, 1, 2, 0, 0])).await;
        let mut bound = Vec::new();
        for (member, seed) in [(0, 80u8), (1, 81)] {
            let device = DeviceSigner::from_bytes(&[seed; 32]).unwrap();
            let proof = secret
                .prove(cluster.fixture.workforce(), invite.id(), device.key())
                .unwrap();
            cluster.peers[member]
                .replica
                .lock()
                .author(
                    &device,
                    cluster.fixture.founder_actor(),
                    9_000 + u64::from(seed),
                    EventBody::DeviceBound(DeviceBound {
                        invite: invite.id(),
                        proof,
                    }),
                )
                .unwrap();
            bound.push(device.key());
        }
        cluster.run(Op::Partition(vec![0, 0, 0, 0, 0])).await;
        cluster.reconnect_everyone().await;

        let states = cluster.states();
        assert_converged(&states);
        for state in &states {
            let flagged: Vec<_> = state
                .devices()
                .filter(|info| info.invite_redeemed_twice)
                .map(|info| info.device)
                .collect();
            assert_eq!(flagged.len(), 2);
            assert!(bound.iter().all(|device| flagged.contains(device)));
        }
    });
}

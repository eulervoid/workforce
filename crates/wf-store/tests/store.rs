use tempfile::TempDir;
use wf_core::event::{Event, EventBody};
use wf_core::id::EventId;
use wf_core::state::{Disposition, State, Trust};
use wf_core::testing::*;
use wf_store::{
    Commit, EventStore, PendingChange, RedbStore, Replica, SNAPSHOT_INTERVAL, SnapshotChange,
};

fn open(directory: &TempDir) -> RedbStore {
    RedbStore::open(directory.path().join("store.redb")).unwrap()
}

fn new_replica(directory: &TempDir, fixture: &Fixture) -> Replica<RedbStore> {
    let mut replica = Replica::open(open(directory), fixture.workforce()).unwrap();
    replica.receive(fixture.genesis.clone()).unwrap();
    replica
}

fn add(replica: &mut Replica<RedbStore>, fixture: &Fixture, count: usize) -> Vec<EventId> {
    (0..count)
        .map(|index| {
            let now = 2_000 + replica.state().event_count() as u64;
            let event = replica
                .author(
                    &fixture.devices[0],
                    fixture.founder_actor(),
                    now,
                    create("task", &format!("task {index}")),
                )
                .unwrap();
            event.id()
        })
        .collect()
}

fn titles(state: &State) -> Vec<String> {
    state
        .ready_items()
        .iter()
        .map(|item| item.title().to_string())
        .collect()
}

#[test]
fn events_survive_reopening() {
    let directory = TempDir::new().unwrap();
    let fixture = Fixture::new(1);
    let expected = {
        let mut replica = new_replica(&directory, &fixture);
        add(&mut replica, &fixture, 5);
        replica.state().clone()
    };
    let reopened = Replica::open(open(&directory), fixture.workforce()).unwrap();
    assert_eq!(reopened.state(), &expected);
    assert_eq!(reopened.store().log_since(0).unwrap().len(), 6);
}

#[test]
fn a_shared_state_stays_as_it_was_taken() {
    let directory = TempDir::new().unwrap();
    let fixture = Fixture::new(1);
    let mut replica = new_replica(&directory, &fixture);
    add(&mut replica, &fixture, 1);
    let taken = replica.shared_state();
    add(&mut replica, &fixture, 2);
    assert_eq!(titles(&taken), ["task 0"]);
    assert_eq!(titles(replica.state()), ["task 0", "task 0", "task 1"]);
    assert_eq!(&*replica.shared_state(), replica.state());
}

#[test]
fn events_can_be_read_back_by_id_and_by_feed() {
    let directory = TempDir::new().unwrap();
    let fixture = Fixture::new(1);
    let mut replica = new_replica(&directory, &fixture);
    let ids = add(&mut replica, &fixture, 3);
    let store = replica.store();

    let found = store.event(&ids[1]).unwrap().unwrap();
    assert_eq!(found.id(), ids[1]);
    assert!(store.event(&EventId::of(b"missing")).unwrap().is_none());

    let author = fixture.devices[0].key();
    let all = store.feed_since(&author, 0).unwrap();
    assert_eq!(
        all.iter()
            .map(|event| event.content.seq)
            .collect::<Vec<_>>(),
        [1, 2, 3, 4]
    );
    let tail = store.feed_since(&author, 2).unwrap();
    assert_eq!(
        tail.iter()
            .map(|event| event.content.seq)
            .collect::<Vec<_>>(),
        [3, 4]
    );
    assert_eq!(store.log_len().unwrap(), 4);
    assert_eq!(store.log_id(0).unwrap(), Some(fixture.genesis.id()));
    let replayed = store.log_since(2).unwrap();
    assert_eq!(replayed.len(), 2);
    assert_eq!(replayed[0].id(), ids[1]);
}

#[test]
fn one_commit_stores_events_pending_events_and_the_snapshot_together() {
    let directory = TempDir::new().unwrap();
    let fixture = Fixture::new(1);
    let mut author = fixture.state();
    let first = fixture.apply(&mut author, 0, 2_000, create("task", "a"));
    let second = fixture.apply(&mut author, 0, 2_001, create("task", "b"));
    let third = fixture.apply(&mut author, 0, 2_002, create("task", "c"));

    let snapshot = vec![1];
    {
        let store = open(&directory);
        store
            .commit(Commit {
                append: &[fixture.genesis.clone(), first, second],
                pending: PendingChange::Update {
                    put: std::slice::from_ref(&third),
                    remove: &[],
                },
                snapshot: SnapshotChange::Replace(&snapshot),
            })
            .unwrap();
    }
    let store = open(&directory);
    assert_eq!(store.log_since(0).unwrap().len(), 3);
    assert_eq!(store.log_len().unwrap(), 3);
    assert_eq!(store.pending().unwrap().len(), 1);
    assert_eq!(store.snapshot().unwrap(), Some(snapshot));
}

#[test]
fn snapshots_are_written_atomically_with_events_and_replay_equals_refold() {
    let directory = TempDir::new().unwrap();
    let fixture = Fixture::new(1);
    let expected = {
        let mut replica = new_replica(&directory, &fixture);
        add(&mut replica, &fixture, SNAPSHOT_INTERVAL + 20);
        assert!(replica.store().snapshot().unwrap().is_some());
        replica.state().clone()
    };

    let with_snapshot = Replica::open(open(&directory), fixture.workforce()).unwrap();
    assert_eq!(with_snapshot.state(), &expected);
    drop(with_snapshot);

    let refolded = Replica::refold(open(&directory), fixture.workforce()).unwrap();
    assert_eq!(refolded.state(), &expected);
    assert_eq!(titles(refolded.state()), titles(&expected));
}

#[test]
fn deleting_the_snapshot_changes_nothing() {
    let directory = TempDir::new().unwrap();
    let fixture = Fixture::new(1);
    let expected = {
        let mut replica = new_replica(&directory, &fixture);
        add(&mut replica, &fixture, SNAPSHOT_INTERVAL + 5);
        replica.state().clone()
    };
    {
        let database = redb::Database::open(directory.path().join("store.redb")).unwrap();
        let transaction = database.begin_write().unwrap();
        transaction
            .open_table(redb::TableDefinition::<&str, &[u8]>::new("meta"))
            .unwrap()
            .remove("snapshot")
            .unwrap();
        transaction.commit().unwrap();
    }
    let store = open(&directory);
    assert!(store.snapshot().unwrap().is_none());
    let replica = Replica::open(store, fixture.workforce()).unwrap();
    assert_eq!(replica.state(), &expected);
    assert!(
        replica.store().snapshot().unwrap().is_some(),
        "refolding writes a new snapshot"
    );
}

#[test]
fn unusable_snapshots_are_discarded_and_refolded() {
    let directory = TempDir::new().unwrap();
    let fixture = Fixture::new(1);
    let expected = {
        let mut replica = new_replica(&directory, &fixture);
        add(&mut replica, &fixture, 10);
        replica.state().clone()
    };
    for garbage in [vec![], vec![9, 9, 9, 9], vec![0, 0, 0, 0, 0, 0, 0, 0]] {
        let store = open(&directory);
        store
            .commit(Commit {
                snapshot: SnapshotChange::Replace(&garbage),
                ..Commit::default()
            })
            .unwrap();
        let replica = Replica::open(store, fixture.workforce()).unwrap();
        assert_eq!(replica.state(), &expected);
    }
}

#[test]
fn a_snapshot_of_the_previous_format_is_discarded_and_refolded() {
    const PREVIOUS_FORMAT: u32 = 10;
    let directory = TempDir::new().unwrap();
    let fixture = Fixture::new(1);
    let expected = {
        let mut replica = new_replica(&directory, &fixture);
        add(&mut replica, &fixture, 10);
        replica.state().clone()
    };
    let previous =
        wf_core::encoding::encode(&(PREVIOUS_FORMAT, wf_core::refs::PARSER_VERSION, &expected))
            .unwrap();
    let store = open(&directory);
    store
        .commit(Commit {
            snapshot: SnapshotChange::Replace(&previous),
            ..Commit::default()
        })
        .unwrap();
    let replica = Replica::open(store, fixture.workforce()).unwrap();
    assert_eq!(replica.state(), &expected);
    let rewritten = replica.store().snapshot().unwrap().unwrap();
    let (format, _, _) =
        wf_core::encoding::decode::<(u32, u16, wf_core::state::State)>(&rewritten).unwrap();
    assert_ne!(
        format, PREVIOUS_FORMAT,
        "refolding writes a snapshot of the current format"
    );
}

#[test]
fn a_store_of_an_older_event_version_is_refused_with_how_to_sign_it_again() {
    let directory = TempDir::new().unwrap();
    let fixture = Fixture::new(1);
    let mut content = fixture.genesis.content.clone();
    content.v = wf_core::event::EVENT_VERSION - 1;
    let old = Event::sign_content(content, &fixture.devices[0]).unwrap();
    open(&directory)
        .commit(Commit {
            append: &[old],
            ..Commit::default()
        })
        .unwrap();
    let Err(error) = Replica::open(open(&directory), fixture.workforce()) else {
        panic!("a store of an older event version opened");
    };
    let message = wf_core::error_chain(&error);
    assert!(message.contains("wf import --resign"), "{message}");
}

#[test]
fn out_of_order_events_are_parked_and_survive_restarts() {
    let directory = TempDir::new().unwrap();
    let fixture = Fixture::new(1);
    let mut author = fixture.state();
    let first = fixture.apply(&mut author, 0, 2_000, create("task", "a"));
    let second = fixture.apply(&mut author, 0, 2_001, create("task", "b"));

    {
        let mut replica = new_replica(&directory, &fixture);
        let received = replica.receive(second.clone()).unwrap();
        assert_eq!(received.disposition, Disposition::Pending);
        assert_eq!(replica.store().pending().unwrap().len(), 1);
        assert_eq!(replica.store().log_since(0).unwrap().len(), 1);
    }
    {
        let mut replica = Replica::open(open(&directory), fixture.workforce()).unwrap();
        assert_eq!(
            replica.state().pending_events().count(),
            1,
            "parked events are restored"
        );
        let received = replica.receive(first).unwrap();
        assert_eq!(received.applied.len(), 2);
        assert!(replica.store().pending().unwrap().is_empty());
        assert_eq!(replica.store().log_since(0).unwrap().len(), 3);
        assert_eq!(replica.state(), &author);
    }
}

#[test]
fn duplicates_are_idempotent() {
    let directory = TempDir::new().unwrap();
    let fixture = Fixture::new(1);
    let mut replica = new_replica(&directory, &fixture);
    let event = replica
        .author(
            &fixture.devices[0],
            fixture.founder_actor(),
            2_000,
            create("task", "a"),
        )
        .unwrap();
    let before = replica.state().clone();
    let received = replica.receive(event).unwrap();
    assert_eq!(received.disposition, Disposition::Duplicate);
    assert_eq!(replica.state(), &before);
    assert_eq!(replica.store().log_since(0).unwrap().len(), 2);
}

#[test]
fn invalid_events_are_refused_and_not_stored() {
    let directory = TempDir::new().unwrap();
    let fixture = Fixture::new(1);
    let mut replica = new_replica(&directory, &fixture);
    let mut forged = fixture.author(replica.state(), 0, 2_000, create("task", "a"));
    forged.content.hlc = wf_core::hlc::Hlc::next_after(forged.content.hlc, 99_999).unwrap();
    assert!(replica.receive(forged).is_err());
    assert_eq!(replica.store().log_since(0).unwrap().len(), 1);

    let violation = replica.author(
        &fixture.devices[0],
        fixture.founder_actor(),
        2_001,
        create("nonsense", "a"),
    );
    assert!(matches!(violation, Err(wf_store::AuthorError::Policy(_))));
    assert_eq!(replica.store().log_since(0).unwrap().len(), 1);
}

#[test]
fn stored_events_decode_to_what_was_written() {
    let directory = TempDir::new().unwrap();
    let fixture = Fixture::new(1);
    let mut replica = new_replica(&directory, &fixture);
    let event: Event = replica
        .author(
            &fixture.devices[0],
            fixture.founder_actor(),
            2_000,
            EventBody::Unknown {
                tag: 1234,
                payload: vec![1, 2, 3],
            },
        )
        .unwrap();
    let stored = replica.store().event(&event.id()).unwrap().unwrap();
    assert_eq!(stored, event);
    stored.verify_signature().unwrap();
    let mut state = State::new();
    for stored in replica.store().log_since(0).unwrap() {
        state.receive(stored, Trust::Untrusted).unwrap();
    }
    assert_eq!(&state, replica.state());
}

/// Exit criterion: startup with 100k events stays under 100 ms with a snapshot.
/// Run with `cargo test --release -p wf-store -- --ignored --nocapture`.
#[test]
#[ignore = "slow; run in release mode"]
fn startup_with_100k_events_and_a_snapshot_is_fast() {
    use std::time::Instant;
    use wf_core::event::Change;

    let directory = TempDir::new().unwrap();
    let fixture = Fixture::new(1);
    let store = open(&directory);
    let mut state = fixture.state();
    let mut batch = vec![fixture.genesis.clone()];
    let mut items = Vec::new();
    let mut now = 2_000u64;
    for index in 0..100_000usize {
        now += 1;
        let body = match (index % 10, items.last().copied()) {
            (0..=2, _) | (_, None) => {
                create_with_body("task", &format!("task {index}"), "see #zzzz and @ada")
            }
            (3..=5, Some(item)) => edit(item, Change::Title(format!("renamed {index}"))),
            (6..=7, Some(item)) => comment(item, "progress note @ada"),
            (_, Some(item)) => moved(item, ItemState::Active),
        };
        let event = fixture.author(&state, 0, now, body);
        state.receive(event.clone(), Trust::Stored).unwrap();
        if matches!(event.content.body, EventBody::ItemCreated(_)) {
            items.push(event.id().into());
        }
        batch.push(event);
        if batch.len() == 5_000 {
            store
                .commit(Commit {
                    append: &batch,
                    ..Commit::default()
                })
                .unwrap();
            batch.clear();
        }
    }
    store
        .commit(Commit {
            append: &batch,
            ..Commit::default()
        })
        .unwrap();

    let started = Instant::now();
    let replica = Replica::open(store, fixture.workforce()).unwrap();
    let first_open = started.elapsed();
    assert_eq!(replica.state(), &state);
    drop(replica);

    let started = Instant::now();
    let replica = Replica::open(open(&directory), fixture.workforce()).unwrap();
    let with_snapshot = started.elapsed();
    assert_eq!(replica.state().event_count(), 100_001);
    eprintln!("first open (full fold): {first_open:?}, open with snapshot: {with_snapshot:?}");
    assert!(with_snapshot.as_millis() < 100, "{with_snapshot:?}");
}

#[test]
fn both_branches_of_a_forked_feed_are_stored_and_replayed() {
    let directory = TempDir::new().unwrap();
    let fixture = Fixture::new(1);
    let mut replica = new_replica(&directory, &fixture);
    let mut branch_state = fixture.state();
    let branch_a = fixture.apply(&mut branch_state, 0, 2_000, create("task", "a"));
    let mut other_state = fixture.state();
    let branch_b = fixture.apply(&mut other_state, 0, 2_001, create("task", "b"));
    replica.receive(branch_a.clone()).unwrap();
    replica.receive(branch_b.clone()).unwrap();

    let author = fixture.devices[0].key();
    let stored = replica.store().feed_since(&author, 1).unwrap();
    assert_eq!(stored.len(), 2);
    assert_eq!(replica.state().equivocations().count(), 1);
    let expected = replica.state().clone();
    drop(replica);

    let reopened = Replica::refold(open(&directory), fixture.workforce()).unwrap();
    assert_eq!(reopened.state(), &expected);
}

#[test]
fn a_replica_refuses_to_write_on_a_forked_own_feed() {
    let directory = TempDir::new().unwrap();
    let fixture = Fixture::new(1);
    let mut replica = new_replica(&directory, &fixture);
    let mut other_state = fixture.state();
    let branch = fixture.apply(&mut other_state, 0, 2_001, create("task", "elsewhere"));
    add(&mut replica, &fixture, 1);
    replica.receive(branch).unwrap();
    let result = replica.author(
        &fixture.devices[0],
        fixture.founder_actor(),
        9_000,
        create("task", "more"),
    );
    assert!(matches!(
        result,
        Err(wf_store::AuthorError::OwnFeedEquivocated)
    ));
}

#[test]
fn a_store_of_another_workforce_is_refused() {
    let directory = TempDir::new().unwrap();
    let fixture = Fixture::new(1);
    {
        let mut replica = new_replica(&directory, &fixture);
        add(&mut replica, &fixture, SNAPSHOT_INTERVAL + 5);
    }
    let other = Fixture::new(2).workforce();
    for result in [
        Replica::open(open(&directory), other),
        Replica::refold(open(&directory), other),
    ] {
        assert!(matches!(
            result,
            Err(wf_store::StoreError::WrongWorkforce { .. })
        ));
    }
}

#[test]
fn a_replica_expecting_a_workforce_refuses_the_genesis_of_another() {
    let directory = TempDir::new().unwrap();
    let fixture = Fixture::new(1);
    let mut replica = Replica::open(open(&directory), Fixture::new(2).workforce()).unwrap();
    assert!(replica.receive(fixture.genesis.clone()).is_err());
    assert_eq!(replica.store().log_len().unwrap(), 0);
}

fn parent_with_items(directory: &TempDir, fixture: &Fixture, count: usize) -> Replica<RedbStore> {
    let mut parent = new_replica(directory, fixture);
    add(&mut parent, fixture, count);
    parent
}

#[test]
fn a_fork_is_created_complete_from_the_replica_it_was_made_from() {
    let parent_directory = TempDir::new().unwrap();
    let child_directory = TempDir::new().unwrap();
    let fixture = Fixture::new(1);
    let parent = parent_with_items(&parent_directory, &fixture, 20);
    let founder = wf_core::identity::PrincipalSigner::from_bytes(&[1; 32]).unwrap();
    let genesis = fixture.fork(parent.state(), &founder, 90_000);

    let child = Replica::create_fork(open(&child_directory), genesis.clone(), &parent).unwrap();
    assert!(!child.state().history_incomplete());
    assert_eq!(
        child.state().event_count(),
        parent.state().event_count() + 1
    );
    assert_eq!(titles(child.state()), titles(parent.state()));
    drop(child);

    let reopened = Replica::open(open(&child_directory), genesis.id().into()).unwrap();
    assert!(!reopened.state().history_incomplete());
    assert_eq!(reopened.state().items().count(), 20);
}

#[test]
fn a_fork_cannot_be_created_from_a_replica_that_lacks_its_history() {
    let parent_directory = TempDir::new().unwrap();
    let child_directory = TempDir::new().unwrap();
    let other_directory = TempDir::new().unwrap();
    let fixture = Fixture::new(1);
    let parent = parent_with_items(&parent_directory, &fixture, 5);
    let founder = wf_core::identity::PrincipalSigner::from_bytes(&[1; 32]).unwrap();
    let genesis = fixture.fork(parent.state(), &founder, 90_000);

    let behind = new_replica(&other_directory, &fixture);
    let result = Replica::create_fork(open(&child_directory), genesis, &behind);
    assert!(matches!(
        result,
        Err(wf_store::ForkError::HistoryIncomplete)
    ));
}

#[test]
#[ignore = "slow; run in release mode"]
fn forking_a_workforce_with_20k_events_from_a_local_replica_is_fast() {
    let parent_directory = TempDir::new().unwrap();
    let child_directory = TempDir::new().unwrap();
    let fixture = Fixture::new(1);
    let mut parent = new_replica(&parent_directory, &fixture);
    let mut batch = Vec::new();
    let mut state = parent.state().clone();
    for index in 0..20_000u64 {
        let event = fixture.author(
            &state,
            0,
            2_000 + index,
            create("task", &format!("t{index}")),
        );
        state.receive(event.clone(), Trust::Stored).unwrap();
        batch.push(event);
    }
    parent.receive_batch(batch).unwrap();
    let founder = wf_core::identity::PrincipalSigner::from_bytes(&[1; 32]).unwrap();
    let genesis = fixture.fork(parent.state(), &founder, 90_000_000);

    let started = std::time::Instant::now();
    let child = Replica::create_fork(open(&child_directory), genesis, &parent).unwrap();
    let elapsed = started.elapsed();
    println!(
        "forked {} events in {elapsed:?}",
        child.state().event_count()
    );
    assert!(elapsed < std::time::Duration::from_secs(5), "{elapsed:?}");
}

/// A store whose writes can be made to fail and are counted.
struct Flaky {
    inner: RedbStore,
    failing: std::cell::Cell<bool>,
    commits: std::cell::Cell<usize>,
}

impl EventStore for Flaky {
    fn event(&self, id: &EventId) -> Result<Option<Event>, wf_store::StoreError> {
        self.inner.event(id)
    }
    fn feed_since(
        &self,
        author: &wf_core::identity::DeviceKey,
        after_seq: u64,
    ) -> Result<Vec<Event>, wf_store::StoreError> {
        self.inner.feed_since(author, after_seq)
    }
    fn log_len(&self) -> Result<u64, wf_store::StoreError> {
        self.inner.log_len()
    }
    fn log_id(&self, position: u64) -> Result<Option<EventId>, wf_store::StoreError> {
        self.inner.log_id(position)
    }
    fn log_page(&self, position: u64, limit: usize) -> Result<Vec<Event>, wf_store::StoreError> {
        self.inner.log_page(position, limit)
    }
    fn pending(&self) -> Result<Vec<Event>, wf_store::StoreError> {
        self.inner.pending()
    }
    fn snapshot(&self) -> Result<Option<Vec<u8>>, wf_store::StoreError> {
        self.inner.snapshot()
    }
    fn commit(&self, commit: Commit<'_>) -> Result<(), wf_store::StoreError> {
        if self.failing.get() {
            return Err(wf_store::StoreError::Inconsistent("disk full".into()));
        }
        self.commits.set(self.commits.get() + 1);
        self.inner.commit(commit)
    }
}

#[test]
fn a_failed_write_marks_the_replica_for_reopening() {
    let fixture = Fixture::new(1);
    let store = Flaky {
        inner: RedbStore::in_memory().unwrap(),
        failing: std::cell::Cell::new(false),
        commits: std::cell::Cell::new(0),
    };
    let mut replica = Replica::open(store, fixture.workforce()).unwrap();
    replica.receive(fixture.genesis.clone()).unwrap();
    assert_eq!(replica.failed_write(), None);

    replica.store().failing.set(true);
    let result = replica.author(
        &fixture.devices[0],
        fixture.founder_actor(),
        2_000,
        create("task", "never stored"),
    );
    assert!(result.is_err());
    assert!(replica.failed_write().is_some());
    assert_eq!(replica.store().log_len().unwrap(), 1);
}

#[test]
fn opening_with_many_pending_events_commits_them_together() {
    const PENDING: usize = 2_000;
    let fixture = Fixture::new(1);
    let mut author = fixture.state();
    let _missing = fixture.apply(&mut author, 0, 2_000, create("task", "missing"));
    let waiting: Vec<Event> = (0..PENDING)
        .map(|index| {
            fixture.apply(
                &mut author,
                0,
                2_001 + index as u64,
                create("task", &format!("task {index}")),
            )
        })
        .collect();
    let store = Flaky {
        inner: RedbStore::in_memory().unwrap(),
        failing: std::cell::Cell::new(false),
        commits: std::cell::Cell::new(0),
    };
    store
        .commit(Commit {
            append: std::slice::from_ref(&fixture.genesis),
            pending: PendingChange::Update {
                put: &waiting,
                remove: &[],
            },
            snapshot: SnapshotChange::Keep,
        })
        .unwrap();
    store.commits.set(0);

    let replica = Replica::open(store, fixture.workforce()).unwrap();
    assert_eq!(replica.state().pending_events().count(), PENDING);
    // The pending events in one commit, and at most the snapshot of what was replayed.
    assert!(replica.store().commits.get() <= 2);
}

#[test]
fn own_events_are_not_judged_by_the_clock_after_it_went_back() {
    let directory = TempDir::new().unwrap();
    let fixture = Fixture::new(1);
    let mut replica = new_replica(&directory, &fixture);
    let ten_hours_ms = 10 * 60 * 60 * 1_000;
    let far_ahead = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + ten_hours_ms;
    for now in [far_ahead, 2_000] {
        replica
            .author(
                &fixture.devices[0],
                fixture.founder_actor(),
                now,
                create("task", "a"),
            )
            .unwrap();
    }
    assert_eq!(replica.state().event_count(), 3);
}

#[test]
fn a_stored_pending_event_that_admission_refuses_is_dropped_on_open() {
    let directory = TempDir::new().unwrap();
    let fixture = Fixture::new(1);
    let foreign = Fixture::new(2);
    {
        let mut replica = new_replica(&directory, &fixture);
        add(&mut replica, &fixture, 1);
        replica
            .store()
            .commit(Commit {
                append: &[],
                pending: PendingChange::Update {
                    put: std::slice::from_ref(&foreign.genesis),
                    remove: &[],
                },
                snapshot: SnapshotChange::Keep,
            })
            .unwrap();
        assert_eq!(replica.store().pending().unwrap().len(), 1);
    }
    let reopened = Replica::open(open(&directory), fixture.workforce()).unwrap();
    assert!(reopened.store().pending().unwrap().is_empty());
    assert_eq!(reopened.state().pending_events().count(), 0);
    assert_eq!(reopened.store().log_since(0).unwrap().len(), 2);
}

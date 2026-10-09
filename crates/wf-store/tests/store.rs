use tempfile::TempDir;
use wf_core::event::{Event, EventBody};
use wf_core::id::{EventId, ItemId};
use wf_core::state::{Disposition, State, Trust};
use wf_core::testing::*;
use wf_store::{
    Commit, EventStore, PendingChange, RedbStore, Replica, SNAPSHOT_INTERVAL, SnapshotChange,
    StoreError, StoredSnapshot,
};

fn open(directory: &TempDir) -> RedbStore {
    RedbStore::open(directory.path().join("store.redb")).unwrap()
}

fn new_replica(directory: &TempDir, fixture: &Fixture) -> Replica<RedbStore> {
    let mut replica = Replica::open(open(directory)).unwrap();
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
    let reopened = Replica::open(open(&directory)).unwrap();
    assert_eq!(reopened.state(), &expected);
    assert_eq!(reopened.store().events().unwrap().len(), 6);
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
    let heads = store.feed_heads().unwrap();
    assert_eq!(heads.len(), 1);
    assert_eq!(heads[0].1.seq, 4);
}

#[test]
fn appends_must_be_gapless() {
    let directory = TempDir::new().unwrap();
    let fixture = Fixture::new(1);
    let mut author = fixture.state();
    let first = fixture.apply(&mut author, 0, 2_000, create("task", "a"));
    let second = fixture.apply(&mut author, 0, 2_001, create("task", "b"));

    let store = open(&directory);
    store
        .commit(Commit {
            append: std::slice::from_ref(&fixture.genesis),
            ..Commit::default()
        })
        .unwrap();
    let error = store
        .commit(Commit {
            append: std::slice::from_ref(&second),
            ..Commit::default()
        })
        .unwrap_err();
    assert!(matches!(
        error,
        StoreError::FeedGap {
            expected: 2,
            got: 3,
            ..
        }
    ));
    store
        .commit(Commit {
            append: &[first, second],
            ..Commit::default()
        })
        .unwrap();
    assert_eq!(store.events().unwrap().len(), 3);
}

#[test]
fn a_failed_commit_changes_nothing() {
    let directory = TempDir::new().unwrap();
    let fixture = Fixture::new(1);
    let mut author = fixture.state();
    let first = fixture.apply(&mut author, 0, 2_000, create("task", "a"));
    let second = fixture.apply(&mut author, 0, 2_001, create("task", "b"));
    let third = fixture.apply(&mut author, 0, 2_002, create("task", "c"));

    let store = open(&directory);
    store
        .commit(Commit {
            append: std::slice::from_ref(&fixture.genesis),
            ..Commit::default()
        })
        .unwrap();
    let snapshot = StoredSnapshot {
        covers: Default::default(),
        bytes: vec![1],
    };
    // `first` is fine but `third` skips `second`: the whole transaction must roll back.
    let result = store.commit(Commit {
        append: &[first, third],
        pending: PendingChange::Update {
            put: std::slice::from_ref(&second),
            remove: &[],
        },
        snapshot: SnapshotChange::Replace(&snapshot),
    });
    assert!(result.is_err());
    drop(store);

    let store = open(&directory);
    assert_eq!(store.events().unwrap().len(), 1);
    assert!(store.pending().unwrap().is_empty());
    assert!(store.snapshot().unwrap().is_none());
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

    let with_snapshot = Replica::open(open(&directory)).unwrap();
    assert_eq!(with_snapshot.state(), &expected);
    drop(with_snapshot);

    let refolded = Replica::refold(open(&directory)).unwrap();
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
    let store = open(&directory);
    store
        .commit(Commit {
            snapshot: SnapshotChange::Discard,
            ..Commit::default()
        })
        .unwrap();
    assert!(store.snapshot().unwrap().is_none());
    let replica = Replica::open(store).unwrap();
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
                snapshot: SnapshotChange::Replace(&StoredSnapshot {
                    covers: Default::default(),
                    bytes: garbage,
                }),
                ..Commit::default()
            })
            .unwrap();
        let replica = Replica::open(store).unwrap();
        assert_eq!(replica.state(), &expected);
    }
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
        assert_eq!(replica.store().events().unwrap().len(), 1);
    }
    {
        let mut replica = Replica::open(open(&directory)).unwrap();
        assert_eq!(
            replica.state().pending_events().count(),
            1,
            "parked events are restored"
        );
        let received = replica.receive(first).unwrap();
        assert_eq!(received.applied.len(), 2);
        assert!(replica.store().pending().unwrap().is_empty());
        assert_eq!(replica.store().events().unwrap().len(), 3);
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
    assert_eq!(replica.store().events().unwrap().len(), 2);
}

#[test]
fn invalid_events_are_refused_and_not_stored() {
    let directory = TempDir::new().unwrap();
    let fixture = Fixture::new(1);
    let mut replica = new_replica(&directory, &fixture);
    let mut forged = fixture.author(replica.state(), 0, 2_000, create("task", "a"));
    forged.content.hlc = wf_core::hlc::Hlc::next_after(forged.content.hlc, 99_999);
    assert!(replica.receive(forged).is_err());
    assert_eq!(replica.store().events().unwrap().len(), 1);

    let violation = replica.author(
        &fixture.devices[0],
        fixture.founder_actor(),
        2_001,
        create("nonsense", "a"),
    );
    assert!(matches!(violation, Err(wf_store::AuthorError::Policy(_))));
    assert_eq!(replica.store().events().unwrap().len(), 1);
}

#[test]
fn a_second_handle_waits_for_the_lock() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("store.redb");
    let first = RedbStore::open(&path).unwrap();
    let holder = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(150));
        drop(first);
    });
    let second = RedbStore::open(&path);
    holder.join().unwrap();
    assert!(second.is_ok(), "the retry outlasts a short-lived writer");
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
    for stored in replica.store().events().unwrap() {
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
                create_with_body("task", &format!("task {index}"), "see #zzzz and @joshua")
            }
            (3..=5, Some(item)) => edit(item, Change::Title(format!("renamed {index}"))),
            (6..=7, Some(item)) => comment(item, "progress note @joshua"),
            (_, Some(item)) => transition(item, "in_progress"),
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
    let replica = Replica::open(store).unwrap();
    let first_open = started.elapsed();
    assert_eq!(replica.state(), &state);
    drop(replica);

    let started = Instant::now();
    let replica = Replica::open(open(&directory)).unwrap();
    let with_snapshot = started.elapsed();
    assert_eq!(replica.state().event_count(), 100_001);
    eprintln!("first open (full fold): {first_open:?}, open with snapshot: {with_snapshot:?}");
    assert!(with_snapshot.as_millis() < 100, "{with_snapshot:?}");
}

//! Syncing a forked workforce: the history it was forked from is fetched by id.

mod common;

use common::SharedReplica;

use wf_core::event::{Change, Event};
use wf_core::identity::PrincipalSigner;
use wf_core::testing::*;
use wf_net::{SyncLog, initiate, read_open, respond};
use wf_store::{EventStore, RedbStore, Replica};

fn replica(workforce: wf_core::id::WorkforceId) -> SharedReplica<RedbStore> {
    SharedReplica::new(Replica::open(RedbStore::in_memory().unwrap(), workforce).unwrap())
}

/// Syncs `from` into `to`, and returns what each side did.
async fn sync(
    from: &SharedReplica<RedbStore>,
    to: &SharedReplica<RedbStore>,
    workforce: wf_core::id::WorkforceId,
) -> (wf_net::SyncReport, wf_net::SyncReport) {
    let (left, right) = tokio::io::duplex(8192);
    let (first, second) = tokio::join!(initiate(left, workforce, from), async {
        let mut right = right;
        assert_eq!(read_open(&mut right).await.unwrap(), workforce);
        respond(right, to).await
    });
    (first.unwrap(), second.unwrap())
}

fn encoded(replica: &SharedReplica<RedbStore>) -> Vec<u8> {
    wf_core::encoding::encode(replica.lock().state()).unwrap()
}

struct Forked {
    parent: wf_core::state::State,
    fixture: Fixture,
    history: Vec<Event>,
    forker: SharedReplica<RedbStore>,
    child: wf_core::id::WorkforceId,
}

fn forked(history_len: usize) -> Forked {
    let fixture = Fixture::new(2);
    let mut parent = fixture.state();
    let mut history = vec![fixture.genesis.clone()];
    for index in 0..history_len {
        history.push(fixture.apply(
            &mut parent,
            index % 2,
            2_000 + index as u64,
            create("task", &format!("task {index}")),
        ));
    }
    let first = parent.items().next().expect("an item exists").id();
    history.push(fixture.apply(
        &mut parent,
        0,
        50_000,
        edit(first, Change::Title("edited in the parent".into())),
    ));
    let founder = PrincipalSigner::from_bytes(&[1; 32]).unwrap();
    let genesis = fixture.fork(&parent, &founder, 90_000);
    let child = genesis.id().into();

    // The forker holds the child and, newest first, the history it names.
    let forker = replica(child);
    forker.lock().receive(genesis).unwrap();
    let mut newest_first = history.clone();
    newest_first.sort_by_key(|event| std::cmp::Reverse((event.content.hlc, event.id())));
    for event in newest_first {
        forker.lock().receive(event).unwrap();
    }
    Forked {
        parent,
        fixture,
        history,
        forker,
        child,
    }
}

#[tokio::test]
async fn a_new_device_takes_in_a_fork_and_its_history() {
    let forked = forked(40);
    assert_eq!(
        forked.forker.lock().state().event_count(),
        forked.history.len() + 1
    );
    assert!(forked.forker.missing().await.unwrap().is_empty());

    let device = replica(forked.child);
    sync(&device, &forked.forker, forked.child).await;
    assert!(device.missing().await.unwrap().is_empty());
    assert_eq!(encoded(&device), encoded(&forked.forker));
    assert_eq!(device.lock().state().items().count(), 40);
    let items = |state: &wf_core::state::State| -> Vec<(String, Option<wf_core::item::Priority>)> {
        state
            .live_items()
            .map(|item| (item.title().to_string(), item.priority()))
            .collect()
    };
    assert_eq!(items(device.lock().state()), items(&forked.parent));
    assert!(
        items(device.lock().state())
            .iter()
            .any(|(title, _)| title == "edited in the parent")
    );
    assert_eq!(device.lock().state().heads().len(), 1);
}

#[tokio::test]
async fn the_fork_continues_on_every_device_after_syncing() {
    let forked = forked(10);
    let laptop = replica(forked.child);
    sync(&laptop, &forked.forker, forked.child).await;

    // The forker writes on in the fork, continuing its feed from before.
    forked
        .forker
        .lock()
        .author(
            &forked.fixture.devices[0],
            forked.fixture.founder_actor(),
            200_000,
            create("task", "written in the fork"),
        )
        .unwrap();
    sync(&laptop, &forked.forker, forked.child).await;
    assert_eq!(encoded(&laptop), encoded(&forked.forker));
    assert_eq!(laptop.lock().state().equivocations().count(), 0);
    assert_eq!(laptop.lock().state().items().count(), 11);
}

#[tokio::test]
async fn the_history_is_served_only_when_asked_for_by_id() {
    let forked = forked(10);
    // What the fork would send unprompted contains only events written in the fork.
    let theirs = std::collections::BTreeMap::new();
    let sent = wf_net::replica::events_after(&forked.forker.lock(), &theirs).unwrap();
    assert_eq!(sent.len(), 1, "just the genesis of the fork");

    let head = *forked
        .forker
        .lock()
        .state()
        .fork_point()
        .unwrap()
        .1
        .first()
        .unwrap();
    let served = forked.forker.events_by_id(&[head]).await.unwrap();
    assert_eq!(
        served.len(),
        forked.history.len(),
        "the head and everything behind it"
    );
    assert!(
        served
            .windows(2)
            .all(|pair| pair[0].content.hlc > pair[1].content.hlc),
        "newest first"
    );
}

#[tokio::test]
async fn a_forked_replica_replays_from_its_log() {
    let forked = forked(20);
    let expected = encoded(&forked.forker);
    let log: Vec<Event> = {
        let replica = forked.forker.lock();
        replica.store().log_since(0).unwrap()
    };
    let mut rebuilt = Replica::open(RedbStore::in_memory().unwrap(), forked.child).unwrap();
    // Replaying in the order they were applied: newest history first.
    for event in log {
        rebuilt.receive(event).unwrap();
    }
    assert_eq!(
        wf_core::encoding::encode(rebuilt.state()).unwrap(),
        expected
    );
}

/// A replica of the parent workforce holding the first `count` events of the history.
fn parent_with(forked: &Forked, count: usize) -> Replica<RedbStore> {
    let mut parent =
        Replica::open(RedbStore::in_memory().unwrap(), forked.fixture.workforce()).unwrap();
    for event in &forked.history[..count] {
        parent.receive(event.clone()).unwrap();
    }
    parent
}

#[tokio::test]
async fn a_device_that_has_the_parent_takes_in_the_history_from_it() {
    let forked = forked(40);
    let device = replica(forked.child);
    device
        .lock()
        .set_history_source(Box::new(parent_with(&forked, forked.history.len())));

    let (_, forker) = sync(&device, &forked.forker, forked.child).await;
    assert_eq!(
        forker.sent, 1,
        "only the genesis of the fork crossed the network"
    );
    assert!(device.missing().await.unwrap().is_empty());
    assert_eq!(encoded(&device), encoded(&forked.forker));
}

#[tokio::test]
async fn a_device_behind_the_fork_point_gets_the_rest_from_a_peer() {
    let forked = forked(40);
    let device = replica(forked.child);
    device
        .lock()
        .set_history_source(Box::new(parent_with(&forked, 10)));

    let (_, forker) = sync(&device, &forked.forker, forked.child).await;
    assert!(forker.sent > 1, "the history it lacked came from the peer");
    assert!(device.missing().await.unwrap().is_empty());
    assert_eq!(encoded(&device), encoded(&forked.forker));
}

#[tokio::test]
async fn a_device_without_the_parent_gets_all_history_from_a_peer() {
    let forked = forked(40);
    let device = replica(forked.child);
    let (_, forker) = sync(&device, &forked.forker, forked.child).await;
    assert_eq!(
        forker.sent,
        forked.forker.lock().state().event_count(),
        "the genesis of the fork and the whole history"
    );
    assert_eq!(encoded(&device), encoded(&forked.forker));
}

#[tokio::test]
async fn a_source_that_holds_another_workforce_is_ignored() {
    let forked = forked(10);
    let device = replica(forked.child);
    let other = Fixture::new(3);
    let unrelated = Replica::open(RedbStore::in_memory().unwrap(), other.workforce()).unwrap();
    device.lock().set_history_source(Box::new(unrelated));
    sync(&device, &forked.forker, forked.child).await;
    assert_eq!(encoded(&device), encoded(&forked.forker));
}

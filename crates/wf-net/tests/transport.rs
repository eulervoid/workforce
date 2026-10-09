//! Real iroh endpoints in one process over loopback, with relays and address lookup off.

use wf_core::identity::DeviceSigner;
use wf_core::testing::*;
use wf_net::{Network, Node, SharedReplica};
use wf_store::{RedbStore, Replica};

fn replica(fixture: &Fixture) -> SharedReplica<RedbStore> {
    let mut replica = Replica::open(RedbStore::in_memory().unwrap()).unwrap();
    replica.receive(fixture.genesis.clone()).unwrap();
    SharedReplica::new(replica)
}

fn write(replica: &SharedReplica<RedbStore>, fixture: &Fixture, device: usize, title: &str) {
    let now = 2_000 + replica.lock().state().event_count() as u64;
    replica
        .lock()
        .author(
            &fixture.devices[device],
            fixture.founder_actor(),
            now,
            create("task", title),
        )
        .unwrap();
}

async fn node(
    fixture: &Fixture,
    signer: &DeviceSigner,
    replica: &SharedReplica<RedbStore>,
) -> Node<SharedReplica<RedbStore>> {
    let node = Node::start(signer, Network::Direct).await.unwrap();
    node.serve(fixture.workforce(), replica.clone());
    node
}

#[tokio::test]
async fn two_endpoints_exchange_their_writes() {
    let fixture = Fixture::new(3);
    let (first, second, third) = (replica(&fixture), replica(&fixture), replica(&fixture));
    write(&first, &fixture, 0, "from the laptop");
    write(&second, &fixture, 1, "from the vm");
    let laptop = node(&fixture, &fixture.devices[0], &first).await;
    let vm = node(&fixture, &fixture.devices[1], &second).await;
    let desktop = node(&fixture, &fixture.devices[2], &third).await;

    let report = laptop
        .sync_with(fixture.workforce(), vm.local_addr())
        .await
        .unwrap();
    assert_eq!((report.applied, report.sent), (1, 1));
    desktop
        .sync_with(fixture.workforce(), laptop.local_addr())
        .await
        .unwrap();

    let state = |replica: &SharedReplica<RedbStore>| replica.lock().state().clone();
    assert_eq!(state(&first), state(&second));
    assert_eq!(state(&first), state(&third));
    assert_eq!(state(&third).items().count(), 2);

    for node in [laptop, vm, desktop] {
        node.shutdown().await;
    }
}

#[tokio::test]
async fn devices_that_are_not_bound_are_refused() {
    let fixture = Fixture::new(1);
    let served = replica(&fixture);
    write(&served, &fixture, 0, "private");
    let member = node(&fixture, &fixture.devices[0], &served).await;

    let stranger_signer = DeviceSigner::from_bytes(&[200; 32]).unwrap();
    let stranger_replica = replica(&fixture);
    let stranger = node(&fixture, &stranger_signer, &stranger_replica).await;
    let result = stranger
        .sync_with(fixture.workforce(), member.local_addr())
        .await;
    assert!(result.is_err());
    assert_eq!(stranger_replica.lock().state().event_count(), 1);

    stranger.shutdown().await;
    member.shutdown().await;
}

#[tokio::test]
async fn an_unserved_workforce_is_refused() {
    let fixture = Fixture::new(2);
    let first = replica(&fixture);
    let second = replica(&fixture);
    let laptop = node(&fixture, &fixture.devices[0], &first).await;
    let vm = Node::start(&fixture.devices[1], Network::Direct)
        .await
        .unwrap();
    vm.serve(fixture.workforce(), second);
    let other = wf_core::id::WorkforceId::from(wf_core::id::EventId::of(b"other"));
    assert!(laptop.sync_with(other, vm.local_addr()).await.is_err());
    laptop
        .sync_with(fixture.workforce(), vm.local_addr())
        .await
        .unwrap();
    laptop.shutdown().await;
    vm.shutdown().await;
}

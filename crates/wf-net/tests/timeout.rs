//! A peer that stops answering does not hold a session forever.

mod common;

use common::SharedReplica;

use std::time::Duration;

use wf_core::testing::*;
use wf_net::{SyncError, initiate_within, respond_within};
use wf_store::{RedbStore, Replica};

fn replica(fixture: &Fixture) -> SharedReplica<RedbStore> {
    let mut replica = Replica::open(RedbStore::in_memory().unwrap(), fixture.workforce()).unwrap();
    replica.receive(fixture.genesis.clone()).unwrap();
    SharedReplica::new(replica)
}

#[tokio::test]
async fn an_initiator_gives_up_on_a_peer_that_goes_silent() {
    let fixture = Fixture::new(1);
    let log = replica(&fixture);
    let (left, _silent) = tokio::io::duplex(8192);
    let result = initiate_within(left, fixture.workforce(), &log, Duration::from_millis(100)).await;
    assert!(matches!(result, Err(SyncError::TimedOut)), "{result:?}");
}

#[tokio::test]
async fn a_responder_gives_up_on_a_peer_that_goes_silent() {
    let fixture = Fixture::new(1);
    let log = replica(&fixture);
    let (_silent, right) = tokio::io::duplex(8192);
    let result = respond_within(right, &log, Duration::from_millis(100)).await;
    assert!(matches!(result, Err(SyncError::TimedOut)), "{result:?}");
}

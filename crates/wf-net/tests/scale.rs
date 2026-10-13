//! Joining a workforce with 100k events over loopback. Slow in debug builds:
//! `cargo test --release -p wf-net --test scale -- --ignored --nocapture`

mod common;

use common::SharedReplica;

use std::time::Instant;

use wf_core::event::{Change, DeviceBound, EventBody};
use wf_core::identity::DeviceSigner;
use wf_core::invite::InviteCode;
use wf_core::state::Trust;
use wf_core::testing::*;
use wf_net::{Network, Node};
use wf_store::{Commit, EventStore, RedbStore, Replica};

#[tokio::test]
#[ignore = "slow; run in release mode"]
async fn joining_a_workforce_with_100k_events_takes_seconds() {
    let directory = tempfile::TempDir::new().unwrap();
    let fixture = Fixture::new(1);
    let store = RedbStore::open(directory.path().join("store.redb")).unwrap();
    let mut state = fixture.state();
    let mut batch = vec![fixture.genesis.clone()];
    let mut items = Vec::new();
    for index in 0..100_000usize {
        let body = match (index % 10, items.last().copied()) {
            (0..=2, _) | (_, None) => create_with_body("task", &format!("task {index}"), "body"),
            (3..=5, Some(item)) => edit(item, Change::Title(format!("renamed {index}"))),
            (6..=7, Some(item)) => comment(item, "progress note"),
            (_, Some(item)) => moved(item, "in_progress"),
        };
        let event = fixture.author(&state, 0, 2_000 + index as u64, body);
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
    let (invite, secret) = fixture.invite(&state, 200_000, u64::MAX / 2);
    batch.push(invite.clone());
    store
        .commit(Commit {
            append: &batch,
            ..Commit::default()
        })
        .unwrap();
    let member_replica = SharedReplica::new(Replica::open(store, fixture.workforce()).unwrap());

    let member = Node::start(&fixture.devices[0], Network::Direct)
        .await
        .unwrap();
    member.serve(fixture.workforce(), member_replica.clone());
    let code = InviteCode::new(
        fixture.workforce(),
        invite.id(),
        &secret,
        vec![member.device()],
    );

    let vm = DeviceSigner::from_bytes(&[90; 32]).unwrap();
    let local = SharedReplica::new(
        Replica::open(RedbStore::in_memory().unwrap(), fixture.workforce()).unwrap(),
    );
    let joiner = Node::start(&vm, Network::Direct).await.unwrap();
    let authoring = local.clone();
    let proof = code.prove(vm.key()).unwrap();
    let signer = vm.clone();
    let actor = fixture.founder_actor();
    let invite_id = invite.id();

    let started = Instant::now();
    joiner
        .join(&code, member.local_addr(), &local, move || {
            std::future::ready(
                authoring
                    .lock()
                    .author(
                        &signer,
                        actor,
                        300_000,
                        EventBody::DeviceBound(DeviceBound {
                            invite: invite_id,
                            proof,
                        }),
                    )
                    .map(|_| ())
                    .map_err(Into::into),
            )
        })
        .await
        .unwrap();
    let elapsed = started.elapsed();
    eprintln!("joined with 100k events in {elapsed:?}");

    assert_eq!(local.lock().state().event_count(), 100_003);
    assert_eq!(member_replica.lock().state().event_count(), 100_003);
    assert!(local.lock().store().log_len().unwrap() > 100_000);
    assert!(elapsed.as_secs() < 10, "{elapsed:?}");
}

//! Logs larger than a frame sync; peers that break the protocol are refused.

mod common;

use common::SharedReplica;

use serde::Serialize;
use tokio::io::{AsyncWriteExt, DuplexStream};
use wf_core::event::{Event, MAX_EVENT_LEN};
use wf_core::id::EventId;
use wf_core::sync::Versions;
use wf_core::testing::*;
use wf_net::frame::{FrameError, MAX_FRAME_LEN, write_frame};
use wf_net::{IDLE_TIMEOUT, SyncError, initiate, read_open, respond};
use wf_store::{RedbStore, Replica};

/// The messages of the sync protocol in the order `wf_net` declares them, for a peer that does
/// not follow it. A change of the protocol's messages must change this too.
#[derive(Serialize, serde::Deserialize)]
enum Wire {
    Open(wf_core::id::WorkforceId),
    Versions(Versions),
    Events(Vec<Event>),
    EndOfEvents,
    Want(Vec<EventId>),
    EndOfWants,
    Have(Vec<Event>),
    EndOfHave,
}

fn replica(fixture: &Fixture) -> SharedReplica<RedbStore> {
    let mut replica = Replica::open(RedbStore::in_memory().unwrap(), fixture.workforce()).unwrap();
    replica.receive(fixture.genesis.clone()).unwrap();
    SharedReplica::new(replica)
}

/// Syncs `count` events with a body of `body_len` bytes each between two replicas and checks
/// that all of them arrive.
async fn sync_log_of(count: usize, body_len: usize) {
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let full = replica(&fixture);
    let empty = replica(&fixture);
    let text = "x".repeat(body_len);
    for index in 0..count {
        let event = fixture.apply(
            &mut state,
            0,
            2_000 + index as u64,
            create_with_body("task", &format!("task {index}"), &text),
        );
        full.lock().receive(event).unwrap();
    }

    let (left, mut right) = tokio::io::duplex(256 * 1024);
    let (pulled, served) = tokio::join!(initiate(left, fixture.workforce(), &empty), async {
        read_open(&mut right, IDLE_TIMEOUT).await.unwrap();
        respond(right, &full).await
    });
    assert_eq!(pulled.unwrap().applied, count);
    assert_eq!(served.unwrap().sent, count);
    assert_eq!(empty.lock().state().event_count(), count + 1);
}

#[tokio::test]
async fn a_log_of_many_events_syncs_in_several_frames() {
    sync_log_of(600, 10).await;
}

#[tokio::test]
async fn a_few_large_events_sync_in_several_frames() {
    sync_log_of(20, 900 * 1024).await;
}

/// Takes 30-50s in debug, so it only runs on request: `cargo test -- --ignored`. The two tests
/// above cover splitting by count and by bytes; this one proves the whole log exceeds
/// `MAX_FRAME_LEN`.
#[tokio::test]
#[ignore = "slow: moves more than MAX_FRAME_LEN bytes"]
async fn a_log_larger_than_a_frame_syncs_in_several() {
    let body_len = MAX_EVENT_LEN - 1000;
    let count = MAX_FRAME_LEN as usize / body_len + 1;
    assert!(count * body_len > MAX_FRAME_LEN as usize);
    sync_log_of(count, body_len).await;
}

/// A responder that has read the open message of a peer that then speaks for itself.
async fn rogue_peer(
    fixture: &Fixture,
) -> (
    DuplexStream,
    tokio::task::JoinHandle<Result<wf_net::SyncReport, SyncError>>,
) {
    let log = replica(fixture);
    let (rogue, mut right) = tokio::io::duplex(1024 * 1024);
    let responder = tokio::spawn(async move {
        read_open(&mut right, IDLE_TIMEOUT).await?;
        respond(right, &log).await
    });
    (rogue, responder)
}

#[tokio::test]
async fn a_peer_that_sends_the_wrong_message_is_refused() {
    let fixture = Fixture::new(1);
    let (mut rogue, responder) = rogue_peer(&fixture).await;
    write_frame(&mut rogue, &Wire::Open(fixture.workforce()))
        .await
        .unwrap();
    write_frame(&mut rogue, &Wire::EndOfEvents).await.unwrap();
    let result = responder.await.unwrap();
    assert!(
        matches!(
            result,
            Err(SyncError::Unexpected {
                expected: "versions",
                got: "end of events"
            })
        ),
        "{result:?}"
    );
}

#[tokio::test]
async fn a_peer_that_asks_for_too_many_events_at_once_is_refused() {
    let fixture = Fixture::new(1);
    let (mut rogue, responder) = rogue_peer(&fixture).await;
    write_frame(&mut rogue, &Wire::Open(fixture.workforce()))
        .await
        .unwrap();
    write_frame(&mut rogue, &Wire::Versions(Versions::new()))
        .await
        .unwrap();
    write_frame(&mut rogue, &Wire::EndOfEvents).await.unwrap();
    let ids = (0..1100u32)
        .map(|index| EventId::of(&index.to_be_bytes()))
        .collect();
    write_frame(&mut rogue, &Wire::Want(ids)).await.unwrap();
    let result = responder.await.unwrap();
    assert!(
        matches!(result, Err(SyncError::WantTooLarge(1100))),
        "{result:?}"
    );
}

#[tokio::test]
async fn a_peer_cannot_make_a_small_message_large() {
    let fixture = Fixture::new(1);
    let (mut rogue, responder) = rogue_peer(&fixture).await;
    write_frame(&mut rogue, &Wire::Open(fixture.workforce()))
        .await
        .unwrap();
    // Far below the limit of frames, far above the one of versions.
    rogue
        .write_all(&(8 * 1024 * 1024u32).to_be_bytes())
        .await
        .unwrap();
    let result = responder.await.unwrap();
    assert!(
        matches!(result, Err(SyncError::Frame(FrameError::TooLarge(_)))),
        "{result:?}"
    );
}

#[tokio::test]
async fn an_open_message_is_small() {
    let (mut rogue, mut right) = tokio::io::duplex(1024 * 1024);
    let mut huge = Vec::from(4096u32.to_be_bytes());
    huge.extend([0; 4096]);
    rogue.write_all(&huge).await.unwrap();
    let result = read_open(&mut right, IDLE_TIMEOUT).await;
    assert!(
        matches!(result, Err(SyncError::Frame(FrameError::TooLarge(_)))),
        "{result:?}"
    );
}

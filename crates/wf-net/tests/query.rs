//! `wf/query/1` in memory, and between two real endpoints.

mod common;

use common::SharedReplica;

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::Arc;
use std::time::Duration;

use n0_future::boxed::BoxFuture;
use wf_core::id::{EventId, ItemId, SessionId, WorkforceId};
use wf_core::identity::DeviceSigner;
use wf_core::session::{CommitId, GitOid};
use wf_core::testing::*;
use wf_net::frame::{FrameError, MAX_FRAME_LEN};
use wf_net::{
    DiffAnswer, DiffRequest, Network, Node, Query, QueryError, QuerySource, SourceError,
    TRANSCRIPT_PART_LEN, TranscriptAnswer, TranscriptRequest, Work, query_initiate,
    query_initiate_within, query_respond, query_respond_within, read_query,
};
use wf_store::{RedbStore, Replica};

fn item() -> ItemId {
    ItemId::from(EventId::of(b"item"))
}

fn commit(byte: u8) -> CommitId {
    CommitId::Git(GitOid::from_bytes([byte; 20]))
}

fn work(head: CommitId) -> Work {
    Work {
        target: "main".into(),
        head,
        committed: "diff --git a/src/lib.rs b/src/lib.rs\n@@ -1 +1 @@ fn main\n-old\n+new\n".into(),
    }
}

/// The work of the item in the agent repository, if the device has one, and the transcripts of
/// the sessions of the device, with whether each ended.
#[derive(Default)]
struct AgentRepository {
    work: Option<Work>,
    transcripts: HashMap<SessionId, (Vec<u8>, bool)>,
}

impl AgentRepository {
    fn with_work(work: Option<Work>) -> Self {
        Self {
            work,
            ..Self::default()
        }
    }

    fn with_transcript(session: SessionId, transcript: &[u8], ended: bool) -> Self {
        Self {
            transcripts: [(session, (transcript.to_vec(), ended))].into(),
            ..Self::default()
        }
    }
}

impl QuerySource for AgentRepository {
    fn diff(
        &self,
        _: WorkforceId,
        _: ItemId,
        since: Option<CommitId>,
    ) -> BoxFuture<Result<DiffAnswer, SourceError>> {
        let answer = match &self.work {
            None => DiffAnswer::NotHere,
            Some(work) if since == Some(work.head) => DiffAnswer::Unchanged,
            Some(work) => DiffAnswer::Work(work.clone()),
        };
        Box::pin(async move { Ok(answer) })
    }

    fn transcript(
        &self,
        _: WorkforceId,
        session: SessionId,
        from: u64,
        limit: usize,
    ) -> BoxFuture<Result<TranscriptAnswer, SourceError>> {
        let answer = match self.transcripts.get(&session) {
            None => Ok(TranscriptAnswer::NotHere),
            Some((transcript, ended)) => {
                TranscriptAnswer::read(Cursor::new(transcript.clone()), from, limit, *ended)
                    .map_err(SourceError::from)
            }
        };
        Box::pin(async move { answer })
    }
}

fn request(workforce: WorkforceId, since: Option<CommitId>) -> DiffRequest {
    DiffRequest {
        workforce,
        item: item(),
        since,
    }
}

/// Runs the request against `source` over an in-memory stream.
async fn ask<R: wf_net::QueryRequest>(source: &AgentRepository, asked: R) -> R::Answer {
    let (left, mut right) = tokio::io::duplex(8192);
    let expected: Query = asked.clone().into();
    let responder = async {
        let request = read_query(&mut right, Duration::from_secs(5)).await?;
        assert_eq!(request, expected);
        query_respond(right, request, source).await
    };
    let (answer, served) = tokio::join!(query_initiate(left, &asked), responder);
    served.unwrap();
    answer.unwrap()
}

async fn ask_diff(source: &AgentRepository, since: Option<CommitId>) -> DiffAnswer {
    ask(source, request(Fixture::new(1).workforce(), since)).await
}

fn session(byte: u8) -> SessionId {
    SessionId::from_bytes([byte; 32])
}

async fn ask_transcript(
    source: &AgentRepository,
    session: SessionId,
    from: u64,
) -> TranscriptAnswer {
    let request = TranscriptRequest {
        workforce: Fixture::new(1).workforce(),
        session,
        from,
    };
    ask(source, request).await
}

#[tokio::test]
async fn a_diff_is_answered_with_the_work() {
    let source = AgentRepository::with_work(Some(work(commit(1))));
    assert_eq!(
        ask_diff(&source, None).await,
        DiffAnswer::Work(work(commit(1)))
    );
    assert_eq!(
        ask_diff(&source, Some(commit(2))).await,
        DiffAnswer::Work(work(commit(1)))
    );
}

#[tokio::test]
async fn the_same_head_is_unchanged() {
    let source = AgentRepository::with_work(Some(work(commit(1))));
    assert_eq!(
        ask_diff(&source, Some(commit(1))).await,
        DiffAnswer::Unchanged
    );
}

#[tokio::test]
async fn an_item_without_work_here_is_not_here() {
    assert_eq!(
        ask_diff(&AgentRepository::with_work(None), None).await,
        DiffAnswer::NotHere
    );
}

#[tokio::test]
async fn a_large_diff_fits_the_answer() {
    let mut big = work(commit(1));
    big.committed = "+".to_string() + &"x".repeat(100) + "\n";
    big.committed = big.committed.repeat(200_000);
    let source = AgentRepository::with_work(Some(big.clone()));
    assert_eq!(ask_diff(&source, None).await, DiffAnswer::Work(big));
}

#[tokio::test]
async fn work_above_the_largest_answer_is_told_apart_from_a_broken_connection() {
    let mut huge = work(commit(1));
    huge.committed = "x".repeat(MAX_FRAME_LEN as usize + 1);
    let source = AgentRepository::with_work(Some(huge));
    assert_eq!(ask_diff(&source, None).await, DiffAnswer::TooLarge);
}

const TRANSCRIPT: &[u8] = b"{\"type\":\"user\"}\n{\"type\":\"assistant\"}\n";

#[tokio::test]
async fn a_transcript_is_answered_from_the_offset_asked() {
    let source = AgentRepository::with_transcript(session(1), TRANSCRIPT, false);
    assert_eq!(
        ask_transcript(&source, session(1), 0).await,
        TranscriptAnswer::Part(TRANSCRIPT.to_vec())
    );
    assert_eq!(
        ask_transcript(&source, session(1), 17).await,
        TranscriptAnswer::Part(TRANSCRIPT[17..].to_vec())
    );
    // A live session's transcript is caught up with, and may grow.
    assert_eq!(
        ask_transcript(&source, session(1), TRANSCRIPT.len() as u64).await,
        TranscriptAnswer::Part(Vec::new())
    );
}

#[tokio::test]
async fn a_long_transcript_comes_in_parts_of_the_limit() {
    let long: Vec<u8> = b"{\"type\":\"assistant\"}\n"
        .repeat(TRANSCRIPT_PART_LEN / 10)
        .into_iter()
        .take(TRANSCRIPT_PART_LEN * 2 + 5)
        .collect();
    let source = AgentRepository::with_transcript(session(1), &long, true);
    let mut copy = Vec::new();
    let last = loop {
        match ask_transcript(&source, session(1), copy.len() as u64).await {
            TranscriptAnswer::Part(bytes) => {
                assert_eq!(
                    bytes.len(),
                    TRANSCRIPT_PART_LEN,
                    "a part stops at the limit"
                );
                copy.extend(bytes);
            }
            TranscriptAnswer::Last(bytes) => break bytes,
            TranscriptAnswer::NotHere => panic!("the session is here"),
        }
    };
    assert_eq!(last.len(), 5);
    copy.extend(last);
    assert_eq!(copy, long);
}

#[tokio::test]
async fn the_transcript_of_an_ended_session_ends_with_its_last_bytes() {
    let source = AgentRepository::with_transcript(session(1), TRANSCRIPT, true);
    assert_eq!(
        ask_transcript(&source, session(1), 0).await,
        TranscriptAnswer::Last(TRANSCRIPT.to_vec())
    );
    assert_eq!(
        ask_transcript(&source, session(1), TRANSCRIPT.len() as u64).await,
        TranscriptAnswer::Last(Vec::new())
    );
    // A copy that is longer than the transcript is told that nothing follows it.
    assert_eq!(
        ask_transcript(&source, session(1), 1_000).await,
        TranscriptAnswer::Last(Vec::new())
    );
}

#[tokio::test]
async fn a_session_of_another_device_is_not_answered() {
    let source = AgentRepository::with_transcript(session(1), TRANSCRIPT, false);
    assert_eq!(
        ask_transcript(&source, session(2), 0).await,
        TranscriptAnswer::NotHere
    );
}

#[tokio::test(start_paused = true)]
async fn an_initiator_gives_up_on_a_peer_that_goes_silent() {
    let fixture = Fixture::new(1);
    let (left, _silent) = tokio::io::duplex(8192);
    let result = query_initiate_within(
        left,
        &request(fixture.workforce(), None),
        Duration::from_millis(100),
    )
    .await;
    assert!(
        matches!(result, Err(QueryError::Frame(FrameError::TimedOut))),
        "{result:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_responder_gives_up_on_a_peer_that_does_not_read() {
    let fixture = Fixture::new(1);
    // A buffer of one byte that nobody drains: the answer cannot be written.
    let (_silent, right) = tokio::io::duplex(1);
    let result = query_respond_within(
        right,
        request(fixture.workforce(), None).into(),
        &AgentRepository::with_work(Some(work(commit(1)))),
        Duration::from_millis(100),
    )
    .await;
    assert!(
        matches!(result, Err(QueryError::Frame(FrameError::TimedOut))),
        "{result:?}"
    );
}

fn replica(fixture: &Fixture) -> SharedReplica<RedbStore> {
    let mut replica = Replica::open(RedbStore::in_memory().unwrap(), fixture.workforce()).unwrap();
    replica.receive(fixture.genesis.clone()).unwrap();
    SharedReplica::new(replica)
}

async fn node(fixture: &Fixture, signer: &DeviceSigner) -> Node<SharedReplica<RedbStore>> {
    let node = Node::start(signer, Network::Direct).await.unwrap();
    node.serve(fixture.workforce(), replica(fixture));
    node
}

#[tokio::test]
async fn a_bound_device_is_answered_and_a_stranger_is_refused() {
    if wf_core::testing::skip_in_fence("iroh needs the route socket") {
        return;
    }
    let fixture = Fixture::new(2);
    let holder = node(&fixture, &fixture.devices[0]).await;
    holder.serve_query(Arc::new(AgentRepository::with_work(Some(work(commit(1))))));
    let member = node(&fixture, &fixture.devices[1]).await;
    let stranger = node(&fixture, &DeviceSigner::from_bytes(&[200; 32]).unwrap()).await;

    let answer = member
        .query_diff(holder.local_addr(), fixture.workforce(), item(), None)
        .await
        .unwrap();
    assert_eq!(answer, DiffAnswer::Work(work(commit(1))));
    let answer = member
        .query_diff(
            holder.local_addr(),
            fixture.workforce(),
            item(),
            Some(commit(1)),
        )
        .await
        .unwrap();
    assert_eq!(answer, DiffAnswer::Unchanged);

    let refused = stranger
        .query_diff(holder.local_addr(), fixture.workforce(), item(), None)
        .await;
    let error = refused.expect_err("a stranger is refused");
    assert!(
        format!("{error:?}").contains("403"),
        "not refused as a non-member: {error:?}"
    );

    for node in [holder, member, stranger] {
        node.shutdown().await;
    }
}

#[tokio::test]
async fn a_bound_device_follows_a_transcript_and_a_stranger_is_refused() {
    if wf_core::testing::skip_in_fence("iroh needs the route socket") {
        return;
    }
    let fixture = Fixture::new(2);
    let holder = node(&fixture, &fixture.devices[0]).await;
    holder.serve_query(Arc::new(AgentRepository::with_transcript(
        session(1),
        TRANSCRIPT,
        true,
    )));
    let member = node(&fixture, &fixture.devices[1]).await;
    let stranger = node(&fixture, &DeviceSigner::from_bytes(&[200; 32]).unwrap()).await;

    let answer = member
        .query_transcript(holder.local_addr(), fixture.workforce(), session(1), 17)
        .await
        .unwrap();
    assert_eq!(answer, TranscriptAnswer::Last(TRANSCRIPT[17..].to_vec()));

    let refused = stranger
        .query_transcript(holder.local_addr(), fixture.workforce(), session(1), 0)
        .await;
    let error = refused.expect_err("a stranger is refused");
    assert!(
        format!("{error:?}").contains("403"),
        "not refused as a non-member: {error:?}"
    );

    for node in [holder, member, stranger] {
        node.shutdown().await;
    }
}

#[tokio::test]
async fn a_node_that_serves_no_query_refuses_instead_of_hanging() {
    if wf_core::testing::skip_in_fence("iroh needs the route socket") {
        return;
    }
    let fixture = Fixture::new(2);
    let holder = node(&fixture, &fixture.devices[0]).await;
    let member = node(&fixture, &fixture.devices[1]).await;

    let refused = tokio::time::timeout(
        Duration::from_secs(20),
        member.query_diff(holder.local_addr(), fixture.workforce(), item(), None),
    )
    .await
    .expect("the request is refused, not left hanging");
    let error = refused.expect_err("nothing is served");
    assert!(
        format!("{error:?}").contains("404"),
        "not refused as unserved: {error:?}"
    );

    for node in [holder, member] {
        node.shutdown().await;
    }
}

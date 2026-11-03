//! `wf/query/3` in memory, and between two real endpoints.

mod common;

use common::SharedReplica;

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::Arc;
use std::time::Duration;

use n0_future::boxed::BoxFuture;
use wf_core::id::{CommentId, EventId, ItemId, SessionId, WorkforceId};
use wf_core::identity::DeviceSigner;
use wf_core::session::{CommentSnapshot, CommitId, GitOid};
use wf_core::testing::*;
use wf_net::frame::{FrameError, MAX_FRAME_LEN};
use wf_net::{
    ChangesAnswer, ChangesRequest, DiffAnswer, DiffRequest, Network, Node, Query, QueryError,
    QuerySource, SnapshotRequest, SourceError, TRANSCRIPT_PART_LEN, TranscriptAnswer,
    TranscriptRequest, Uncommitted, Work, WorkAt, query_initiate, query_initiate_within,
    query_respond, query_respond_within, read_query,
};
use wf_store::{RedbStore, Replica};

fn item() -> ItemId {
    ItemId::from(EventId::of(b"item"))
}

fn commit(byte: u8) -> CommitId {
    CommitId::Git(GitOid::from_bytes([byte; 20]))
}

fn tree(byte: u8) -> GitOid {
    GitOid::from_bytes([byte; 20])
}

/// Work at `head` whose latest snapshot is `snapshot`.
fn work_at(head: CommitId, snapshot: Option<GitOid>) -> Work {
    Work {
        target: "main".into(),
        head,
        committed: "diff --git a/src/lib.rs b/src/lib.rs\n@@ -1 +1 @@ fn main\n-old\n+new\n".into(),
        uncommitted: snapshot.map(|snapshot| Uncommitted {
            snapshot,
            diff: "diff --git a/notes.md b/notes.md\n@@ -0,0 +1 @@\n+draft\n".into(),
        }),
    }
}

fn work(head: CommitId) -> Work {
    work_at(head, Some(tree(9)))
}

/// The work of the item in the agent repository, if the device has one, the work at the snapshots
/// of its sessions, and the transcripts of the sessions of the device, with whether each ended.
#[derive(Default)]
struct AgentRepository {
    work: Option<Work>,
    snapshots: Vec<(CommentSnapshot, Work)>,
    /// What each snapshot of a session changed against the one before, by session, the one
    /// before and the snapshot.
    changes: Vec<((SessionId, GitOid, GitOid), String)>,
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
        since: Option<WorkAt>,
    ) -> BoxFuture<Result<DiffAnswer, SourceError>> {
        let answer = match &self.work {
            None => DiffAnswer::NotHere,
            Some(work) if since == Some(work.at()) => DiffAnswer::Unchanged,
            Some(work) => DiffAnswer::Work(work.clone()),
        };
        Box::pin(async move { Ok(answer) })
    }

    fn snapshot(
        &self,
        _: WorkforceId,
        _: ItemId,
        at: CommentSnapshot,
    ) -> BoxFuture<Result<DiffAnswer, SourceError>> {
        let answer = match self.snapshots.iter().find(|(taken, _)| *taken == at) {
            None => DiffAnswer::NotHere,
            Some((_, work)) => DiffAnswer::Work(work.clone()),
        };
        Box::pin(async move { Ok(answer) })
    }

    fn changes(&self, request: ChangesRequest) -> BoxFuture<Result<ChangesAnswer, SourceError>> {
        let asked = (request.session, request.against, request.snapshot);
        let answer = match self.changes.iter().find(|(recorded, _)| *recorded == asked) {
            None => ChangesAnswer::NotHere,
            Some((_, diff)) => ChangesAnswer::Changes(diff.clone()),
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

fn request(workforce: WorkforceId, since: Option<WorkAt>) -> DiffRequest {
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

async fn ask_diff(source: &AgentRepository, since: Option<WorkAt>) -> DiffAnswer {
    ask(source, request(Fixture::new(1).workforce(), since)).await
}

fn taken_at(byte: u8) -> CommentSnapshot {
    CommentSnapshot::Taken {
        session: session(1),
        comment: CommentId::from(EventId::of(&[byte])),
    }
}

fn parked_on(byte: u8) -> CommentSnapshot {
    CommentSnapshot::Parked {
        session: session(1),
        blocker: CommentId::from(EventId::of(&[byte])),
    }
}

async fn ask_snapshot(source: &AgentRepository, at: CommentSnapshot) -> DiffAnswer {
    let request = SnapshotRequest {
        workforce: Fixture::new(1).workforce(),
        item: item(),
        at,
    };
    ask(source, request).await
}

fn with_snapshots(snapshots: Vec<(CommentSnapshot, Work)>) -> AgentRepository {
    AgentRepository {
        snapshots,
        ..AgentRepository::default()
    }
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
        ask_diff(&source, Some(work(commit(2)).at())).await,
        DiffAnswer::Work(work(commit(1)))
    );
}

#[tokio::test]
async fn the_same_head_and_snapshot_are_unchanged() {
    let source = AgentRepository::with_work(Some(work(commit(1))));
    assert_eq!(
        ask_diff(&source, Some(work(commit(1)).at())).await,
        DiffAnswer::Unchanged
    );
}

#[tokio::test]
async fn a_new_snapshot_on_the_same_head_is_answered_with_the_work() {
    let source = AgentRepository::with_work(Some(work_at(commit(1), Some(tree(2)))));
    for seen in [None, Some(tree(1))] {
        assert_eq!(
            ask_diff(&source, Some(work_at(commit(1), seen).at())).await,
            DiffAnswer::Work(work_at(commit(1), Some(tree(2))))
        );
    }
}

#[tokio::test]
async fn the_work_at_a_snapshot_is_answered_and_one_not_here_is_not_here() {
    let at_comment = work_at(commit(1), Some(tree(1)));
    let parked = work_at(commit(2), Some(tree(2)));
    let source = with_snapshots(vec![
        (taken_at(1), at_comment.clone()),
        (parked_on(2), parked.clone()),
    ]);
    assert_eq!(
        ask_snapshot(&source, taken_at(1)).await,
        DiffAnswer::Work(at_comment)
    );
    assert_eq!(
        ask_snapshot(&source, parked_on(2)).await,
        DiffAnswer::Work(parked)
    );
    for missing in [taken_at(2), parked_on(1)] {
        assert_eq!(ask_snapshot(&source, missing).await, DiffAnswer::NotHere);
    }
}

#[tokio::test]
async fn the_work_at_a_snapshot_above_the_largest_answer_is_too_large() {
    let mut huge = work(commit(1));
    huge.uncommitted.as_mut().unwrap().diff = "x".repeat(MAX_FRAME_LEN as usize + 1);
    let source = with_snapshots(vec![(taken_at(1), huge)]);
    assert_eq!(
        ask_snapshot(&source, taken_at(1)).await,
        DiffAnswer::TooLarge
    );
}

fn changes_of(against: u8, snapshot: u8) -> ChangesRequest {
    ChangesRequest {
        workforce: Fixture::new(1).workforce(),
        item: item(),
        session: session(1),
        against: tree(against),
        snapshot: tree(snapshot),
    }
}

#[tokio::test]
async fn what_a_snapshot_changed_is_answered_and_one_not_recorded_is_not_here() {
    let diff = "diff --git a/notes.md b/notes.md\n@@ -1 +1 @@\n-one\n+two\n".to_string();
    let source = AgentRepository {
        changes: vec![((session(1), tree(1), tree(2)), diff.clone())],
        ..AgentRepository::default()
    };
    assert_eq!(
        ask(&source, changes_of(1, 2)).await,
        ChangesAnswer::Changes(diff)
    );
    for missing in [changes_of(2, 1), changes_of(3, 2)] {
        assert_eq!(ask(&source, missing).await, ChangesAnswer::NotHere);
    }
    let huge = AgentRepository {
        changes: vec![(
            (session(1), tree(1), tree(2)),
            "x".repeat(MAX_FRAME_LEN as usize + 1),
        )],
        ..AgentRepository::default()
    };
    assert_eq!(ask(&huge, changes_of(1, 2)).await, ChangesAnswer::TooLarge);
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
    holder.serve_query(Arc::new(AgentRepository {
        work: Some(work(commit(1))),
        snapshots: vec![(taken_at(1), work_at(commit(1), Some(tree(1))))],
        ..AgentRepository::default()
    }));
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
            Some(work(commit(1)).at()),
        )
        .await
        .unwrap();
    assert_eq!(answer, DiffAnswer::Unchanged);
    let answer = member
        .query_snapshot(
            holder.local_addr(),
            fixture.workforce(),
            item(),
            taken_at(1),
        )
        .await
        .unwrap();
    assert_eq!(answer, DiffAnswer::Work(work_at(commit(1), Some(tree(1)))));

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

//! `wf/query/1` in memory, and between two real endpoints.

mod common;

use common::SharedReplica;

use std::sync::Arc;
use std::time::Duration;

use n0_future::boxed::BoxFuture;
use wf_core::diff::{Content, DiffLine, FileChange, FileDiff, Hunk};
use wf_core::id::{EventId, ItemId, WorkforceId};
use wf_core::identity::DeviceSigner;
use wf_core::session::{CommitId, GitOid};
use wf_core::testing::*;
use wf_net::frame::{FrameError, MAX_FRAME_LEN};
use wf_net::{
    DiffAnswer, Network, Node, Query, QueryError, QuerySource, SourceError, Work, query_initiate,
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
        committed: vec![FileDiff {
            path: "src/lib.rs".into(),
            change: FileChange::Modified,
            content: Content::Text(vec![Hunk {
                heading: "fn main".into(),
                lines: vec![
                    DiffLine::Removed {
                        old: 1,
                        text: "old".into(),
                    },
                    DiffLine::Added {
                        new: 1,
                        text: "new".into(),
                    },
                ],
            }]),
        }],
    }
}

/// The work of the item in the worktree, if the device has one.
struct Worktree(Option<Work>);

impl QuerySource for Worktree {
    fn diff(
        &self,
        _: WorkforceId,
        _: ItemId,
        since: Option<CommitId>,
    ) -> BoxFuture<Result<DiffAnswer, SourceError>> {
        let answer = match &self.0 {
            None => DiffAnswer::NotHere,
            Some(work) if since == Some(work.head) => DiffAnswer::Unchanged,
            Some(work) => DiffAnswer::Work(work.clone()),
        };
        Box::pin(async move { Ok(answer) })
    }
}

fn request(workforce: WorkforceId, since: Option<CommitId>) -> Query {
    Query::Diff {
        workforce,
        item: item(),
        since,
    }
}

/// Runs the request against `source` over an in-memory stream.
async fn ask(source: &Worktree, since: Option<CommitId>) -> DiffAnswer {
    let fixture = Fixture::new(1);
    let (left, mut right) = tokio::io::duplex(8192);
    let asked = request(fixture.workforce(), since);
    let responder = async {
        let request = read_query(&mut right, Duration::from_secs(5)).await?;
        assert_eq!(request, asked);
        query_respond(right, request, source).await
    };
    let (answer, served) = tokio::join!(query_initiate(left, &asked), responder);
    served.unwrap();
    answer.unwrap()
}

#[tokio::test]
async fn a_diff_is_answered_with_the_work() {
    let source = Worktree(Some(work(commit(1))));
    assert_eq!(ask(&source, None).await, DiffAnswer::Work(work(commit(1))));
    assert_eq!(
        ask(&source, Some(commit(2))).await,
        DiffAnswer::Work(work(commit(1)))
    );
}

#[tokio::test]
async fn the_same_head_is_unchanged() {
    let source = Worktree(Some(work(commit(1))));
    assert_eq!(ask(&source, Some(commit(1))).await, DiffAnswer::Unchanged);
}

#[tokio::test]
async fn an_item_without_work_here_is_not_here() {
    assert_eq!(ask(&Worktree(None), None).await, DiffAnswer::NotHere);
}

#[tokio::test]
async fn a_large_diff_fits_the_answer() {
    let mut big = work(commit(1));
    let line = DiffLine::Added {
        new: 1,
        text: "x".repeat(100),
    };
    big.committed = (0..2_000)
        .map(|index| FileDiff {
            path: format!("file{index}"),
            change: FileChange::Added,
            content: Content::Text(vec![Hunk {
                heading: String::new(),
                lines: vec![line.clone(); 100],
            }]),
        })
        .collect();
    let source = Worktree(Some(big.clone()));
    assert_eq!(ask(&source, None).await, DiffAnswer::Work(big));
}

#[tokio::test]
async fn work_above_the_largest_answer_is_told_apart_from_a_broken_connection() {
    let mut huge = work(commit(1));
    huge.committed[0].content = Content::Text(vec![Hunk {
        heading: String::new(),
        lines: vec![DiffLine::Added {
            new: 1,
            text: "x".repeat(MAX_FRAME_LEN as usize + 1),
        }],
    }]);
    let source = Worktree(Some(huge));
    assert_eq!(ask(&source, None).await, DiffAnswer::TooLarge);
}

#[tokio::test]
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

#[tokio::test]
async fn a_responder_gives_up_on_a_peer_that_does_not_read() {
    let fixture = Fixture::new(1);
    // A buffer of one byte that nobody drains: the answer cannot be written.
    let (_silent, right) = tokio::io::duplex(1);
    let result = query_respond_within(
        right,
        request(fixture.workforce(), None),
        &Worktree(Some(work(commit(1)))),
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
    holder.serve_query(Arc::new(Worktree(Some(work(commit(1))))));
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

//! `wf/peek/1` in memory, and between two real endpoints.

mod common;

use common::SharedReplica;

use std::sync::Arc;
use std::time::Duration;

use n0_future::boxed::BoxFuture;
use wf_core::id::{EventId, ItemId, WorkforceId};
use wf_core::identity::DeviceSigner;
use wf_core::session::{CommitId, GitOid};
use wf_core::testing::*;
use wf_net::frame::FrameError;
use wf_net::{
    Answer, Network, Node, PeekError, PeekSource, Request, SourceError, Work, peek_initiate,
    peek_initiate_within, peek_respond, peek_respond_within, read_request,
};
use wf_store::{RedbStore, Replica};
use wf_vcs::diff::{Content, DiffLine, FileChange, FileDiff, Hunk};

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

impl PeekSource for Worktree {
    fn diff(
        &self,
        _: WorkforceId,
        _: ItemId,
        since: Option<CommitId>,
    ) -> BoxFuture<Result<Answer, SourceError>> {
        let answer = match &self.0 {
            None => Answer::NotHere,
            Some(work) if since == Some(work.head) => Answer::Unchanged,
            Some(work) => Answer::Work(work.clone()),
        };
        Box::pin(async move { Ok(answer) })
    }
}

fn request(workforce: WorkforceId, since: Option<CommitId>) -> Request {
    Request::Diff {
        workforce,
        item: item(),
        since,
    }
}

/// Runs the request against `source` over an in-memory stream.
async fn ask(source: &Worktree, since: Option<CommitId>) -> Answer {
    let fixture = Fixture::new(1);
    let (left, mut right) = tokio::io::duplex(8192);
    let asked = request(fixture.workforce(), since);
    let responder = async {
        let request = read_request(&mut right, Duration::from_secs(5)).await?;
        assert_eq!(request, asked);
        peek_respond(right, request, source).await
    };
    let (answer, served) = tokio::join!(peek_initiate(left, &asked), responder);
    served.unwrap();
    answer.unwrap()
}

#[tokio::test]
async fn a_diff_is_answered_with_the_work() {
    let source = Worktree(Some(work(commit(1))));
    assert_eq!(ask(&source, None).await, Answer::Work(work(commit(1))));
    assert_eq!(
        ask(&source, Some(commit(2))).await,
        Answer::Work(work(commit(1)))
    );
}

#[tokio::test]
async fn the_same_head_is_unchanged() {
    let source = Worktree(Some(work(commit(1))));
    assert_eq!(ask(&source, Some(commit(1))).await, Answer::Unchanged);
}

#[tokio::test]
async fn an_item_without_work_here_is_not_here() {
    assert_eq!(ask(&Worktree(None), None).await, Answer::NotHere);
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
    assert_eq!(ask(&source, None).await, Answer::Work(big));
}

#[tokio::test]
async fn an_initiator_gives_up_on_a_peer_that_goes_silent() {
    let fixture = Fixture::new(1);
    let (left, _silent) = tokio::io::duplex(8192);
    let result = peek_initiate_within(
        left,
        &request(fixture.workforce(), None),
        Duration::from_millis(100),
    )
    .await;
    assert!(
        matches!(result, Err(PeekError::Frame(FrameError::TimedOut))),
        "{result:?}"
    );
}

#[tokio::test]
async fn a_responder_gives_up_on_a_peer_that_does_not_read() {
    let fixture = Fixture::new(1);
    // A buffer of one byte that nobody drains: the answer cannot be written.
    let (_silent, right) = tokio::io::duplex(1);
    let result = peek_respond_within(
        right,
        request(fixture.workforce(), None),
        &Worktree(Some(work(commit(1)))),
        Duration::from_millis(100),
    )
    .await;
    assert!(
        matches!(result, Err(PeekError::Frame(FrameError::TimedOut))),
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
async fn a_bound_device_peeks_and_a_stranger_is_refused() {
    if wf_core::testing::skip_in_fence("iroh needs the route socket") {
        return;
    }
    let fixture = Fixture::new(2);
    let holder = node(&fixture, &fixture.devices[0]).await;
    holder.serve_peek(Arc::new(Worktree(Some(work(commit(1))))));
    let member = node(&fixture, &fixture.devices[1]).await;
    let stranger = node(&fixture, &DeviceSigner::from_bytes(&[200; 32]).unwrap()).await;

    let answer = member
        .peek(holder.local_addr(), fixture.workforce(), item(), None)
        .await
        .unwrap();
    assert_eq!(answer, Answer::Work(work(commit(1))));
    let answer = member
        .peek(
            holder.local_addr(),
            fixture.workforce(),
            item(),
            Some(commit(1)),
        )
        .await
        .unwrap();
    assert_eq!(answer, Answer::Unchanged);

    let refused = stranger
        .peek(holder.local_addr(), fixture.workforce(), item(), None)
        .await;
    assert!(refused.is_err(), "{refused:?}");

    for node in [holder, member, stranger] {
        node.shutdown().await;
    }
}

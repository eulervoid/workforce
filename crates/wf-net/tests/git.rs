//! `wf/git/1` in memory with a fake process, and between two real endpoints.

mod common;

use common::SharedReplica;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use n0_future::boxed::BoxFuture;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wf_core::id::WorkforceId;
use wf_core::identity::DeviceSigner;
use wf_core::testing::*;
use wf_net::{
    GitError, GitProcess, GitRequest, GitSource, Network, Node, SourceError, git_initiate,
    git_respond, read_git_request,
};
use wf_store::{RedbStore, Replica};

const SILENCE: Duration = Duration::from_secs(5);

/// A process that answers with what it was sent, in capitals, and exits when its input ends.
struct Shout;

impl GitSource for Shout {
    fn upload_pack(&self, _: WorkforceId) -> BoxFuture<Result<GitProcess, SourceError>> {
        Box::pin(async {
            let (input, mut process_input) = tokio::io::duplex(1024);
            let (mut process_output, output) = tokio::io::duplex(1024);
            let finished = tokio::spawn(async move {
                let mut received = Vec::new();
                process_input.read_to_end(&mut received).await?;
                process_output
                    .write_all(&received.to_ascii_uppercase())
                    .await?;
                process_output.shutdown().await
            });
            Ok(GitProcess {
                input: Box::new(input),
                output: Box::new(output),
                finished: Box::pin(async move {
                    finished.await.map_err(SourceError::from)??;
                    Ok(())
                }),
            })
        })
    }
}

#[tokio::test]
async fn a_stream_is_piped_to_the_process_and_back() {
    let fixture = Fixture::new(1);
    let request = GitRequest {
        workforce: fixture.workforce(),
    };
    let (initiator, mut responder) = tokio::io::duplex(1024);
    let (mut git, local) = tokio::io::duplex(1024);
    let responding = async {
        let read = read_git_request(&mut responder, Duration::from_secs(5)).await?;
        assert_eq!(read, request);
        git_respond(responder, read, &Shout, SILENCE).await
    };
    let fetching = git_initiate(initiator, &request, local, SILENCE);
    let talking = async {
        git.write_all(b"want a commit").await.unwrap();
        git.shutdown().await.unwrap();
        let mut answer = String::new();
        git.read_to_string(&mut answer).await.unwrap();
        answer
    };
    let (served, fetched, answer) = tokio::join!(responding, fetching, talking);
    served.unwrap();
    fetched.unwrap();
    assert_eq!(answer, "WANT A COMMIT");
}

/// A process that refuses: it says so and exits without reading what it is sent, and tells when
/// it has been waited for.
struct Refuse(Arc<AtomicBool>);

impl GitSource for Refuse {
    fn upload_pack(&self, _: WorkforceId) -> BoxFuture<Result<GitProcess, SourceError>> {
        let reaped = Arc::clone(&self.0);
        Box::pin(async move {
            let (input, process_input) = tokio::io::duplex(16);
            let (mut process_output, output) = tokio::io::duplex(1024);
            drop(process_input);
            process_output.write_all(b"ERR not our ref").await?;
            drop(process_output);
            Ok(GitProcess {
                input: Box::new(input),
                output: Box::new(output),
                finished: Box::pin(async move {
                    reaped.store(true, Ordering::SeqCst);
                    Err("exit status: 128".into())
                }),
            })
        })
    }
}

/// A process that never says anything and never reads, and tells when it has been waited for.
struct Mute(Arc<AtomicBool>);

impl GitSource for Mute {
    fn upload_pack(&self, _: WorkforceId) -> BoxFuture<Result<GitProcess, SourceError>> {
        let reaped = Arc::clone(&self.0);
        Box::pin(async move {
            let (input, process_input) = tokio::io::duplex(16);
            let (process_output, output) = tokio::io::duplex(16);
            Ok(GitProcess {
                input: Box::new(input),
                output: Box::new(output),
                finished: Box::pin(async move {
                    drop((process_input, process_output));
                    reaped.store(true, Ordering::SeqCst);
                    Ok(())
                }),
            })
        })
    }
}

#[tokio::test]
async fn the_answer_of_a_process_that_stops_reading_arrives_and_it_is_waited_for() {
    let fixture = Fixture::new(1);
    let request = GitRequest {
        workforce: fixture.workforce(),
    };
    let reaped = Arc::new(AtomicBool::new(false));
    let (initiator, responder) = tokio::io::duplex(1024);
    let (mut git, local) = tokio::io::duplex(1024);
    let source = Refuse(Arc::clone(&reaped));
    let responding = git_respond(responder, request, &source, SILENCE);
    let fetching = git_initiate(initiator, &request, local, SILENCE);
    let talking = async {
        // Still sending when the process has gone.
        let _ = git.write_all(&[b'x'; 64 * 1024]).await;
        let _ = git.shutdown().await;
        let mut answer = String::new();
        git.read_to_string(&mut answer).await.unwrap();
        answer
    };
    let (served, fetched, answer) = tokio::join!(responding, fetching, talking);
    assert_eq!(answer, "ERR not our ref");
    assert!(matches!(served, Err(GitError::Process(_))), "{served:?}");
    fetched.unwrap();
    assert!(reaped.load(Ordering::SeqCst));
}

#[tokio::test(start_paused = true)]
async fn a_fetch_where_nothing_moves_is_given_up_and_the_process_is_waited_for() {
    let fixture = Fixture::new(1);
    let request = GitRequest {
        workforce: fixture.workforce(),
    };
    let reaped = Arc::new(AtomicBool::new(false));
    // The initiator sent its request and then went quiet.
    let (_initiator, responder) = tokio::io::duplex(1024);
    let served = git_respond(responder, request, &Mute(Arc::clone(&reaped)), SILENCE).await;
    assert!(matches!(served, Err(GitError::Silent(_))), "{served:?}");
    assert!(reaped.load(Ordering::SeqCst));

    // And the initiator gives up on a responder that went quiet.
    let (initiator, _responder) = tokio::io::duplex(1024);
    let (_git, local) = tokio::io::duplex(1024);
    let fetched = git_initiate(initiator, &request, local, SILENCE).await;
    assert!(matches!(fetched, Err(GitError::Silent(_))), "{fetched:?}");
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

async fn connect(
    node: &Node<SharedReplica<RedbStore>>,
    holder: &Node<SharedReplica<RedbStore>>,
    workforce: WorkforceId,
) -> Result<String, wf_net::NetError> {
    let (mut git, local) = tokio::io::duplex(1024);
    let talking = async {
        git.write_all(b"hello").await.unwrap();
        git.shutdown().await.unwrap();
        let mut answer = String::new();
        let _ = git.read_to_string(&mut answer).await;
        answer
    };
    let (connected, answer) = tokio::join!(
        node.git_connect(holder.local_addr(), workforce, local),
        talking
    );
    connected.map(|()| answer)
}

#[tokio::test]
async fn a_bound_device_reaches_the_process_and_a_stranger_is_refused() {
    let fixture = Fixture::new(2);
    let holder = node(&fixture, &fixture.devices[0]).await;
    holder.serve_git(Arc::new(Shout));
    let member = node(&fixture, &fixture.devices[1]).await;
    let stranger = node(&fixture, &DeviceSigner::from_bytes(&[200; 32]).unwrap()).await;

    assert_eq!(
        connect(&member, &holder, fixture.workforce())
            .await
            .unwrap(),
        "HELLO"
    );
    let refused = connect(&stranger, &holder, fixture.workforce()).await;
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
async fn a_node_that_serves_no_git_refuses_instead_of_hanging() {
    let fixture = Fixture::new(2);
    let holder = node(&fixture, &fixture.devices[0]).await;
    let member = node(&fixture, &fixture.devices[1]).await;
    let refused = tokio::time::timeout(
        Duration::from_secs(20),
        connect(&member, &holder, fixture.workforce()),
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

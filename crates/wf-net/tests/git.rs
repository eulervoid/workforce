//! `wf/git/1` in memory with a fake process, and between two real endpoints.

mod common;

use common::SharedReplica;

use std::sync::Arc;
use std::time::Duration;

use n0_future::boxed::BoxFuture;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wf_core::id::WorkforceId;
use wf_core::identity::DeviceSigner;
use wf_core::testing::*;
use wf_net::{
    GitProcess, GitRequest, GitSource, Network, Node, SourceError, git_initiate, git_respond,
    read_git_request,
};
use wf_store::{RedbStore, Replica};

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
        git_respond(responder, read, &Shout).await
    };
    let fetching = git_initiate(initiator, &request, local);
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
    if wf_core::testing::skip_in_fence("iroh needs the route socket") {
        return;
    }
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
    if wf_core::testing::skip_in_fence("iroh needs the route socket") {
        return;
    }
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

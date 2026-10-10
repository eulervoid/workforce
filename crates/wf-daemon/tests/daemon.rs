use std::path::Path;
use std::sync::Arc;

use tempfile::TempDir;
use wf_core::testing::*;
use wf_daemon::client::{ClientError, DaemonClient};
use wf_daemon::proto::{
    Hello, HelloReply, PROTOCOL_VERSION, ProtoError, read_message, write_message,
};
use wf_daemon::{Config, Daemon, DaemonError, STORE_FILE};
use wf_net::Network;
use wf_store::{RedbStore, Replica};

struct Running {
    _directory: TempDir,
    fixture: Fixture,
    socket: std::path::PathBuf,
    daemon: Arc<Daemon>,
    task: tokio::task::JoinHandle<Result<(), DaemonError>>,
    shutdown: tokio::sync::oneshot::Sender<()>,
}

async fn start() -> Running {
    let directory = TempDir::new().unwrap();
    let fixture = Fixture::new(1);
    let store_directory = directory.path().join(fixture.workforce().to_string());
    std::fs::create_dir_all(&store_directory).unwrap();
    let mut replica =
        Replica::open(RedbStore::open(store_directory.join(STORE_FILE)).unwrap()).unwrap();
    replica.receive(fixture.genesis.clone()).unwrap();
    replica
        .author(
            &fixture.devices[0],
            fixture.founder_actor(),
            2_000,
            create("task", "stored before the daemon"),
        )
        .unwrap();
    drop(replica);

    let socket = directory.path().join("daemon.sock");
    let daemon = Daemon::start(
        Config {
            data_dir: directory.path().to_path_buf(),
            socket: socket.clone(),
            network: Network::Direct,
        },
        fixture.devices[0].clone(),
    )
    .await
    .unwrap();
    let (shutdown, signal) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(Arc::clone(&daemon).serve(async {
        let _ = signal.await;
    }));
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    Running {
        _directory: directory,
        fixture,
        socket,
        daemon,
        task,
        shutdown,
    }
}

async fn blocking<T: Send + 'static>(job: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(job).await.unwrap()
}

fn connect(socket: &Path) -> DaemonClient {
    DaemonClient::connect(socket).unwrap().expect("a daemon")
}

#[tokio::test]
async fn the_daemon_serves_the_state_events_and_authoring() {
    let running = start().await;
    let socket = running.socket.clone();
    let workforce = running.fixture.workforce();
    let actor = running.fixture.founder_actor();

    let (state, events, authored, after) = blocking(move || {
        let mut client = connect(&socket);
        let state = client.state(workforce).unwrap();
        let events = client.events(workforce).unwrap();
        let authored = client
            .author(workforce, actor, create("task", "through the socket"))
            .unwrap();
        let fetched = client.event(workforce, authored.id()).unwrap();
        assert_eq!(fetched.as_ref(), Some(&authored));
        let after = client.state(workforce).unwrap();
        (state, events, authored, after)
    })
    .await;
    assert_eq!(state.event_count(), 2);
    assert_eq!(events.len(), 2);
    assert_eq!(after.event_count(), 3);
    assert!(after.contains(&authored.id()));

    running.shutdown.send(()).unwrap();
    running.task.await.unwrap().unwrap();
    assert!(!running.socket.exists(), "the socket is removed on exit");
}

#[tokio::test]
async fn policy_violations_come_back_as_errors() {
    let running = start().await;
    let socket = running.socket.clone();
    let workforce = running.fixture.workforce();
    let actor = running.fixture.founder_actor();
    let result =
        blocking(move || connect(&socket).author(workforce, actor, create("nonsense", "x"))).await;
    assert!(matches!(result, Err(ClientError::Failed(message)) if message.contains("nonsense")));

    let unknown = wf_core::id::WorkforceId::from(wf_core::id::EventId::of(b"unknown"));
    let socket = running.socket.clone();
    let result = blocking(move || connect(&socket).state(unknown)).await;
    assert!(matches!(result, Err(ClientError::Failed(_))));
    drop(running.daemon);
}

#[tokio::test]
async fn a_client_of_another_build_is_refused() {
    let running = start().await;
    let socket = running.socket.clone();
    let reply = blocking(move || {
        let mut stream = std::os::unix::net::UnixStream::connect(&socket).unwrap();
        write_message(
            &mut stream,
            &Hello {
                version: PROTOCOL_VERSION + 1,
            },
        )
        .unwrap();
        read_message::<_, HelloReply>(&mut stream).unwrap()
    })
    .await;
    assert!(matches!(
        reply,
        HelloReply::Refused {
            version: PROTOCOL_VERSION
        }
    ));
    let _ = ProtoError::VersionMismatch {
        daemon: 1,
        client: 2,
    }
    .to_string();
}

#[tokio::test]
async fn a_second_daemon_on_the_same_socket_is_refused() {
    let running = start().await;
    let second = Daemon::start(
        Config {
            data_dir: running._directory.path().to_path_buf(),
            socket: running.socket.clone(),
            network: Network::Direct,
        },
        wf_core::identity::DeviceSigner::from_bytes(&[77; 32]).unwrap(),
    )
    .await
    .unwrap();
    let result = second.serve(std::future::pending()).await;
    assert!(matches!(result, Err(DaemonError::AlreadyRunning(_))));
}

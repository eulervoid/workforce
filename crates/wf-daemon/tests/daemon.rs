use std::path::Path;
use std::sync::Arc;

use tempfile::TempDir;
use wf_core::testing::*;
use wf_daemon::client::{ClientError, DaemonClient};
use wf_daemon::data_dir::STORE_FILE;
use wf_daemon::proto::{
    Hello, HelloReply, PROTOCOL_VERSION, ProtoError, read_message, write_message,
};
use wf_daemon::{Daemon, DaemonError, DataDir, Launch};
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

/// Ends the test process on a panic in any task or thread, as the daemon binary stops: a
/// panicking task would otherwise go unnoticed, as only the tasks it serves would stop. The
/// hook is process-wide and tests run in parallel, so it is installed once, with a watcher
/// thread that outlives every test runtime.
fn fail_on_panic() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        let mut panics = wf_daemon::watch_panics();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            let report = runtime.block_on(panics.first());
            eprintln!("the daemon panicked: {report}");
            std::process::exit(101);
        });
    });
}

async fn start() -> Running {
    fail_on_panic();
    let directory = TempDir::new().unwrap();
    let fixture = Fixture::new(1);
    let store_directory = directory.path().join(fixture.workforce().to_string());
    std::fs::create_dir_all(&store_directory).unwrap();
    let mut replica = Replica::open(
        RedbStore::open(store_directory.join(STORE_FILE)).unwrap(),
        fixture.workforce(),
    )
    .unwrap();
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
        Launch {
            data_dir: DataDir::new(directory.path().to_path_buf()),
            socket: socket.clone(),
            network: Network::Direct,
            harnesses: Default::default(),
            machine_config: None,
            wf_binary: None,
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

/// Connects as the principal, as the CLI does: the tests keep the socket in the data directory.
fn connect(socket: &Path) -> DaemonClient {
    let token = DataDir::new(socket.parent().unwrap().to_path_buf()).read_principal_token();
    DaemonClient::connect(socket, token)
        .unwrap()
        .expect("a daemon")
}

#[tokio::test]
async fn the_daemon_serves_the_state_events_and_authoring() {
    let running = start().await;
    let socket = running.socket.clone();
    let workforce = running.fixture.workforce();
    let principal = running.fixture.founder_actor();

    let device = running.fixture.devices[0].key();
    let (state, events, authored, after) = blocking(move || {
        let mut client = connect(&socket);
        assert_eq!(
            client.device(),
            device,
            "the daemon says which device it signs as"
        );
        let state = client.state(workforce).unwrap();
        let events = client.events(workforce).unwrap();
        let authored = client
            .author(workforce, create("task", "through the socket"))
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
    assert_eq!(
        authored.content.actor, principal,
        "the principal token acts for the principal"
    );

    running.shutdown.send(()).unwrap();
    running.task.await.unwrap().unwrap();
    assert!(!running.socket.exists(), "the socket is removed on exit");
}

#[tokio::test]
async fn policy_violations_come_back_as_errors() {
    let running = start().await;
    let socket = running.socket.clone();
    let workforce = running.fixture.workforce();
    let result =
        blocking(move || connect(&socket).author(workforce, create("nonsense", "x"))).await;
    assert!(matches!(result, Err(ClientError::Failed(message)) if message.contains("nonsense")));

    let unknown = wf_core::id::WorkforceId::from(wf_core::id::EventId::of(b"unknown"));
    let socket = running.socket.clone();
    let result = blocking(move || connect(&socket).state(unknown)).await;
    assert!(matches!(result, Err(ClientError::Failed(_))));
    drop(running.daemon);
}

#[tokio::test]
async fn the_daemon_says_its_own_pid() {
    let running = start().await;
    let socket = running.socket.clone();
    let pid = blocking(move || connect(&socket).pid()).await;
    assert_eq!(pid, std::process::id());
    drop(running.daemon);
}

#[tokio::test]
async fn reads_of_a_workforce_the_daemon_lacks_are_errors() {
    let running = start().await;
    let socket = running.socket.clone();
    let unknown = wf_core::id::WorkforceId::from(wf_core::id::EventId::of(b"unknown"));
    let (permissions, faults, last_seen) = blocking(move || {
        let mut client = connect(&socket);
        (
            client.permissions(unknown).map(drop),
            client.machine_faults(unknown).map(drop),
            client.last_seen(unknown).map(drop),
        )
    })
    .await;
    for result in [permissions, faults, last_seen] {
        assert!(matches!(result, Err(ClientError::Failed(_))), "{result:?}");
    }
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
async fn a_connection_without_a_token_of_the_daemon_acts_for_nobody() {
    let running = start().await;
    for token in [None, Some("ab".repeat(32).parse().unwrap())] {
        let socket = running.socket.clone();
        let result = blocking(move || DaemonClient::connect(&socket, token)).await;
        assert!(
            matches!(result, Err(ClientError::Unrecognized)),
            "{:?}",
            result.map(|_| ())
        );
    }
}

#[tokio::test]
async fn a_second_daemon_on_the_same_socket_is_refused() {
    let running = start().await;
    let workforce = running.fixture.workforce();
    let second = Daemon::start(
        Launch {
            data_dir: DataDir::new(running._directory.path().to_path_buf()),
            socket: running.socket.clone(),
            network: Network::Direct,
            harnesses: Default::default(),
            machine_config: None,
            wf_binary: None,
        },
        wf_core::identity::DeviceSigner::from_bytes(&[77; 32]).unwrap(),
    )
    .await
    .unwrap();
    let result = second.serve(std::future::pending()).await;
    assert!(matches!(result, Err(DaemonError::AlreadyRunning(_))));
    let socket = running.socket.clone();
    blocking(move || connect(&socket).state(workforce).unwrap()).await;
}

async fn start_peer(
    fixture: &Fixture,
    device: usize,
) -> (TempDir, Arc<Daemon>, std::path::PathBuf) {
    start_peer_as(fixture, fixture.devices[device].clone()).await
}

async fn start_peer_as(
    fixture: &Fixture,
    signer: wf_core::identity::DeviceSigner,
) -> (TempDir, Arc<Daemon>, std::path::PathBuf) {
    let directory = TempDir::new().unwrap();
    let store_directory = directory.path().join(fixture.workforce().to_string());
    std::fs::create_dir_all(&store_directory).unwrap();
    let mut replica = Replica::open(
        RedbStore::open(store_directory.join(STORE_FILE)).unwrap(),
        fixture.workforce(),
    )
    .unwrap();
    replica.receive(fixture.genesis.clone()).unwrap();
    drop(replica);
    let socket = directory.path().join("daemon.sock");
    let daemon = Daemon::start(
        Launch {
            data_dir: DataDir::new(directory.path().to_path_buf()),
            socket: socket.clone(),
            network: Network::Direct,
            harnesses: Default::default(),
            machine_config: None,
            wf_binary: None,
        },
        signer,
    )
    .await
    .unwrap();
    tokio::spawn(Arc::clone(&daemon).serve(std::future::pending()));
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    (directory, daemon, socket)
}

#[tokio::test]
async fn daemons_of_two_devices_converge_on_their_own() {
    if wf_core::testing::skip_in_fence("iroh needs the route socket") {
        return;
    }
    let fixture = Fixture::new(2);
    let (_first_dir, laptop, laptop_socket) = start_peer(&fixture, 0).await;
    let (_second_dir, vm, vm_socket) = start_peer(&fixture, 1).await;
    laptop
        .node()
        .await
        .unwrap()
        .remember(vm.node().await.unwrap().local_addr());
    vm.node()
        .await
        .unwrap()
        .remember(laptop.node().await.unwrap().local_addr());
    let workforce = fixture.workforce();

    let written = {
        let socket = laptop_socket.clone();
        blocking(move || {
            connect(&socket)
                .author(workforce, create("task", "written on the laptop"))
                .unwrap()
        })
        .await
    };

    let mut arrived = false;
    for _ in 0..200 {
        let socket = vm_socket.clone();
        let state = blocking(move || connect(&socket).state(workforce).unwrap()).await;
        if state.contains(&written.id()) {
            arrived = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(arrived, "the vm's daemon received the event by itself");

    let mut seen_vm = false;
    for _ in 0..200 {
        let socket = laptop_socket.clone();
        let seen = blocking(move || connect(&socket).last_seen(workforce).unwrap()).await;
        if seen.iter().any(|(device, _)| *device == vm.device()) {
            seen_vm = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(seen_vm, "the laptop knows when it last saw the vm");
}

#[tokio::test]
async fn the_daemon_founds_a_fork_from_its_own_replica() {
    let running = start().await;
    let socket = running.socket.clone();
    let parent = running.fixture.workforce();
    let founder = wf_core::identity::PrincipalSigner::from_bytes(&[1; 32]).unwrap();
    let fixture = Fixture::new(1);

    let (parent_state, child_state, child) = blocking(move || {
        let mut client = connect(&socket);
        let parent_state = client.state(parent).unwrap();
        let genesis = fixture.fork(&parent_state, &founder, 90_000);
        let child = genesis.id().into();
        client.fork(parent, genesis).unwrap();
        let child_state = client.state(child).unwrap();
        (parent_state, child_state, child)
    })
    .await;
    assert_eq!(child_state.workforce_id(), Some(child));
    assert!(!child_state.history_incomplete());
    assert_eq!(child_state.items().count(), parent_state.items().count());
    assert_eq!(child_state.event_count(), parent_state.event_count() + 1);

    running.shutdown.send(()).unwrap();
    running.task.await.unwrap().unwrap();
}

#[tokio::test]
async fn a_fork_that_cannot_be_founded_leaves_no_store_behind() {
    let running = start().await;
    let socket = running.socket.clone();
    let parent = running.fixture.workforce();
    let founder = wf_core::identity::PrincipalSigner::from_bytes(&[1; 32]).unwrap();
    // A genesis that names heads this daemon does not have.
    let other = Fixture::new(1);
    let mut elsewhere = other.state();
    other.apply(&mut elsewhere, 0, 3_000, create("task", "only elsewhere"));
    let genesis = other.fork(&elsewhere, &founder, 90_000);
    let child: wf_core::id::WorkforceId = genesis.id().into();

    let result = blocking(move || connect(&socket).fork(parent, genesis)).await;
    assert!(matches!(result, Err(ClientError::Failed(_))), "{result:?}");
    let store = running
        ._directory
        .path()
        .join(child.to_string())
        .join(STORE_FILE);
    assert!(!store.exists());
}

#[tokio::test]
async fn a_daemon_follows_a_fork_with_the_history_it_already_has() {
    if wf_core::testing::skip_in_fence("iroh needs the route socket") {
        return;
    }
    let fixture = Fixture::new(1);
    let (_laptop_dir, laptop, laptop_socket) = start_peer(&fixture, 0).await;
    let (_vm_dir, vm, vm_socket) = start_peer_as(&fixture, fixture.member_device.clone()).await;
    laptop
        .node()
        .await
        .unwrap()
        .remember(vm.node().await.unwrap().local_addr());
    vm.node()
        .await
        .unwrap()
        .remember(laptop.node().await.unwrap().local_addr());
    let parent = fixture.workforce();

    let socket = laptop_socket.clone();
    blocking(move || {
        let mut client = connect(&socket);
        for index in 0..5 {
            client
                .author(parent, create("task", &format!("task {index}")))
                .unwrap();
        }
    })
    .await;
    let mut synced = false;
    for _ in 0..200 {
        let socket = vm_socket.clone();
        let state = blocking(move || connect(&socket).state(parent).unwrap()).await;
        if state.event_count() == 6 {
            synced = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(synced, "the vm has the parent before the fork");

    let socket = laptop_socket.clone();
    let founder = wf_core::identity::PrincipalSigner::from_bytes(&[1; 32]).unwrap();
    let genesis_fixture = Fixture::new(1);
    let (child, forker_state) = blocking(move || {
        let mut client = connect(&socket);
        let state = client.state(parent).unwrap();
        let genesis = genesis_fixture.fork(&state, &founder, 90_000);
        let child: wf_core::id::WorkforceId = genesis.id().into();
        client.fork(parent, genesis).unwrap();
        (child, client.state(child).unwrap())
    })
    .await;

    let socket = vm_socket.clone();
    let laptop_device = laptop.device();
    let (results, followed) = blocking(move || {
        let mut client = connect(&socket);
        let results = client.follow(child, vec![laptop_device]).unwrap();
        (results, client.state(child).unwrap())
    })
    .await;
    assert!(results.iter().all(|result| result.reached()));
    for result in &results {
        match result.outcome {
            wf_daemon::PeerOutcome::Synced { received, .. } => {
                assert!(received <= 1, "the history did not come from the laptop")
            }
            wf_daemon::PeerOutcome::Unreachable(_) => unreachable!("reached"),
        }
    }
    assert!(!followed.history_incomplete());
    assert_eq!(followed.event_count(), forker_state.event_count());
    assert_eq!(followed.items().count(), 5);
}

async fn start_empty(
    signer: wf_core::identity::DeviceSigner,
) -> (TempDir, Arc<Daemon>, std::path::PathBuf) {
    let directory = TempDir::new().unwrap();
    let socket = directory.path().join("daemon.sock");
    let daemon = Daemon::start(
        Launch {
            data_dir: DataDir::new(directory.path().to_path_buf()),
            socket: socket.clone(),
            network: Network::Direct,
            harnesses: Default::default(),
            machine_config: None,
            wf_binary: None,
        },
        signer,
    )
    .await
    .unwrap();
    tokio::spawn(Arc::clone(&daemon).serve(std::future::pending()));
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    (directory, daemon, socket)
}

#[tokio::test]
async fn a_running_daemon_joins_a_workforce_with_its_own_key() {
    if wf_core::testing::skip_in_fence("iroh needs the route socket") {
        return;
    }
    use wf_core::event::{DeviceProfile, EventBody, InviteCreated, InviteKind};
    use wf_core::identity::{InviteSigner, Signed};
    use wf_core::invite::InviteCode;

    let fixture = Fixture::new(1);
    let (_laptop_dir, laptop, laptop_socket) = start_peer(&fixture, 0).await;
    let vm_signer = wf_core::identity::DeviceSigner::from_bytes(&[90; 32]).unwrap();
    let (_vm_dir, vm, vm_socket) = start_empty(vm_signer.clone()).await;
    vm.node()
        .await
        .unwrap()
        .remember(laptop.node().await.unwrap().local_addr());
    let workforce = fixture.workforce();

    let secret = InviteSigner::generate([7; 32]);
    let statement = InviteCreated {
        kind: InviteKind::Device,
        invite: secret.key(),
        expires_ms: u64::MAX / 2,
    };
    let body = EventBody::InviteCreated(
        Signed::sign(&fixture.founder, Some(workforce), statement).unwrap(),
    );
    let socket = laptop_socket.clone();
    let invite = blocking(move || connect(&socket).author(workforce, body).unwrap()).await;
    let code = InviteCode::new(workforce, invite.id(), &secret, vec![laptop.device()]);

    let socket = vm_socket.clone();
    let state = blocking(move || {
        let mut client = connect(&socket);
        let profile = DeviceProfile {
            name: "vm".into(),
            hardware: "test".into(),
            agents: Default::default(),
            max_sessions: 1,
        };
        client.join(workforce, &code, profile).unwrap();
        client.state(workforce).unwrap()
    })
    .await;
    assert_eq!(
        state.principal_of(&vm_signer.key()),
        Some(fixture.founder.key())
    );
    assert!(
        state
            .devices()
            .any(|info| info.profile.is_some_and(|p| p.name == "vm"))
    );

    let socket = laptop_socket.clone();
    let laptop_state = blocking(move || connect(&socket).state(workforce).unwrap()).await;
    assert_eq!(
        laptop_state.principal_of(&vm_signer.key()),
        Some(fixture.founder.key()),
        "the laptop heard of the new device"
    );
}

#[tokio::test]
async fn a_log_longer_than_a_page_arrives_whole_and_in_order() {
    use wf_core::state::Trust;
    use wf_daemon::proto::EVENTS_PER_PAGE;

    let directory = TempDir::new().unwrap();
    let fixture = Fixture::new(1);
    let store_directory = directory.path().join(fixture.workforce().to_string());
    std::fs::create_dir_all(&store_directory).unwrap();
    let mut replica = Replica::open(
        RedbStore::open(store_directory.join(STORE_FILE)).unwrap(),
        fixture.workforce(),
    )
    .unwrap();
    replica.receive(fixture.genesis.clone()).unwrap();
    let mut state = replica.state().clone();
    let mut batch = Vec::new();
    for index in 0..(2 * EVENTS_PER_PAGE as u64 + 10) {
        let event = fixture.author(
            &state,
            0,
            2_000 + index,
            create("task", &format!("t{index}")),
        );
        state.receive(event.clone(), Trust::Stored).unwrap();
        batch.push(event);
    }
    replica.receive_batch(batch).unwrap();
    let expected = wf_store::EventStore::log_since(replica.store(), 0).unwrap();
    drop(replica);

    let socket = directory.path().join("daemon.sock");
    let daemon = Daemon::start(
        Launch {
            data_dir: DataDir::new(directory.path().to_path_buf()),
            socket: socket.clone(),
            network: Network::Direct,
            harnesses: Default::default(),
            machine_config: None,
            wf_binary: None,
        },
        fixture.devices[0].clone(),
    )
    .await
    .unwrap();
    tokio::spawn(Arc::clone(&daemon).serve(std::future::pending()));
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let workforce = fixture.workforce();
    let events = blocking(move || connect(&socket).events(workforce).unwrap()).await;
    assert_eq!(events, expected);
    assert!(events.len() > 2 * EVENTS_PER_PAGE);
}

#[tokio::test]
async fn a_daemon_starts_with_an_empty_store_in_its_data_directory() {
    fail_on_panic();
    let directory = TempDir::new().unwrap();
    let fixture = Fixture::new(1);
    let store_directory = directory.path().join(fixture.workforce().to_string());
    std::fs::create_dir_all(&store_directory).unwrap();
    drop(RedbStore::open(store_directory.join(STORE_FILE)).unwrap());
    let daemon = Daemon::start(
        Launch {
            data_dir: DataDir::new(directory.path().to_path_buf()),
            socket: directory.path().join("daemon.sock"),
            network: Network::Direct,
            harnesses: Default::default(),
            machine_config: None,
            wf_binary: None,
        },
        fixture.devices[0].clone(),
    )
    .await
    .unwrap();
    // The tasks that need the policy run at once; give them time to panic.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    drop(daemon);
}

#[tokio::test]
async fn a_join_with_an_invite_for_another_workforce_leaves_no_store_behind() {
    use wf_core::event::DeviceProfile;
    use wf_core::identity::InviteSigner;
    use wf_core::invite::InviteCode;

    let running = start().await;
    let socket = running.socket.clone();
    let own = running.fixture.workforce();
    let other = Fixture::new(2).workforce();
    let code = InviteCode::new(
        own,
        running.fixture.genesis.id(),
        &InviteSigner::generate([7; 32]),
        vec![running.daemon.device()],
    );
    let data_dir = socket.parent().unwrap().to_path_buf();
    let result = blocking(move || {
        let profile = DeviceProfile {
            name: "vm".into(),
            hardware: "test".into(),
            agents: Default::default(),
            max_sessions: 1,
        };
        connect(&socket).join(other, &code, profile)
    })
    .await;
    assert!(result.is_err());
    assert!(!data_dir.join(other.to_string()).exists());
}

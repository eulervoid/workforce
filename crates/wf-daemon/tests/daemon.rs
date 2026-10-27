use std::path::Path;
use std::sync::Arc;

use tempfile::TempDir;
use wf_core::session::SessionRole;
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
    start_with(|_| {}).await
}

/// Like [`start`], with `prepare` run on the directory of the workforce before the daemon starts.
async fn start_with(prepare: impl FnOnce(&Path)) -> Running {
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
    prepare(&store_directory);

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
    wait_for_socket(&socket).await;
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

/// Waits until the daemon listens on `socket`.
async fn wait_for_socket(socket: &std::path::Path) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while !socket.exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the daemon listens on {}",
            socket.display()
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_workforce_without_a_bare_repository_gets_one_from_the_repository_it_remembered() {
    let repository = wf_vcs::testing::TestRepository::new();
    repository.work("wf/k3f9k3f9k3f9", "a.txt", "a\n");
    repository.work("private", "b.txt", "b\n");
    let checkout = repository.directory();
    let running = start_with(|directory| {
        std::fs::write(directory.join("repository"), checkout.to_str().unwrap()).unwrap();
    })
    .await;
    let workforce_directory = DataDir::new(running._directory.path().to_path_buf())
        .workforce(&running.fixture.workforce());
    let bare = wf_vcs::bare_repository(&workforce_directory);
    assert!(bare.join("HEAD").is_file());
    let branches = wf_vcs::testing::git_in(&bare, &["branch", "--format=%(refname:short)"]);
    let mut branches: Vec<_> = branches.lines().collect();
    branches.sort();
    assert_eq!(branches, ["main", "wf/k3f9k3f9k3f9"]);
    assert!(
        !workforce_directory.join("repository").exists(),
        "the file is done with"
    );
    let upstream =
        wf_vcs::testing::git_in(&checkout, &["rev-parse", "--abbrev-ref", "main@{upstream}"]);
    assert_eq!(upstream, "wf/main");
}

#[tokio::test]
async fn a_workforce_whose_remembered_checkout_is_gone_gets_an_empty_bare_repository() {
    let running = start_with(|directory| {
        std::fs::write(directory.join("repository"), "/nonexistent/checkout").unwrap();
    })
    .await;
    let workforce_directory = DataDir::new(running._directory.path().to_path_buf())
        .workforce(&running.fixture.workforce());
    let bare = wf_vcs::bare_repository(&workforce_directory);
    assert!(bare.join("HEAD").is_file());
    assert_eq!(wf_vcs::testing::git_in(&bare, &["branch", "--list"]), "");
    assert!(
        !workforce_directory.join("repository").exists(),
        "the file is done with"
    );
}

#[tokio::test]
async fn a_workforce_that_never_had_a_repository_gets_an_empty_bare_one() {
    let running = start().await;
    let workforce_directory = DataDir::new(running._directory.path().to_path_buf())
        .workforce(&running.fixture.workforce());
    let bare = wf_vcs::bare_repository(&workforce_directory);
    assert_eq!(
        wf_vcs::testing::git_in(&bare, &["rev-parse", "--is-bare-repository"]),
        "true"
    );
    assert_eq!(
        wf_vcs::testing::git_in(&bare, &["branch", "--list"]),
        "",
        "no branches yet"
    );
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
            client.presence(unknown).map(drop),
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
    start_peer_with(fixture, signer, |_| {}).await
}

/// Like [`start_peer_as`], with `prepare` run on the replica before the daemon starts.
async fn start_peer_with(
    fixture: &Fixture,
    signer: wf_core::identity::DeviceSigner,
    prepare: impl FnOnce(&mut Replica<RedbStore>),
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
    prepare(&mut replica);
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
    wait_for_socket(&socket).await;
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
        let presence = blocking(move || connect(&socket).presence(workforce).unwrap()).await;
        if presence.heard_ms().any(|(device, _)| device == vm.device()) {
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
    wait_for_socket(&socket).await;
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
    wait_for_socket(&socket).await;
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

/// Where `directory`, a daemon's data directory, has its agent repositories, and a repository,
/// standing in for the bare repository of the workforce, with the work of `item` on its branch in
/// the agent repository there (a worktree of the test repository); returns the head of the
/// branch.
fn give_work(
    directory: &Path,
    workforce: wf_core::id::WorkforceId,
    item: wf_core::id::ItemId,
    repository: &wf_vcs::testing::TestRepository,
) -> wf_core::session::CommitId {
    let data_dir = DataDir::new(directory.to_path_buf());
    let workforce_directory = data_dir.workforce(&workforce);
    // The test repository stands in for the workforce's bare repository, which the daemon made
    // empty when it loaded the workforce.
    let bare = wf_vcs::bare_repository(&workforce_directory);
    std::fs::remove_dir_all(&bare).unwrap();
    std::os::unix::fs::symlink(repository.directory(), bare).unwrap();
    let agent_repository = wf_vcs::item_repository(&workforce_directory, item);
    repository.prepare_worktree(&wf_vcs::item_branch(item), &agent_repository, "main");
    repository.write(&agent_repository, "live.txt", "work in progress\n");
    repository.commit(&agent_repository, "the first step")
}

/// Starts a work session of a new item on the daemon behind `socket`, which has the item; returns
/// the item.
async fn start_work_session(
    socket: &Path,
    workforce: wf_core::id::WorkforceId,
) -> wf_core::id::ItemId {
    let socket = socket.to_path_buf();
    blocking(move || {
        let mut client = connect(&socket);
        client.author(workforce, define_agent("coder")).unwrap();
        let item = client
            .author(workforce, create("task", "the work shown live"))
            .unwrap()
            .id()
            .into();
        client
            .author(workforce, start_session("coder", item))
            .unwrap();
        item
    })
    .await
}

async fn live_diff(
    socket: &Path,
    workforce: wf_core::id::WorkforceId,
    item: wf_core::id::ItemId,
    since: Option<wf_core::session::CommitId>,
) -> wf_daemon::proto::LiveDiff {
    let socket = socket.to_path_buf();
    blocking(move || connect(&socket).live_diff(workforce, item, since).unwrap()).await
}

#[tokio::test]
async fn a_session_on_this_device_is_answered_from_its_agent_repository() {
    use wf_daemon::proto::LiveDiff;
    use wf_net::DiffAnswer;

    let running = start().await;
    let workforce = running.fixture.workforce();
    let idle = {
        let socket = running.socket.clone();
        blocking(move || {
            let mut client = connect(&socket);
            client.author(workforce, define_agent("coder")).unwrap();
            client
                .author(workforce, create("task", "nobody works on it"))
                .unwrap()
                .id()
                .into()
        })
        .await
    };
    assert_eq!(
        live_diff(&running.socket, workforce, idle, None).await,
        LiveDiff::NoSession
    );

    let item = start_work_session(&running.socket, workforce).await;
    let repository = wf_vcs::testing::TestRepository::new();
    let head = give_work(running._directory.path(), workforce, item, &repository);

    let LiveDiff::Answer(DiffAnswer::Work(work)) =
        live_diff(&running.socket, workforce, item, None).await
    else {
        panic!("the daemon answers with the work of its own session");
    };
    assert_eq!((work.target.as_str(), work.head), ("main", head));
    assert!(work.committed.contains("+work in progress"));
    assert_eq!(
        live_diff(&running.socket, workforce, item, Some(head)).await,
        LiveDiff::Answer(DiffAnswer::Unchanged)
    );

    // The branch and the agent repository are gone, as after the session ended.
    let agent_repository = wf_vcs::item_repository(
        &DataDir::new(running._directory.path().to_path_buf()).workforce(&workforce),
        item,
    );
    repository.remove_worktree(&agent_repository);
    repository
        .git
        .delete_branch(&wf_vcs::item_branch(item))
        .unwrap();
    assert_eq!(
        live_diff(&running.socket, workforce, item, None).await,
        LiveDiff::Answer(DiffAnswer::NotHere)
    );
}

#[tokio::test]
async fn a_device_reads_the_committed_work_of_a_session_on_another() {
    use wf_daemon::proto::LiveDiff;
    use wf_net::DiffAnswer;

    if wf_core::testing::skip_in_fence("iroh needs the route socket") {
        return;
    }
    let fixture = Fixture::new(2);
    let (_laptop_dir, laptop, laptop_socket) = start_peer(&fixture, 0).await;
    let (vm_dir, vm, vm_socket) = start_peer(&fixture, 1).await;
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

    let item = start_work_session(&vm_socket, workforce).await;
    let repository = wf_vcs::testing::TestRepository::new();
    let head = give_work(vm_dir.path(), workforce, item, &repository);

    // The laptop knows the session once the vm's events arrive.
    let mut known = false;
    for _ in 0..200 {
        let socket = laptop_socket.clone();
        let state = blocking(move || connect(&socket).state(workforce).unwrap()).await;
        if state.holder(item, &SessionRole::Work).is_some() {
            known = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(known, "the laptop learns of the vm's session");

    let LiveDiff::Answer(DiffAnswer::Work(work)) =
        live_diff(&laptop_socket, workforce, item, None).await
    else {
        panic!("the vm answers with its work");
    };
    assert_eq!((work.target.as_str(), work.head), ("main", head));
    assert!(work.committed.contains("+work in progress"));
    assert_eq!(
        live_diff(&laptop_socket, workforce, item, Some(head)).await,
        LiveDiff::Answer(DiffAnswer::Unchanged)
    );

    // The branch and the agent repository are gone, as after the session ended.
    let agent_repository = wf_vcs::item_repository(
        &DataDir::new(vm_dir.path().to_path_buf()).workforce(&workforce),
        item,
    );
    repository.remove_worktree(&agent_repository);
    repository
        .git
        .delete_branch(&wf_vcs::item_branch(item))
        .unwrap();
    assert_eq!(
        live_diff(&laptop_socket, workforce, item, None).await,
        LiveDiff::Answer(DiffAnswer::NotHere)
    );
}

async fn follow_transcript(
    socket: &Path,
    workforce: wf_core::id::WorkforceId,
    session: wf_core::id::SessionId,
) -> Result<wf_daemon::proto::TranscriptCopy, ClientError> {
    let socket = socket.to_path_buf();
    blocking(move || connect(&socket).follow_transcript(workforce, session)).await
}

/// Follows the transcript of `session` through the daemon behind `socket` until its copy is
/// `wanted` with `content`.
async fn follow_until(
    socket: &Path,
    workforce: wf_core::id::WorkforceId,
    session: wf_core::id::SessionId,
    wanted: impl Fn(&wf_daemon::proto::TranscriptCopy) -> bool,
    content: &[u8],
) {
    let copy =
        DataDir::new(socket.parent().unwrap().to_path_buf()).transcript_copy(&workforce, session);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let now = follow_transcript(socket, workforce, session).await.unwrap();
        let copied = std::fs::read(&copy).unwrap_or_default();
        if wanted(&now) && copied == content {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the copy came to {now:?} with {:?}, not what was wanted with {:?}",
            String::from_utf8_lossy(&copied),
            String::from_utf8_lossy(content),
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Waits until the daemon behind `socket` knows `session`.
async fn wait_for_session(
    socket: &Path,
    workforce: wf_core::id::WorkforceId,
    session: wf_core::id::SessionId,
    live: bool,
) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let socket = socket.to_path_buf();
        let state = blocking(move || connect(&socket).state(workforce).unwrap()).await;
        if state
            .session(session)
            .is_some_and(|known| known.is_live() == live)
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the session reaches the other device"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

async fn work_session(
    socket: &Path,
    workforce: wf_core::id::WorkforceId,
    item: wf_core::id::ItemId,
) -> wf_core::id::SessionId {
    let socket = socket.to_path_buf();
    blocking(move || {
        connect(&socket)
            .state(workforce)
            .unwrap()
            .holder(item, &SessionRole::Work)
            .expect("a work session")
            .id()
    })
    .await
}

#[tokio::test]
async fn a_device_follows_the_transcript_of_a_session_on_another() {
    use wf_core::session::EndReason;
    use wf_daemon::proto::TranscriptCopy;

    if wf_core::testing::skip_in_fence("iroh needs the route socket") {
        return;
    }
    let fixture = Fixture::new(2);
    let (laptop_dir, laptop, laptop_socket) = start_peer(&fixture, 0).await;
    let (vm_dir, vm, vm_socket) = start_peer(&fixture, 1).await;
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

    let item = start_work_session(&vm_socket, workforce).await;
    let session = work_session(&vm_socket, workforce, item).await;
    let transcript = DataDir::new(vm_dir.path().to_path_buf()).transcript(&workforce, session);
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    let first = b"{\"type\":\"user\"}\n";
    std::fs::write(&transcript, first).unwrap();
    wait_for_session(&laptop_socket, workforce, session, true).await;

    follow_until(
        &laptop_socket,
        workforce,
        session,
        |copy| *copy == TranscriptCopy::CaughtUp,
        first,
    )
    .await;
    let both = b"{\"type\":\"user\"}\n{\"type\":\"assistant\"}\n";
    std::fs::write(&transcript, both).unwrap();
    follow_until(
        &laptop_socket,
        workforce,
        session,
        |copy| *copy == TranscriptCopy::CaughtUp,
        both,
    )
    .await;
    // The copy is kept apart from the transcripts of the laptop's own sessions.
    let laptop_data = DataDir::new(laptop_dir.path().to_path_buf());
    assert!(!laptop_data.transcript(&workforce, session).exists());

    let socket = vm_socket.clone();
    blocking(move || {
        connect(&socket)
            .author(workforce, end_session(session, EndReason::Cancelled))
            .unwrap()
    })
    .await;
    follow_until(
        &laptop_socket,
        workforce,
        session,
        |copy| *copy == TranscriptCopy::Complete,
        both,
    )
    .await;

    // A copy is a cache: once removed, it is fetched again.
    std::fs::remove_file(laptop_data.transcript_copy(&workforce, session)).unwrap();
    follow_until(
        &laptop_socket,
        workforce,
        session,
        |copy| *copy == TranscriptCopy::Complete,
        both,
    )
    .await;

    // The vm answers for its own sessions only.
    let laptop_item = start_work_session(&laptop_socket, workforce).await;
    let laptop_session = work_session(&laptop_socket, workforce, laptop_item).await;
    wait_for_session(&vm_socket, workforce, laptop_session, true).await;
    let answer = laptop
        .node()
        .await
        .unwrap()
        .query_transcript(
            vm.node().await.unwrap().local_addr(),
            workforce,
            laptop_session,
            0,
        )
        .await
        .unwrap();
    assert_eq!(answer, wf_net::TranscriptAnswer::NotHere);
    // And the laptop keeps no copy of its own session's transcript.
    assert!(
        follow_transcript(&laptop_socket, workforce, laptop_session)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn the_transcript_of_a_device_that_cannot_be_reached_says_so() {
    use wf_daemon::proto::TranscriptCopy;

    if wf_core::testing::skip_in_fence("iroh needs the route socket") {
        return;
    }
    let fixture = Fixture::new(2);
    // The vm started a session and is gone: the laptop has its events, and nobody runs it.
    let vm = fixture.devices[1].clone();
    let mut item = None;
    let (_laptop_dir, _laptop, laptop_socket) =
        start_peer_with(&fixture, fixture.devices[0].clone(), |replica| {
            let actor = fixture.founder_actor();
            replica
                .author(&vm, actor.clone(), 2_000, define_agent("coder"))
                .unwrap();
            let created = replica
                .author(&vm, actor.clone(), 2_001, create("task", "on the vm"))
                .unwrap();
            replica
                .author(
                    &vm,
                    actor,
                    2_002,
                    start_session("coder", created.id().into()),
                )
                .unwrap();
            item = Some(created.id().into());
        })
        .await;
    let workforce = fixture.workforce();
    let session = work_session(&laptop_socket, workforce, item.unwrap()).await;

    follow_until(
        &laptop_socket,
        workforce,
        session,
        |copy| matches!(copy, TranscriptCopy::Unreachable { device, .. } if *device == vm.key()),
        b"",
    )
    .await;
}

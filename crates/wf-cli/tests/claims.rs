//! Claims between the daemons of two devices of one principal: two that claim one item before
//! they sync end one session `Outclaimed`, and a device that went silent has its session taken
//! over after the lease, which it stops once it hears of it, naming its last head. Placement: of
//! two devices that could start an agent on an item, the first by rank starts and the other waits
//! its turn, once it has heard from the first, and takes over when the first is gone.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;
use tracing::instrument::WithSubscriber;
use wf_core::event::{AgentDefined, Event, EventBody};
use wf_core::id::{HashId, ItemId, SessionId, WorkforceId};
use wf_core::identity::DeviceKey;
use wf_core::name::{AgentName, HarnessName};
use wf_core::placement::RANK_DELAY;
use wf_core::session::{CommitId, EndReason, SessionRole};
use wf_core::state::{State, Trust};
use wf_core::testing::*;
use wf_daemon::client::DaemonClient;
use wf_daemon::data_dir::STORE_FILE;
use wf_daemon::{Daemon, DataDir, Launch};
use wf_harness::AcpAgentConfig;
use wf_net::Network;
use wf_store::{RedbStore, Replica};
use wf_vcs::GitCli;
use wf_vcs::testing::TestRepository;

mod common;

use common::{DaemonLog, LOG_TAIL, bare_repository_with_main, serving};

/// Commits once, then works until it is stopped.
const SCRIPT: &str = "\
sh echo work > work.txt && git add work.txt && git -c user.email=coder@example.com commit -q -m work
hang
";

fn fake_agent() -> PathBuf {
    let test = std::env::current_exe().unwrap();
    let examples = test.parent().unwrap().parent().unwrap().join("examples");
    let path = examples.join("fake-acp-agent");
    assert!(path.is_file(), "{} is not built", path.display());
    path
}

/// The agent `coder` on the fake harness, and a task for it.
fn workforce_with_a_task(fixture: &Fixture) -> (Vec<Event>, ItemId) {
    let mut state = fixture.state();
    let coder = EventBody::AgentDefined(AgentDefined {
        name: AgentName::new("coder").unwrap(),
        harness: HarnessName::new("fake").unwrap(),
        model: None,
        instructions: None,
        capabilities: vec![],
        lanes: vec![],
        autonomy: Default::default(),
    });
    let defined = fixture.apply(&mut state, 0, 2_000, coder);
    let created = fixture.apply(&mut state, 0, 2_001, create("task", "claimed twice"));
    let item = created.id().into();
    (vec![defined, created], item)
}

/// The agent `coder`, started by the dispatcher, hosted by both devices, and a task for it; with
/// the two devices in the order they rank for the task.
fn workforce_with_an_auto_agent(fixture: &Fixture) -> (Vec<Event>, ItemId, [usize; 2]) {
    let mut state = fixture.state();
    let coder = EventBody::AgentDefined(AgentDefined {
        name: AgentName::new("coder").unwrap(),
        harness: HarnessName::new("fake").unwrap(),
        model: None,
        instructions: None,
        capabilities: vec![],
        lanes: vec![wf_core::name::LaneName::new("default").unwrap()],
        autonomy: wf_core::autonomy::Autonomy {
            start: wf_core::autonomy::StartMode::Auto,
            ..Default::default()
        },
    });
    let mut events = vec![fixture.apply(&mut state, 0, 2_000, coder)];
    for device in 0..2 {
        let profile = EventBody::DeviceProfile(wf_core::event::DeviceProfile {
            name: format!("device {device}"),
            hardware: "test".into(),
            agents: BTreeMap::from([(AgentName::new("coder").unwrap(), 1)]),
            max_sessions: 1,
        });
        events.push(fixture.apply(&mut state, device, 2_001, profile));
    }
    let created = fixture.apply(&mut state, 0, 2_002, create("task", "placed"));
    let item = created.id().into();
    events.push(created);
    let presence = wf_core::claims::Presence::new(2_002, []);
    let placement = state.placement(fixture.devices[0].key(), &presence, 2_002);
    let ranked = placement.ranked(item, &SessionRole::Work);
    let index = |key| {
        (0..2)
            .find(|device| fixture.devices[*device].key() == key)
            .unwrap()
    };
    assert_eq!(ranked.len(), 2, "both devices could take the task");
    (events, item, [index(ranked[0]), index(ranked[1])])
}

/// The work sessions on `item`, with the device of each.
fn work_sessions(state: &State, item: ItemId) -> Vec<(SessionId, DeviceKey)> {
    state
        .sessions_of(item)
        .filter(|session| session.role() == SessionRole::Work)
        .map(|session| (session.id(), session.device()))
        .collect()
}

struct Device {
    directory: TempDir,
    daemon: Arc<Daemon>,
    socket: PathBuf,
    key: DeviceKey,
    log: DaemonLog,
}

/// A test that fails, by a wait that timed out or an assertion, tells what each daemon was doing.
impl Drop for Device {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!(
                "the end of the daemon log of device {}:\n{}",
                self.key,
                self.log.tail(LOG_TAIL)
            );
        }
    }
}

impl Device {
    /// A daemon for `device` whose store already has the genesis and `events`, and whose bare
    /// repository has the `main` of `repository`.
    async fn start(
        fixture: &Fixture,
        device: usize,
        events: &[Event],
        repository: &TestRepository,
    ) -> Self {
        let directory = TempDir::new().unwrap();
        let store_directory = directory.path().join(fixture.workforce().to_string());
        std::fs::create_dir_all(&store_directory).unwrap();
        let mut replica = Replica::open(
            RedbStore::open(store_directory.join(STORE_FILE)).unwrap(),
            fixture.workforce(),
        )
        .unwrap();
        replica.receive(fixture.genesis.clone()).unwrap();
        for event in events {
            replica.receive(event.clone()).unwrap();
        }
        drop(replica);
        let data_dir = DataDir::new(directory.path().to_path_buf());
        bare_repository_with_main(&data_dir, fixture.workforce(), repository);
        let script = directory.path().join("script");
        std::fs::write(&script, SCRIPT).unwrap();
        let machine_config = directory.path().join("config.toml");
        std::fs::write(&machine_config, "[sandbox]\nkind = \"off\"\n").unwrap();
        let harnesses = BTreeMap::from([(
            HarnessName::new("fake").unwrap(),
            AcpAgentConfig::new(fake_agent()).env("FAKE_AGENT_SCRIPT", script.to_string_lossy()),
        )]);
        let socket = directory.path().join("daemon.sock");
        let log = DaemonLog::default();
        let dispatch = log.dispatch();
        let daemon = Daemon::start(
            Launch {
                data_dir,
                socket: socket.clone(),
                network: Network::Direct,
                harnesses,
                machine_config: Some(machine_config),
                // The bridge `git fetch` runs.
                wf_binary: Some(PathBuf::from(env!("CARGO_BIN_EXE_wf"))),
            },
            fixture.devices[device].clone(),
        )
        .with_subscriber(dispatch.clone())
        .await
        .unwrap();
        tokio::spawn(
            Arc::clone(&daemon)
                .serve(std::future::pending())
                .with_subscriber(dispatch),
        );
        serving(&socket).await;
        Self {
            directory,
            daemon,
            socket,
            key: fixture.devices[device].key(),
            log,
        }
    }

    /// A client of this device's daemon. Connecting blocks until the daemon answers the
    /// handshake, so it runs off the runtime's threads, which serve the daemon.
    async fn client(&self) -> DaemonClient {
        let token = DataDir::new(self.directory.path().to_path_buf()).read_principal_token();
        let socket = self.socket.clone();
        within(
            "connecting to the daemon",
            blocking(move || {
                DaemonClient::connect(&socket, token)
                    .unwrap()
                    .expect("a daemon")
            }),
        )
        .await
    }

    async fn state(&self, workforce: WorkforceId) -> State {
        let mut client = self.client().await;
        within(
            "reading the state",
            blocking(move || client.state(workforce).unwrap()),
        )
        .await
    }

    /// Starts a session of `coder` on `item` as `wf session start` does.
    async fn start_session(
        &self,
        workforce: WorkforceId,
        item: ItemId,
    ) -> Result<SessionId, String> {
        let mut client = self.client().await;
        within(
            "starting a session",
            blocking(move || {
                client
                    .start_session(workforce, AgentName::new("coder").unwrap(), item, None)
                    .map_err(|error| wf_core::error_chain(&error))
            }),
        )
        .await
    }

    async fn sync_now(&self, workforce: WorkforceId) {
        let mut client = self.client().await;
        within(
            "syncing",
            blocking(move || client.sync_now(workforce).unwrap()),
        )
        .await;
    }

    /// Waits until the state of this device's daemon passes `check`, for up to a minute.
    async fn wait_for(&self, workforce: WorkforceId, what: &str, check: impl Fn(&State) -> bool) {
        for _ in 0..600 {
            if check(&self.state(workforce).await) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("{what} did not happen within a minute");
    }

    /// Waits until the agent working on `item` here has made its commit.
    async fn committed(&self, workforce: WorkforceId, item: ItemId) {
        let repository = wf_vcs::item_repository(
            &DataDir::new(self.directory.path().to_path_buf()).workforce(&workforce),
            item,
        );
        eventually("the agent commits", || {
            std::process::Command::new("git")
                .args(["log", "-1", "--format=%s"])
                .current_dir(&repository)
                .output()
                .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).trim() == "work")
        })
        .await;
    }

    fn bare(&self, workforce: WorkforceId) -> PathBuf {
        wf_vcs::bare_repository(
            &DataDir::new(self.directory.path().to_path_buf()).workforce(&workforce),
        )
    }
}

async fn blocking<T: Send + 'static>(job: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(job).await.unwrap()
}

/// Awaits `future` for up to a minute, so that a hang fails the test with `what` instead of
/// running forever.
async fn within<T>(what: &str, future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(60), future)
        .await
        .unwrap_or_else(|_| panic!("{what} did not finish within a minute"))
}

/// Waits until `check` holds, for up to 30 seconds.
async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..300 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("{what} did not happen");
}

/// Lets the nodes of the two devices find each other.
async fn connect(one: &Device, other: &Device) {
    let one_node = within("a node", one.daemon.node()).await.unwrap();
    let other_node = within("a node", other.daemon.node()).await.unwrap();
    one_node.remember(other_node.local_addr());
    other_node.remember(one_node.local_addr());
}

fn reason(state: &State, session: SessionId) -> Option<EndReason> {
    state
        .session(session)
        .and_then(|folded| folded.ended())
        .map(|ended| ended.reason)
}

fn has_commit(bare: &std::path::Path, commit: CommitId) -> bool {
    GitCli::open(bare)
        .and_then(|git| git.has_commit(commit))
        .unwrap_or(false)
}

// Multi-threaded: the daemons serve on the runtime while the test waits on them.
#[tokio::test(flavor = "multi_thread")]
async fn a_takeover_whose_start_fails_leaves_the_item_open_for_the_next_start() {
    // One daemon, so it runs in Fence too: the vm's session is in the log, and the vm is never
    // heard from.
    let fixture = Fixture::new(2);
    let workforce = fixture.workforce();
    let (mut events, item) = workforce_with_a_task(&fixture);
    let mut state = fixture.state();
    for event in &events {
        state.receive(event.clone(), Trust::Untrusted).unwrap();
    }
    let started = fixture.apply(&mut state, 1, 3_000, start_session("coder", item));
    let on_vm = SessionId::from(started.id());
    let active = fixture.apply(&mut state, 1, 3_001, moved(item, ItemState::Active));
    events.extend([started, active]);
    let repository = TestRepository::new();
    let laptop = within(
        "starting the laptop",
        Device::start(&fixture, 0, &events, &repository),
    )
    .await;
    let lease = laptop.state(workforce).await.lease();
    laptop
        .daemon
        .backdate_presence(lease + Duration::from_secs(60));

    laptop
        .daemon
        .fail_next_write(|body| matches!(body, EventBody::SessionStarted(_)));
    laptop
        .start_session(workforce, item)
        .await
        .expect_err("the write of the start fails");
    let state = laptop.state(workforce).await;
    assert_eq!(reason(&state, on_vm), Some(EndReason::LeaseExpired));
    assert_eq!(state.live_state(item), Some(ItemState::Open));

    let on_laptop = laptop.start_session(workforce, item).await.unwrap();
    let state = laptop.state(workforce).await;
    assert!(state.session(on_laptop).unwrap().is_live());
    assert_eq!(state.live_state(item), Some(ItemState::Active));
    laptop.daemon.halt().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn two_devices_that_claim_one_item_before_they_sync_end_the_later_session_outclaimed() {
    let fixture = Fixture::new(2);
    let workforce = fixture.workforce();
    let (events, item) = workforce_with_a_task(&fixture);
    let repository = TestRepository::new();
    let laptop = within(
        "starting the laptop",
        Device::start(&fixture, 0, &events, &repository),
    )
    .await;
    let vm = within(
        "starting the vm",
        Device::start(&fixture, 1, &events, &repository),
    )
    .await;

    // Neither can reach the other yet, so each claims the item.
    let on_laptop = laptop.start_session(workforce, item).await.unwrap();
    let on_vm = vm.start_session(workforce, item).await.unwrap();
    laptop.committed(workforce, item).await;
    vm.committed(workforce, item).await;
    connect(&laptop, &vm).await;
    laptop.sync_now(workforce).await;

    let state = laptop.state(workforce).await;
    let earlier =
        state.session(on_laptop).unwrap().started() < state.session(on_vm).unwrap().started();
    let (holder, loser, losing_device) = if earlier {
        (on_laptop, on_vm, &vm)
    } else {
        (on_vm, on_laptop, &laptop)
    };
    for device in [&laptop, &vm] {
        device
            .wait_for(workforce, "the later session ends Outclaimed", |state| {
                reason(state, loser) == Some(EndReason::Outclaimed)
            })
            .await;
    }
    let state = losing_device.state(workforce).await;
    assert!(state.session(holder).unwrap().is_live());
    assert!(
        state.session(loser).unwrap().head().is_some(),
        "the lost session keeps its commit, named at its end"
    );
    laptop.daemon.halt().await;
    vm.daemon.halt().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_silent_device_is_taken_over_after_the_lease_and_stops_its_session_when_it_hears() {
    let fixture = Fixture::new(2);
    let workforce = fixture.workforce();
    let (events, item) = workforce_with_a_task(&fixture);
    let repository = TestRepository::new();
    let laptop = within(
        "starting the laptop",
        Device::start(&fixture, 0, &events, &repository),
    )
    .await;
    let vm = within(
        "starting the vm",
        Device::start(&fixture, 1, &events, &repository),
    )
    .await;
    connect(&laptop, &vm).await;

    let on_vm = vm.start_session(workforce, item).await.unwrap();
    vm.committed(workforce, item).await;
    laptop
        .wait_for(workforce, "the vm's session arrives", |state| {
            state.session(on_vm).is_some()
        })
        .await;
    let refused = laptop
        .start_session(workforce, item)
        .await
        .expect_err("the vm is there, so its claim holds");
    assert!(
        refused.contains("already has a live work session"),
        "{refused}"
    );

    // The vm has not been heard from for longer than the lease, as far as the laptop knows. The
    // vm still syncs, so what it hears ends the silence at once; a start that it beats to it is
    // tried again.
    let lease = laptop.state(workforce).await.lease();
    let mut taken_over = None;
    for _ in 0..20 {
        laptop
            .daemon
            .backdate_presence(lease + Duration::from_secs(60));
        match laptop.start_session(workforce, item).await {
            Ok(session) => {
                taken_over = Some(session);
                break;
            }
            Err(error) => assert!(error.contains("already has a live work session"), "{error}"),
        }
    }
    let on_laptop = taken_over.expect("the laptop takes the vm's session over");
    let state = laptop.state(workforce).await;
    assert_eq!(reason(&state, on_vm), Some(EndReason::LeaseExpired));
    assert!(state.session(on_laptop).unwrap().is_live());
    let note = state
        .item(item)
        .and_then(|entry| match entry {
            wf_core::item::ItemEntry::Live(live) => Some(live.comments().len()),
            wf_core::item::ItemEntry::Archived(_) => None,
        })
        .unwrap();
    assert!(note > 0, "the takeover says why");

    // The vm hears of it, stops its session and names the commit its agent made, which then
    // reaches the laptop.
    laptop
        .wait_for(
            workforce,
            "the vm names the last head of its session",
            |state| {
                state
                    .session(on_vm)
                    .is_some_and(|folded| folded.head().is_some())
            },
        )
        .await;
    let head = laptop
        .state(workforce)
        .await
        .session(on_vm)
        .unwrap()
        .head()
        .unwrap();
    let laptop_bare = laptop.bare(workforce);
    eventually("the head arrives on the laptop", || {
        has_commit(&laptop_bare, head)
    })
    .await;
    let vm_state = vm.state(workforce).await;
    assert_eq!(reason(&vm_state, on_vm), Some(EndReason::LeaseExpired));
    assert!(
        vm_state.session(on_laptop).unwrap().is_live(),
        "the vm does not end the session that took over"
    );
    laptop.daemon.halt().await;
    vm.daemon.halt().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_device_ranked_second_waits_until_it_hears_from_the_first_and_takes_over_once_it_is_gone()
 {
    // One daemon, so it runs in Fence too: the device ranked first is never heard from, so a claim
    // of it could not have arrived, however long the device ranked second has waited its turn.
    let fixture = Fixture::new(2);
    let workforce = fixture.workforce();
    let (events, item, [first, second]) = workforce_with_an_auto_agent(&fixture);
    let repository = TestRepository::new();
    let device = within(
        "starting the device ranked second",
        Device::start(&fixture, second, &events, &repository),
    )
    .await;

    // Its dispatcher passes the task over until it hears from the first: it has no turn to wait
    // out, after which it would start.
    let hears = format!(
        "coder waits for #{} until this device hears from {}, ranked ahead",
        item.short(),
        fixture.devices[first].key().fmt_short()
    );
    eventually(
        "the device ranked second waits to hear from the first",
        || device.log.contains(&hears),
    )
    .await;
    assert!(
        work_sessions(&device.state(workforce).await, item).is_empty(),
        "the device ranked second waits to hear from the first"
    );

    // The device ranked first has not been heard from for longer than the lease.
    let lease = device.state(workforce).await.lease();
    device
        .daemon
        .backdate_presence(lease + Duration::from_secs(60));
    device
        .wait_for(workforce, "the device ranked second takes over", |state| {
            !work_sessions(state, item).is_empty()
        })
        .await;
    assert_eq!(
        work_sessions(&device.state(workforce).await, item)
            .into_iter()
            .map(|(_, on)| on)
            .collect::<Vec<_>>(),
        [fixture.devices[second].key()]
    );
    device.daemon.halt().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn of_two_devices_that_could_start_an_item_the_first_by_rank_does_and_the_other_does_not() {
    let fixture = Fixture::new(2);
    let workforce = fixture.workforce();
    let (events, item, [first, second]) = workforce_with_an_auto_agent(&fixture);
    let repository = TestRepository::new();
    let devices = [
        within(
            "starting device 0",
            Device::start(&fixture, 0, &events, &repository),
        )
        .await,
        within(
            "starting device 1",
            Device::start(&fixture, 1, &events, &repository),
        )
        .await,
    ];
    let started = std::time::Instant::now();
    connect(&devices[0], &devices[1]).await;

    devices[first]
        .wait_for(workforce, "the device ranked first starts", |state| {
            !work_sessions(state, item).is_empty()
        })
        .await;
    assert!(
        started.elapsed() < RANK_DELAY,
        "the device ranked first does not wait"
    );
    let [(session, on)] = work_sessions(&devices[first].state(workforce).await, item)[..] else {
        panic!("one work session");
    };
    assert_eq!(on, fixture.devices[first].key());
    devices[second]
        .wait_for(workforce, "the claim reaches the other device", |state| {
            state.session(session).is_some()
        })
        .await;

    // The claim leaves the device ranked second nothing to start, so it has no turn left to wait
    // out, after which it would start.
    eventually("the device ranked second passes the task over", || {
        devices[second].log.contains("coder has nothing to start")
    })
    .await;
    for device in &devices {
        let state = device.state(workforce).await;
        assert_eq!(work_sessions(&state, item), [(session, on)]);
        assert!(state.session(session).unwrap().is_live());
    }
    for device in devices {
        device.daemon.halt().await;
    }
}

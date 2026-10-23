//! Comments reach a session across devices (SPEC §9, What a session is told): a comment written on
//! one device syncs to the device that runs the session, which tells it at the end of the
//! session's turn.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;
use wf_core::event::{AgentDefined, Event, EventBody};
use wf_core::id::{ItemId, SessionId, WorkforceId};
use wf_core::name::{AgentName, HarnessName};
use wf_core::state::State;
use wf_core::testing::*;
use wf_daemon::client::DaemonClient;
use wf_daemon::data_dir::STORE_FILE;
use wf_daemon::{Daemon, DataDir, Launch};
use wf_harness::AcpAgentConfig;
use wf_net::Network;
use wf_store::{RedbStore, Replica};
use wf_vcs::testing::TestRepository;

mod common;

use common::{bare_repository_with_main, serving};

fn fake_agent() -> PathBuf {
    let test = std::env::current_exe().unwrap();
    let examples = test.parent().unwrap().parent().unwrap().join("examples");
    let path = examples.join("fake-acp-agent");
    assert!(path.is_file(), "{} is not built", path.display());
    path
}

/// The agent `coder` on the fake harness, a task for it and another item.
fn workforce_with_two_items(fixture: &Fixture) -> (Vec<Event>, ItemId, ItemId) {
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
    let task = fixture.apply(&mut state, 0, 2_001, create("task", "worked on"));
    let other = fixture.apply(&mut state, 0, 2_002, create("task", "elsewhere"));
    let items = (task.id().into(), other.id().into());
    (vec![defined, task, other], items.0, items.1)
}

struct Device {
    directory: TempDir,
    daemon: Arc<Daemon>,
    socket: PathBuf,
}

impl Device {
    /// A daemon for `device` whose store already has the genesis and `events`, whose bare
    /// repository has the `main` of `repository`, and whose agent follows `script`.
    async fn start(
        fixture: &Fixture,
        device: usize,
        events: &[Event],
        repository: &TestRepository,
        script: &str,
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
        let script_file = directory.path().join("script");
        std::fs::write(&script_file, script).unwrap();
        let machine_config = directory.path().join("config.toml");
        std::fs::write(&machine_config, "[sandbox]\nkind = \"off\"\n").unwrap();
        let harnesses = BTreeMap::from([(
            HarnessName::new("fake").unwrap(),
            AcpAgentConfig::new(fake_agent())
                .env("FAKE_AGENT_SCRIPT", script_file.to_string_lossy()),
        )]);
        let socket = directory.path().join("daemon.sock");
        let daemon = Daemon::start(
            Launch {
                data_dir,
                socket: socket.clone(),
                network: Network::Direct,
                harnesses,
                machine_config: Some(machine_config),
                wf_binary: Some(PathBuf::from(env!("CARGO_BIN_EXE_wf"))),
            },
            fixture.devices[device].clone(),
        )
        .await
        .unwrap();
        tokio::spawn(Arc::clone(&daemon).serve(std::future::pending()));
        serving(&socket).await;
        Self {
            directory,
            daemon,
            socket,
        }
    }

    fn data_dir(&self) -> DataDir {
        DataDir::new(self.directory.path().to_path_buf())
    }

    /// A client of this device's daemon. Connecting blocks until the daemon answers the
    /// handshake, so it runs off the runtime's threads, which serve the daemon.
    async fn client(&self) -> DaemonClient {
        let token = self.data_dir().read_principal_token();
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

    /// Writes `body` as the principal, as `wf comment` does.
    async fn author(&self, workforce: WorkforceId, body: EventBody) {
        let mut client = self.client().await;
        within(
            "writing an event",
            blocking(move || client.author(workforce, body).unwrap()),
        )
        .await;
    }

    async fn sync_now(&self, workforce: WorkforceId) {
        let mut client = self.client().await;
        within(
            "syncing",
            blocking(move || client.sync_now(workforce).unwrap()),
        )
        .await;
    }

    /// Starts a session of `coder` on `item` as `wf session start` does.
    async fn start_session(&self, workforce: WorkforceId, item: ItemId) -> SessionId {
        let mut client = self.client().await;
        within(
            "starting a session",
            blocking(move || {
                client
                    .start_session(workforce, AgentName::new("coder").unwrap(), item, None)
                    .unwrap()
            }),
        )
        .await
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

    /// The transcript of `session` as this device keeps it.
    fn transcript(&self, workforce: WorkforceId, session: SessionId) -> String {
        std::fs::read_to_string(self.data_dir().transcript(&workforce, session)).unwrap_or_default()
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

/// How many comments `item` has.
fn comments_on(state: &State, item: ItemId) -> usize {
    match state.item(item) {
        Some(wf_core::item::ItemEntry::Live(item)) => item.comments().len(),
        Some(wf_core::item::ItemEntry::Archived(_)) | None => 0,
    }
}

/// A script whose first turn goes on until `go` exists, and whose next turn hangs.
fn waiting_script(go: &Path) -> String {
    format!(
        "say started\nsh while [ ! -f '{}' ]; do sleep 0.1; done\nturn\nhang\n",
        go.display()
    )
}

// Multi-threaded: the daemons serve on the runtime while the test waits on them.
#[tokio::test(flavor = "multi_thread")]
async fn a_comment_written_on_one_device_reaches_a_session_on_the_other_at_the_end_of_its_turn() {
    if wf_core::testing::skip_in_fence("iroh needs the route socket") {
        return;
    }
    let fixture = Fixture::new(2);
    let workforce = fixture.workforce();
    let (events, task, other) = workforce_with_two_items(&fixture);
    let repository = TestRepository::new();
    let go = TempDir::new().unwrap();
    let go = go.path().join("go");
    let script = waiting_script(&go);
    let laptop = within(
        "starting the laptop",
        Device::start(&fixture, 0, &events, &repository, &script),
    )
    .await;
    let vm = within(
        "starting the vm",
        Device::start(&fixture, 1, &events, &repository, &script),
    )
    .await;
    connect(&laptop, &vm).await;

    let session = laptop.start_session(workforce, task).await;
    eventually("the first turn", || {
        laptop.transcript(workforce, session).contains("started")
    })
    .await;
    vm.author(workforce, comment(task, "Change of plan: use humantime"))
        .await;
    vm.author(workforce, comment(other, "@ada/coder the API moved"))
        .await;
    vm.sync_now(workforce).await;
    laptop
        .wait_for(workforce, "the comments sync to the laptop", |state| {
            comments_on(state, task) == 1 && comments_on(state, other) == 1
        })
        .await;
    assert!(
        !laptop
            .transcript(workforce, session)
            .contains("Change of plan"),
        "nothing is told during a turn"
    );
    std::fs::write(&go, "").unwrap();

    eventually("the end of the turn tells the comments", || {
        laptop
            .transcript(workforce, session)
            .contains("Written for you during your turn")
    })
    .await;
    let transcript = laptop.transcript(workforce, session);
    assert!(
        transcript.contains("Change of plan: use humantime"),
        "{transcript}"
    );
    assert!(
        transcript.contains(&format!(
            "on #{} elsewhere",
            wf_core::id::HashId::short(&other)
        )),
        "{transcript}"
    );
    assert!(transcript.contains("the API moved"), "{transcript}");
    laptop.daemon.halt().await;
    vm.daemon.halt().await;
}

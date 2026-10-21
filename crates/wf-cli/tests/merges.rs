//! Merges between the daemons of two devices of one principal (SPEC §10): two that reach each
//! other merge an item once, the device whose claim is earliest; two that merged onto the same tip
//! while cut off from each other agree on the chain once they meet, and the one that lost resets
//! its target to the winner, merges its item again on top, checks included, and has the principal
//! hear of a push it lost with it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;
use wf_core::claims::Presence;
use wf_core::event::{Event, EventBody};
use wf_core::id::{ItemId, SessionId, WorkforceId};
use wf_core::identity::Actor;
use wf_core::item::{Item, ItemEntry, ItemState};
use wf_core::session::CommitId;
use wf_core::state::State;
use wf_core::target::MoveCause;
use wf_core::testing::*;
use wf_daemon::client::DaemonClient;
use wf_daemon::data_dir::STORE_FILE;
use wf_daemon::{Daemon, DataDir, Launch};
use wf_net::Network;
use wf_store::{RedbStore, Replica};
use wf_vcs::GitCli;
use wf_vcs::testing::{TestRepository, git_in};

mod common;

use common::{bare_repository_with_main, serving};

/// The policy of the fixture, whose work needs no review, with a check that writes down the
/// directory it runs in, the check worktree of the device that runs it, in `runs`.
fn policy_with_a_check(runs: &Path) -> String {
    let policy = wf_core::policy::DEFAULT_POLICY_TOML.replace("review = \"manual\"\n", "");
    format!(
        "{policy}\n[[merge.check]]\nname = \"counted\"\nrun = [\"sh\", \"-c\", '''pwd >> \"{}\"''']\n",
        runs.display()
    )
}

/// Tasks submitted at `commits`, one each, which wait in `review` for their merge.
fn submitted_tasks(
    fixture: &Fixture,
    policy: &str,
    commits: &[CommitId],
) -> (Vec<Event>, Vec<ItemId>) {
    let mut state = fixture.state();
    let set = fixture.policy_set(&state, 0, 2_000, &fixture.founder, policy);
    let mut events = vec![
        fixture.apply(&mut state, 0, 2_000, set),
        fixture.apply(&mut state, 0, 2_001, define_agent("coder")),
    ];
    let mut items = Vec::new();
    for (index, commit) in (0..).zip(commits) {
        let at = 3_000 + 10 * index;
        let created = fixture.apply(&mut state, 0, at, create("task", &format!("work {index}")));
        let item: ItemId = created.id().into();
        let started = fixture.apply(&mut state, 0, at + 1, start_session("coder", item));
        let session = SessionId::from(started.id());
        let review = fixture.apply(&mut state, 0, at + 2, moved(item, ItemState::Review));
        let submitted = fixture.apply(&mut state, 0, at + 3, submit_session(session, *commit));
        events.extend([created, started, review, submitted]);
        items.push(item);
    }
    (events, items)
}

struct Device {
    directory: TempDir,
    daemon: Arc<Daemon>,
    socket: PathBuf,
    workforce: WorkforceId,
}

impl Device {
    /// A daemon for `device` whose store already has the genesis and `events`, and whose bare
    /// repository has the `main` of `repository`. It merges unsigned, and runs checks without a
    /// sandbox.
    async fn start(
        fixture: &Fixture,
        device: usize,
        events: &[Event],
        repository: &TestRepository,
    ) -> Self {
        let directory = TempDir::new().unwrap();
        let workforce = fixture.workforce();
        let store_directory = directory.path().join(workforce.to_string());
        std::fs::create_dir_all(&store_directory).unwrap();
        let mut replica = Replica::open(
            RedbStore::open(store_directory.join(STORE_FILE)).unwrap(),
            workforce,
        )
        .unwrap();
        replica.receive(fixture.genesis.clone()).unwrap();
        for event in events {
            replica.receive(event.clone()).unwrap();
        }
        drop(replica);
        let data_dir = DataDir::new(directory.path().to_path_buf());
        bare_repository_with_main(&data_dir, workforce, repository);
        let machine_config = directory.path().join("config.toml");
        std::fs::write(
            &machine_config,
            "[sandbox]\nkind = \"off\"\n\n[commits]\nmerge_signing = \"unsigned\"\n",
        )
        .unwrap();
        let socket = directory.path().join("daemon.sock");
        let daemon = Daemon::start(
            Launch {
                data_dir,
                socket: socket.clone(),
                network: Network::Direct,
                harnesses: BTreeMap::new(),
                machine_config: Some(machine_config),
                // The bridge `git fetch` runs.
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
            workforce,
        }
    }

    /// Fetches `branch` of `repository` into `reference` of the bare repository.
    fn fetch(&self, repository: &TestRepository, branch: &str, reference: &str) {
        let source = repository.directory().display().to_string();
        let refspec = format!("+refs/heads/{branch}:{reference}");
        git_in(&self.bare(), &["fetch", "--quiet", &source, &refspec]);
    }

    /// Fetches `branch` of `repository`, the work of `item`, and comments on the item, so that the
    /// dispatcher sees at once that the commit is here rather than on its next round.
    async fn receive_work(&self, repository: &TestRepository, branch: &str, item: ItemId) {
        self.fetch(repository, branch, &format!("refs/test/{branch}"));
        self.author(comment(item, "the work is here")).await;
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

    async fn state(&self) -> State {
        let mut client = self.client().await;
        let workforce = self.workforce;
        within(
            "reading the state",
            blocking(move || client.state(workforce).unwrap()),
        )
        .await
    }

    /// Has the daemon author `body` as the principal.
    async fn author(&self, body: EventBody) {
        let mut client = self.client().await;
        let workforce = self.workforce;
        within(
            "authoring",
            blocking(move || client.author(workforce, body).unwrap()),
        )
        .await;
    }

    async fn sync_now(&self) {
        let mut client = self.client().await;
        let workforce = self.workforce;
        within(
            "syncing",
            blocking(move || client.sync_now(workforce).unwrap()),
        )
        .await;
    }

    /// Waits until the state of this device's daemon passes `check`, for up to two minutes.
    async fn wait_for(&self, what: &str, check: impl Fn(&State) -> bool) -> State {
        for _ in 0..1_200 {
            let state = self.state().await;
            if check(&state) {
                return state;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("{what} did not happen within two minutes");
    }

    fn bare(&self) -> PathBuf {
        wf_vcs::bare_repository(
            &DataDir::new(self.directory.path().to_path_buf()).workforce(&self.workforce),
        )
    }

    /// Where the target branch of the bare repository is.
    fn main(&self) -> CommitId {
        GitCli::open(&self.bare())
            .unwrap()
            .branch_head("main")
            .unwrap()
    }

    /// How often the check ran on this device, as the directories it wrote down say.
    fn check_runs(&self, runs: &Path) -> usize {
        let directory = self.directory.path().canonicalize().unwrap();
        std::fs::read_to_string(runs)
            .unwrap_or_default()
            .lines()
            .map(|line| {
                Path::new(line)
                    .canonicalize()
                    .unwrap_or_else(|_| line.into())
            })
            .filter(|line| line.starts_with(&directory))
            .count()
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

fn has_commit(bare: &Path, commit: CommitId) -> bool {
    GitCli::open(bare)
        .and_then(|git| git.has_commit(commit))
        .unwrap_or(false)
}

fn has_file(bare: &Path, commit: CommitId, file: &str) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(bare)
        .args(["cat-file", "-e", &format!("{commit}:{file}")])
        .status()
        .is_ok_and(|status| status.success())
}

fn live(state: &State, item: ItemId) -> &Item {
    match state.item(item) {
        Some(ItemEntry::Live(live)) => live,
        _ => panic!("the item is live"),
    }
}

/// Whether `item` is done with a merge that stands.
fn merged(state: &State, item: ItemId) -> bool {
    state.live_state(item) == Some(ItemState::Done) && live(state, item).merges().count() == 1
}

// Multi-threaded: the daemons serve on the runtime while the test waits on them.
#[tokio::test(flavor = "multi_thread")]
async fn two_devices_that_reach_each_other_merge_an_item_once_by_the_earliest_claim() {
    let fixture = Fixture::new(2);
    let checks = TempDir::new().unwrap();
    let runs = checks.path().canonicalize().unwrap().join("runs.txt");
    let repository = TestRepository::new();
    let work = repository.work("work", "work.txt", "work\n");
    let (events, items) = submitted_tasks(&fixture, &policy_with_a_check(&runs), &[work]);
    let item = items[0];
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
    connect(&devices[0], &devices[1]).await;
    for device in &devices {
        device.sync_now().await;
    }

    // The approved commit reaches both at once, so that both claim the merge.
    tokio::join!(
        devices[0].receive_work(&repository, "work", item),
        devices[1].receive_work(&repository, "work", item),
    );
    for device in &devices {
        let bare = device.bare();
        device
            .wait_for("the merge", |state| {
                merged(state, item) && state.target().is_some_and(|tip| has_commit(&bare, tip))
            })
            .await;
    }
    // Long enough for a second merge, had the other device made one, and for <wf>/git to follow.
    tokio::time::sleep(Duration::from_secs(10)).await;

    let now_ms = wf_store::now_ms();
    let everyone_there = Presence::new(
        0,
        fixture.devices.iter().map(|device| (device.key(), now_ms)),
    );
    let mut targets = Vec::new();
    for device in &devices {
        let state = device.state().await;
        let merges: Vec<_> = live(&state, item).all_merges().collect();
        assert_eq!(merges.len(), 1, "one Merged: {merges:?}");
        let holder = state
            .merge_holder(
                item,
                work,
                fixture.devices[0].key(),
                &everyone_there,
                now_ms,
            )
            .expect("a claim holds the merge")
            .device;
        let tip = state.target_tip().expect("the merge moved the target");
        assert_eq!(tip.cause, MoveCause::Merge { item });
        assert_eq!(
            tip.device, holder,
            "the device whose claim is earliest merged"
        );
        assert_eq!(state.target_moves().count(), 1);
        assert_eq!(device.main(), tip.commit, "<wf>/git is at the merge");
        targets.push(tip.commit);
    }
    assert_eq!(targets[0], targets[1]);
    for device in devices {
        device.daemon.halt().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fork_made_while_cut_off_resolves_and_the_lost_merge_runs_again_on_the_winner() {
    let fixture = Fixture::new(2);
    let principal = fixture.founder.key();
    let checks = TempDir::new().unwrap();
    let runs = checks.path().canonicalize().unwrap().join("runs.txt");
    let repository = TestRepository::new();
    let first_work = repository.work("first", "first.txt", "first\n");
    let second_work = repository.work("second", "second.txt", "second\n");
    let (events, items) = submitted_tasks(
        &fixture,
        &policy_with_a_check(&runs),
        &[first_work, second_work],
    );
    let [first, second] = [items[0], items[1]];
    let [winner, loser] = [
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
    let main = winner.main();

    // Cut off from each other, each has the work of one task and merges it onto `main`: device 0
    // first, so that its merge wins the fork.
    winner.receive_work(&repository, "first", first).await;
    let state = winner
        .wait_for("device 0 merges the first task", |state| {
            merged(state, first)
        })
        .await;
    let won = state.target_tip().unwrap();
    assert_eq!(
        (won.onto, won.cause),
        (main, MoveCause::Merge { item: first })
    );
    let won = won.commit;
    loser.receive_work(&repository, "second", second).await;
    let state = loser
        .wait_for("device 1 merges the second task", |state| {
            merged(state, second)
        })
        .await;
    let lost = state.target_tip().unwrap();
    assert_eq!(
        (lost.onto, lost.cause),
        (main, MoveCause::Merge { item: second })
    );
    let (lost_event, lost) = (lost.event(), lost.commit);
    let runs_before = loser.check_runs(&runs);
    assert!(runs_before > 0, "the merge ran the check");

    // The principal pushes to device 1 on top of its merge.
    let bare = loser.bare();
    let tree = git_in(&bare, &["rev-parse", &format!("{lost}^{{tree}}")]);
    let parent = lost.to_string();
    let pushed = git_in(
        &bare,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit-tree",
            &tree,
            "-p",
            &parent,
            "-m",
            "pushed",
        ],
    );
    git_in(&bare, &["update-ref", "refs/heads/main", &pushed, &parent]);
    let pushed = CommitId::Git(pushed.parse().unwrap());
    let state = loser
        .wait_for("device 1 writes the push down", |state| {
            state.target() == Some(pushed)
        })
        .await;
    let push = state.target_tip().unwrap();
    assert_eq!((push.onto, push.cause), (lost, MoveCause::Push));
    let push_event = push.event();

    connect(&winner, &loser).await;
    winner.sync_now().await;
    loser.sync_now().await;

    // The second task goes back to review on device 1, and is merged again onto the winner.
    let merged_again = |state: &State| {
        merged(state, second)
            && live(state, second).all_merges().count() == 2
            && state.target_tip().is_some_and(|tip| {
                tip.cause == MoveCause::Merge { item: second } && tip.onto == won
            })
    };
    let mut tips = Vec::new();
    for device in [&winner, &loser] {
        let state = device
            .wait_for("the second task merges again", merged_again)
            .await;
        let tip = state.target_tip().unwrap();
        eventually("<wf>/git follows the tip", || device.main() == tip.commit).await;
        assert!(merged(&state, first));
        assert!(state.move_lost(lost_event), "the later merge lost the fork");
        assert!(
            state.move_lost(push_event),
            "the push on top of it lost with it"
        );
        let lost_pushes: Vec<_> = state
            .lost_pushes(principal)
            .iter()
            .map(|push| push.event())
            .collect();
        assert_eq!(
            lost_pushes,
            [push_event],
            "the principal's inbox has the lost push"
        );
        let lost_at = live(&state, second)
            .all_merges()
            .map(|merge| merge.merged_at())
            .min()
            .unwrap();
        let said = live(&state, second)
            .comments()
            .values()
            .any(|comment| matches!(comment.by, Actor::Daemon(_)) && comment.at > lost_at);
        assert!(said, "device 1 says why the task went back to review");
        tips.push((tip.event(), tip.commit));
    }
    assert_eq!(tips[0], tips[1], "both devices agree on the chain");
    let target = tips[0].1;
    for device in [&winner, &loser] {
        assert!(has_file(&device.bare(), target, "first.txt"));
        assert!(has_file(&device.bare(), target, "second.txt"));
    }
    assert!(
        loser.check_runs(&runs) > runs_before,
        "the merge on the winner ran the check again"
    );
    winner.daemon.halt().await;
    loser.daemon.halt().await;
}

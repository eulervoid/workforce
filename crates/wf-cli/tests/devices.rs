//! Commits between the daemons of two devices, the whole way: the daemon that lacks a commit the
//! log names runs `git fetch` through `wf git-bridge` (the real binary), over `wf/git/1`, to the
//! other daemon's `git upload-pack` on its bare repository.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;
use wf_core::id::{SessionId, WorkforceId};
use wf_core::session::{CommitId, EndReason};
use wf_core::testing::*;
use wf_daemon::client::DaemonClient;
use wf_daemon::data_dir::STORE_FILE;
use wf_daemon::{Daemon, DataDir, Launch};
use wf_net::Network;
use wf_store::{RedbStore, Replica};
use wf_vcs::GitCli;
use wf_vcs::testing::{TestRepository, git_in};

struct Device {
    directory: TempDir,
    daemon: Arc<Daemon>,
    socket: PathBuf,
}

impl Device {
    async fn start(fixture: &Fixture, device: usize) -> Self {
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
                // The bridge `git fetch` runs.
                wf_binary: Some(PathBuf::from(env!("CARGO_BIN_EXE_wf"))),
            },
            fixture.devices[device].clone(),
        )
        .await
        .unwrap();
        tokio::spawn(Arc::clone(&daemon).serve(std::future::pending()));
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Self {
            directory,
            daemon,
            socket,
        }
    }

    fn client(&self) -> DaemonClient {
        let token = DataDir::new(self.directory.path().to_path_buf()).read_principal_token();
        DaemonClient::connect(&self.socket, token)
            .unwrap()
            .expect("a daemon")
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

/// Puts `commit` of `repository` into the bare repository at `bare` with no ref reaching it, as
/// a head rewritten away leaves it.
fn give_unreachable(bare: &Path, repository: &TestRepository, branch: &str) {
    let source = repository.directory().display().to_string();
    git_in(
        bare,
        &[
            "fetch",
            "--quiet",
            &source,
            &format!("+{branch}:refs/heads/scratch"),
        ],
    );
    git_in(bare, &["update-ref", "-d", "refs/heads/scratch"]);
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

#[tokio::test]
async fn a_head_named_on_one_device_is_fetched_by_the_other_over_wf_git() {
    if wf_core::testing::skip_in_fence("iroh needs the route socket") {
        return;
    }
    let fixture = Fixture::new(2);
    let workforce = fixture.workforce();
    let laptop = Device::start(&fixture, 0).await;
    let vm = Device::start(&fixture, 1).await;
    let (laptop_node, vm_node) = (
        laptop.daemon.node().await.unwrap(),
        vm.daemon.node().await.unwrap(),
    );
    laptop_node.remember(vm_node.local_addr());
    vm_node.remember(laptop_node.local_addr());

    // The vm's session ends with a head that only the vm's repository has, and no ref reaches.
    let repository = TestRepository::new();
    let head = repository.work("wf/work", "a.txt", "a\n");
    eventually("the vm's bare repository is made", || {
        vm.bare(workforce).join("HEAD").is_file()
    })
    .await;
    give_unreachable(&vm.bare(workforce), &repository, "wf/work");
    let mut client = vm.client();
    let item = blocking(move || {
        client.author(workforce, define_agent("coder")).unwrap();
        let item: wf_core::id::ItemId = client
            .author(workforce, create("task", "work on the vm"))
            .unwrap()
            .id()
            .into();
        let session: SessionId = client
            .author(workforce, start_session("coder", item))
            .unwrap()
            .id()
            .into();
        client
            .author(
                workforce,
                end_session_at(session, EndReason::Released, head),
            )
            .unwrap();
        item
    })
    .await;

    let laptop_bare = laptop.bare(workforce);
    eventually(
        "the head arrives on the laptop as the branch of the item",
        || {
            GitCli::open(&laptop_bare)
                .and_then(|git| git.branch_head(&wf_vcs::item_branch(item)))
                .is_ok_and(|branch| branch == head)
        },
    )
    .await;
    let mut client = laptop.client();
    let awaited = blocking(move || client.awaited_commits(workforce).unwrap()).await;
    assert!(awaited.is_empty(), "{awaited:?}");

    // What the vm has is served by id, whether a ref reaches it or not; what it lacks is
    // refused with a clear error.
    let worktree = repository.worktree_path("wf/work");
    repository.write(&worktree, "b.txt", "b\n");
    let rewritten = repository.commit(&worktree, "rewritten away");
    give_unreachable(&vm.bare(workforce), &repository, "wf/work");
    laptop
        .daemon
        .fetch_commit_from(workforce, vm.daemon.device(), rewritten)
        .await
        .unwrap();
    assert!(
        GitCli::open(&laptop_bare)
            .unwrap()
            .has_commit(rewritten)
            .unwrap()
    );
    let unknown = CommitId::Git(wf_core::session::GitOid::from_bytes([9; 20]));
    let refused = laptop
        .daemon
        .fetch_commit_from(workforce, vm.daemon.device(), unknown)
        .await
        .expect_err("the vm does not have it");
    let message = wf_core::error_chain(&refused);
    assert!(
        message.contains(&format!("does not have commit {}", unknown.short())),
        "{message}"
    );
}

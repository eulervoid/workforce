//! `wf git-bridge`, the program `git fetch` runs to reach the daemon: it pipes its input and output
//! to the socket it inherits as descriptor 3, and passes on the end of either.

use std::io::Write;
use std::net::Shutdown;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::process::{Child, Stdio};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

mod common;

use common::Sandbox;

#[test]
fn the_bridge_pipes_both_ways_and_passes_on_the_ends() {
    let sandbox = Sandbox::new();
    let (daemon_end, bridge_end) = UnixStream::pair().unwrap();
    let mut bridge = spawn_bridge(&sandbox, bridge_end, Stdio::piped());
    let daemon = std::thread::spawn(move || {
        // Reads until the bridge says git is done sending, then answers and hangs up. Through
        // tokio, as the daemon reads: macOS can leave a blocking `read` asleep when the bridge
        // shuts down its write half, so a plain `read_to_string` here would wait for the kill.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .unwrap();
        runtime.block_on(async {
            daemon_end.set_nonblocking(true).unwrap();
            let mut daemon_end = tokio::net::UnixStream::from_std(daemon_end).unwrap();
            let mut received = String::new();
            daemon_end.read_to_string(&mut received).await.unwrap();
            daemon_end
                .write_all(received.to_uppercase().as_bytes())
                .await
                .unwrap();
            received
        })
    });
    let mut input = bridge.stdin.take().unwrap();
    input.write_all(b"want a commit").unwrap();
    drop(input);
    // A bridge that does not pass on the ends is killed, which ends the daemon's side too.
    let output = common::output_of(bridge, "wf git-bridge", Duration::from_secs(10));
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "WANT A COMMIT");
    assert_eq!(daemon.join().unwrap(), "want a commit");
}

#[test]
fn a_bridge_exits_when_the_daemon_shuts_down_its_side_while_git_still_has_its_input_open() {
    let sandbox = Sandbox::new();
    let (mut daemon_end, bridge_end) = UnixStream::pair().unwrap();
    let mut bridge = spawn_bridge(&sandbox, bridge_end, Stdio::piped());
    // Git's side stays open, as when git waits for more; the other device has said all it will,
    // and the daemon passes that on with a shutdown, keeping its end of the socket open.
    let _input = bridge.stdin.take().unwrap();
    daemon_end.write_all(b"no such workforce").unwrap();
    daemon_end.shutdown(Shutdown::Write).unwrap();
    let output = common::output_of(bridge, "wf git-bridge", Duration::from_secs(10));
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "no such workforce"
    );
    drop(daemon_end);
}

#[test]
fn a_bridge_started_without_the_daemons_socket_refuses() {
    let sandbox = Sandbox::new();
    let output = common::output(
        sandbox
            .command(&["git-bridge"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not the daemon's bridge"), "{stderr}");
}

#[test]
fn git_fetches_a_commit_through_the_bridge_on_the_socket_it_inherits() {
    use wf_vcs::testing::{TestRepository, git_in};
    use wf_vcs::{Bridge, GitCli};

    let sandbox = Sandbox::new();
    let repository = TestRepository::new();
    let head = repository.work("wf/work", "a.txt", "a\n");
    let served = sandbox.root.path().join("served.git");
    let source = repository.directory().display().to_string();
    git_in(
        sandbox.root.path(),
        &[
            "clone",
            "--quiet",
            "--bare",
            &source,
            served.to_str().unwrap(),
        ],
    );
    git_in(&served, &["fetch", "--quiet", &source, "wf/work:wf/work"]);
    let server = GitCli::open(&served).unwrap();
    let into_path = sandbox.root.path().join("into.git");
    std::fs::create_dir(&into_path).unwrap();
    git_in(&into_path, &["init", "--quiet", "--bare"]);
    let into = GitCli::open(&into_path).unwrap();

    // This side of the pair stands in for the daemon: it pipes to `upload-pack` directly, where
    // the daemon goes over iroh.
    let (daemon_end, bridge_end) = UnixStream::pair().unwrap();
    let mut upload_pack = server.spawn_upload_pack().unwrap();
    let (mut input, mut output) = (
        upload_pack.stdin.take().unwrap(),
        upload_pack.stdout.take().unwrap(),
    );
    let mut reader = daemon_end.try_clone().unwrap();
    let mut writer = daemon_end;
    let requests = std::thread::spawn(move || {
        let _ = std::io::copy(&mut reader, &mut input);
    });
    let answers = std::thread::spawn(move || {
        let _ = std::io::copy(&mut output, &mut writer);
        let _ = writer.shutdown(std::net::Shutdown::Write);
        upload_pack.wait().unwrap()
    });
    let wf = sandbox
        .command(&[])
        .get_program()
        .to_string_lossy()
        .into_owned();
    let bridge = Bridge {
        command: vec![wf, "git-bridge".to_string()],
        descriptor: Some(OwnedFd::from(bridge_end)),
    };
    into.fetch_commit(bridge, head).unwrap();
    assert!(into.has_commit(head).unwrap());
    requests.join().unwrap();
    assert!(answers.join().unwrap().success());
}

#[test]
fn a_bridge_whose_other_side_ends_at_once_lets_git_fail_instead_of_waiting() {
    use wf_vcs::testing::git_in;
    use wf_vcs::{Bridge, GitCli};

    let sandbox = Sandbox::new();
    let into_path = sandbox.root.path().join("into.git");
    std::fs::create_dir(&into_path).unwrap();
    git_in(&into_path, &["init", "--quiet", "--bare"]);
    let into = GitCli::open(&into_path).unwrap();
    // The daemon's side hangs up before saying anything, as a device that refuses does.
    let (daemon_end, bridge_end) = UnixStream::pair().unwrap();
    drop(daemon_end);
    let wf = sandbox
        .command(&[])
        .get_program()
        .to_string_lossy()
        .into_owned();
    let bridge = Bridge {
        command: vec![wf, "git-bridge".to_string()],
        descriptor: Some(OwnedFd::from(bridge_end)),
    };
    let unknown = wf_core::session::CommitId::Git(wf_core::session::GitOid::from_bytes([9; 20]));
    assert!(into.fetch_commit(bridge, unknown).is_err());
}

#[test]
fn a_bridge_exits_when_the_daemon_hangs_up_while_git_still_has_its_input_open() {
    let sandbox = Sandbox::new();
    let (daemon_end, bridge_end) = UnixStream::pair().unwrap();
    let mut bridge = spawn_bridge(&sandbox, bridge_end, Stdio::null());
    // Git's side stays open, as when git waits for an answer; the daemon, or its process, goes.
    let _input = bridge.stdin.take().unwrap();
    drop(daemon_end);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let status = loop {
        if let Some(status) = bridge.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() > deadline {
            let _ = bridge.kill();
            panic!("the bridge outlived the daemon's end of its socket");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    assert!(status.success());
}

/// Starts `wf git-bridge` as git does, with `bridge_end` as its descriptor 3 and its input piped.
fn spawn_bridge(sandbox: &Sandbox, bridge_end: UnixStream, output: Stdio) -> Child {
    let bridge_end = OwnedFd::from(bridge_end);
    let inherited = bridge_end.as_raw_fd();
    let mut command = sandbox.command(&["git-bridge"]);
    wf_vcs::inherit_as_bridge(&mut command, inherited);
    command
        .stdin(Stdio::piped())
        .stdout(output)
        .spawn()
        .unwrap()
}

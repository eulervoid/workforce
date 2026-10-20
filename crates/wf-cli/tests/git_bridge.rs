//! `wf git-bridge`, the program `git fetch` runs to reach the daemon: it pipes its input and output
//! to the socket it inherits as descriptor 3, and passes on the end of either.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::Stdio;

mod common;

use common::Sandbox;

/// The descriptor the daemon hands git, and git the bridge.
const BRIDGE_DESCRIPTOR: i32 = 3;

#[test]
fn the_bridge_pipes_both_ways_and_passes_on_the_ends() {
    let sandbox = Sandbox::new();
    let (mut daemon_end, bridge_end) = UnixStream::pair().unwrap();
    let bridge_end = OwnedFd::from(bridge_end);
    let inherited = bridge_end.as_raw_fd();
    let mut command = sandbox.command(&["git-bridge"]);
    // SAFETY: dup2 is async-signal-safe; the copy it makes is not closed on exec.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(inherited, BRIDGE_DESCRIPTOR) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut bridge = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    drop(bridge_end);
    let daemon = std::thread::spawn(move || {
        // Reads until the bridge says git is done sending, then answers and hangs up.
        let mut received = String::new();
        daemon_end.read_to_string(&mut received).unwrap();
        daemon_end
            .write_all(received.to_uppercase().as_bytes())
            .unwrap();
        received
    });
    let mut input = bridge.stdin.take().unwrap();
    input.write_all(b"want a commit").unwrap();
    drop(input);
    let mut answer = String::new();
    bridge
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut answer)
        .unwrap();
    assert!(bridge.wait().unwrap().success());
    assert_eq!(answer, "WANT A COMMIT");
    assert_eq!(daemon.join().unwrap(), "want a commit");
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

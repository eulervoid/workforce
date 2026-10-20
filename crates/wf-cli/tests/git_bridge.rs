//! `wf git-bridge`, the program `git fetch` runs to reach the daemon: it pipes its input and output
//! to a unix socket, and passes on the end of either.

use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::process::{Command, Stdio};

#[test]
fn the_bridge_pipes_both_ways_and_passes_on_the_ends() {
    let directory = tempfile::TempDir::new().unwrap();
    let socket = directory.path().join("bridge");
    let listener = UnixListener::bind(&socket).unwrap();
    let daemon = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        // Reads until the bridge says git is done sending, then answers and hangs up.
        let mut received = String::new();
        stream.read_to_string(&mut received).unwrap();
        stream
            .write_all(received.to_uppercase().as_bytes())
            .unwrap();
        received
    });
    let mut bridge = Command::new(env!("CARGO_BIN_EXE_wf"))
        .arg("git-bridge")
        .arg(&socket)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
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

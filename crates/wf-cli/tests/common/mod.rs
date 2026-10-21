//! What the tests that run the `wf` binary share.

#![allow(dead_code)]

use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::Value;
use tempfile::TempDir;

pub struct Sandbox {
    pub root: TempDir,
    pub repository: PathBuf,
    pub data: PathBuf,
    pub bin: PathBuf,
}

/// A session's own identity, which `cargo test` inherits inside a session, must not reach the
/// sandboxed workforce: `Workspace::open` in the test process reads it as well as `wf` does.
fn clear_session_identity() {
    static CLEARED: std::sync::Once = std::sync::Once::new();
    CLEARED.call_once(|| {
        for variable in [
            "WF_SESSION",
            "WF_SESSION_TOKEN",
            "WF_WORKFORCE",
            "GIT_CONFIG_COUNT",
        ] {
            // SAFETY: runs once, before the test that called it spawns or reads anything.
            unsafe { std::env::remove_var(variable) };
        }
    });
}

impl Sandbox {
    pub fn new() -> Self {
        clear_session_identity();
        let root = TempDir::new().unwrap();
        let repository = root.path().join("repo");
        let data = root.path().join("data");
        let bin = root.path().join("bin");
        std::fs::create_dir_all(&repository).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        for harness in ["claude-agent-acp", "opencode"] {
            let path = bin.join(harness);
            std::fs::write(&path, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // Commands that show or check out work run git.
        let git = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|directory| directory.join("git"))
            .find(|path| path.is_file())
            .expect("git is installed");
        std::os::unix::fs::symlink(git, bin.join("git")).unwrap();
        Self {
            root,
            repository,
            data,
            bin,
        }
    }

    pub fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_wf"));
        command
            .args(args)
            .current_dir(&self.repository)
            .env("XDG_DATA_HOME", &self.data)
            .env("XDG_RUNTIME_DIR", self.root.path().join("run"))
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("PATH", &self.bin)
            .env("USER", "ada")
            // A session's own identity must not leak into the sandboxed workforce.
            .env_remove("WF_SESSION")
            .env_remove("WF_SESSION_TOKEN")
            .env_remove("WF_WORKFORCE")
            // A session's `commit.gpgsign=false` would win over a test repository's signing.
            .env_remove("GIT_CONFIG_COUNT");
        inherit_only_stdio(&mut command);
        command
    }

    pub fn run(&self, args: &[&str]) -> String {
        let output = output(&mut self.command(args));
        assert!(
            output.status.success(),
            "wf {args:?} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    pub fn fail(&self, args: &[&str]) -> String {
        let output = output(&mut self.command(args));
        assert!(!output.status.success(), "wf {args:?} should fail");
        String::from_utf8(output.stderr).unwrap()
    }

    pub fn json(&self, args: &[&str]) -> Value {
        let mut all = args.to_vec();
        all.push("--json");
        let value: Value = serde_json::from_str(&self.run(&all)).unwrap();
        assert_eq!(value["schema"], wf_cli::view::SCHEMA_VERSION);
        value["data"].clone()
    }

    pub fn add(&self, args: &[&str]) -> String {
        let mut all = vec!["item", "add"];
        all.extend_from_slice(args);
        self.json(&all)["id"].as_str().unwrap().to_string()
    }
}

/// Has `command` inherit no descriptor beyond stdin, stdout and stderr. The tests spawn from many
/// threads at once, and on macOS std sets close-on-exec on a new pipe in a second step: a `wf`
/// spawned in between inherits the write end of another command's output, and one that lives
/// long, as a daemon in the background does, keeps that command's output from ending.
pub fn inherit_only_stdio(command: &mut Command) {
    // SAFETY: setting close-on-exec only calls functions that are async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            close_fds::set_fds_cloexec(3, &[]);
            Ok(())
        });
    }
}

/// How long a command a test runs may take before the test fails instead of waiting on.
const COMMAND_LIMIT: Duration = Duration::from_secs(120);

/// Runs `command` to the end and reads its output, as `Command::output` does, but fails the test
/// with what it ran once that takes longer than `COMMAND_LIMIT`. Output does not end while another
/// process holds a descriptor of it, even after the command exited.
pub fn output(command: &mut Command) -> Output {
    output_within(command, COMMAND_LIMIT)
}

/// [`output`], with the time it may take.
pub fn output_within(command: &mut Command, limit: Duration) -> Output {
    let ran = format!("{command:?}");
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("{ran} does not start: {error}"));
    let process = child.id();
    let (finished, outcome) = mpsc::channel();
    std::thread::spawn(move || finished.send(child.wait_with_output()));
    match outcome.recv_timeout(limit) {
        Ok(output) => output.unwrap_or_else(|error| panic!("{ran} failed: {error}")),
        Err(_) => {
            let _ = Command::new("/bin/kill")
                .args(["-KILL", &process.to_string()])
                .status();
            panic!("{ran} did not finish within {}s", limit.as_secs());
        }
    }
}

pub fn short(id: &str) -> &str {
    &id[..6]
}

/// The comments in the activity of an item `wf item show --json` printed, oldest first: comments
/// of their own and the reasons given with moves and verdicts, each with the text of its entry.
pub fn comments(shown: &Value) -> Value {
    Value::Array(
        shown["activity"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["comment"].is_object())
            .map(|entry| {
                let mut comment = entry["comment"].clone();
                comment["text"] = entry["text"].clone();
                comment
            })
            .collect(),
    )
}

/// What a daemon of a test logged, across restarts, so that a wait that times out tells what the
/// daemon was doing.
#[derive(Clone, Default)]
pub struct DaemonLog(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

/// What [`DaemonLog`] keeps: what `wf daemon start` logs, what the dispatcher does, and how each
/// session's harness gets on.
const LOG_FILTER: &str = "warn,wf_daemon=info,wf_daemon::runner::dispatch=debug,wf_daemon::runner::drive=debug,wf_net=info";
/// How many lines of the daemon log a wait that times out prints.
pub const LOG_TAIL: usize = 60;

impl DaemonLog {
    /// Where a daemon of the test logs: each daemon of the tests runs with a log of its own,
    /// which its tasks inherit.
    pub fn dispatch(&self) -> tracing::Dispatch {
        let log = self.clone();
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new(LOG_FILTER))
            .with_ansi(false)
            .with_writer(move || log.clone())
            .finish()
            .into()
    }

    /// The last `lines` lines.
    pub fn tail(&self, lines: usize) -> String {
        let text = String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned();
        let all: Vec<&str> = text.lines().collect();
        all[all.len().saturating_sub(lines)..].join("\n")
    }
}

impl std::io::Write for DaemonLog {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

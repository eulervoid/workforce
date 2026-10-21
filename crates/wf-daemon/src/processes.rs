//! The processes a session or a command left running, found by the variables they inherited from
//! it: also those that left its process group or session and were reparented to `launchd` or
//! init, as a daemonized server or a job in the background of a harness does, which stopping the
//! group misses (SPEC §9).
//!
//! The environment is read as the process was started with it: on Linux from
//! `/proc/<pid>/environ`, on macOS through `sysctl` `KERN_PROCARGS2`, only for the processes of
//! this user. macOS hides the environment of Apple's own programs (`sh`, `sleep`, `yes`), and a
//! process that cleared its environment carries no mark (SPEC §13).

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::time::{Duration, Instant};

use nix::sys::signal::Signal;
use nix::unistd::Pid;
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use wf_core::id::{HashId, SessionId, WorkforceId};

use crate::caller::{SESSION_VARIABLE, WORKFORCE_VARIABLE};

/// Set for a command the daemon or `wf check` runs, to a value of its own for each run.
pub(crate) const COMMAND_RUN_VARIABLE: &str = "WF_COMMAND_RUN";

/// How long a process has to end after `TERM` before it is killed.
const GRACE: Duration = Duration::from_secs(2);
/// How often the processes are looked at again while they end.
const POLL: Duration = Duration::from_millis(100);

/// What the processes started for one session or one run of a command carry in their environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mark {
    /// `WF_SESSION` and `WF_WORKFORCE`, which the daemon sets for the harness.
    Session {
        workforce: WorkforceId,
        session: SessionId,
    },
    /// [`COMMAND_RUN_VARIABLE`].
    CommandRun(u64),
}

impl Mark {
    /// A mark for a run of a command that starts now.
    pub(crate) fn new_command_run() -> Self {
        Mark::CommandRun(rand::random())
    }

    /// The variables the processes it marks carry, with their values.
    pub(crate) fn variables(&self) -> Vec<(&'static str, String)> {
        match self {
            Mark::Session { workforce, session } => vec![
                (SESSION_VARIABLE, session.to_string()),
                (WORKFORCE_VARIABLE, workforce.to_string()),
            ],
            Mark::CommandRun(run) => vec![(COMMAND_RUN_VARIABLE, format!("{run:016x}"))],
        }
    }
}

/// A process of this user, other than the daemon, with the environment it was started with.
struct Process {
    pid: Pid,
    environment: Vec<OsString>,
}

impl Process {
    /// The value of the variable `name` in its environment.
    fn variable(&self, name: &str) -> Option<&str> {
        self.environment
            .iter()
            .find_map(|entry| entry.to_str()?.strip_prefix(name)?.strip_prefix('='))
    }

    fn carries(&self, mark: &Mark) -> bool {
        mark.variables()
            .iter()
            .all(|(name, value)| self.variable(name) == Some(value))
    }
}

/// The processes of this user other than the daemon, those whose environment cannot be read left
/// out. Takes some milliseconds, as it reads every process.
fn list() -> Vec<Process> {
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_environ(UpdateKind::Always)
            .with_user(UpdateKind::Always)
            .without_tasks(),
    );
    // SAFETY: `getuid` cannot fail and touches no memory.
    let user = unsafe { libc::getuid() };
    let daemon = std::process::id();
    system
        .processes()
        .iter()
        .filter(|(pid, process)| {
            pid.as_u32() != daemon
                && process.thread_kind().is_none()
                && process.user_id().is_some_and(|id| **id == user)
                && !process.environ().is_empty()
        })
        .filter_map(|(pid, process)| {
            Some(Process {
                pid: Pid::from_raw(i32::try_from(pid.as_u32()).ok()?),
                environment: process.environ().to_vec(),
            })
        })
        .collect()
}

/// The running processes that carry `mark`.
async fn marked(mark: Mark) -> BTreeSet<Pid> {
    tokio::task::spawn_blocking(move || {
        list()
            .into_iter()
            .filter(|process| process.carries(&mark))
            .map(|process| process.pid)
            .collect()
    })
    .await
    .unwrap_or_default()
}

/// Stops the processes that carry `mark`: `TERM` first, and `KILL` for those still running after
/// [`GRACE`]. Those they start meanwhile are stopped too. Returns how many it stopped.
pub(crate) async fn stop(mark: Mark) -> usize {
    let mut stopped = BTreeSet::new();
    let killed_after = Instant::now() + GRACE;
    loop {
        let running = marked(mark).await;
        if running.is_empty() {
            return stopped.len();
        }
        let signal = if Instant::now() < killed_after {
            Signal::SIGTERM
        } else {
            Signal::SIGKILL
        };
        for &pid in &running {
            // It may have ended since it was found; then there is nothing to stop.
            let _ = nix::sys::signal::kill(pid, signal);
        }
        stopped.extend(running);
        tokio::time::sleep(POLL).await;
    }
}

/// Stops what `session` of `workforce` left running, as [`stop`] does, and logs how many there
/// were. The daemon itself may have been started in a session; that session's processes are left
/// alone, as what the daemon starts with its own environment, such as git, would be among them.
pub(crate) async fn stop_session(workforce: WorkforceId, session: SessionId) -> usize {
    if std::env::var(SESSION_VARIABLE).is_ok_and(|own| own == session.to_string()) {
        return 0;
    }
    let stopped = stop(Mark::Session { workforce, session }).await;
    if stopped > 0 {
        tracing::info!(
            "session {}: stopped {stopped} process{} it left running",
            session.short(),
            if stopped == 1 { "" } else { "es" }
        );
    }
    stopped
}

/// The sessions of `workforce` that running processes carry the mark of.
pub(crate) async fn marked_sessions(workforce: WorkforceId) -> BTreeSet<SessionId> {
    let workforce = workforce.to_string();
    tokio::task::spawn_blocking(move || {
        list()
            .iter()
            .filter(|process| process.variable(WORKFORCE_VARIABLE) == Some(&workforce))
            .filter_map(|process| process.variable(SESSION_VARIABLE)?.parse().ok())
            .collect()
    })
    .await
    .unwrap_or_default()
}

#[cfg(test)]
pub(crate) mod testing {
    //! A process that daemonizes, for the tests. macOS hides the environment of its own programs,
    //! so the process that stays is this test binary, not `sleep`.

    use std::os::unix::process::CommandExt;
    use std::path::Path;
    use std::process::Command;
    use std::time::{Duration, Instant};

    use nix::unistd::Pid;

    /// Set for this test binary to sleep in [`sleeper`] instead of testing, to the file it writes
    /// its pid to once it runs.
    pub(crate) const SLEEP: &str = "WF_TEST_SLEEP";
    /// Set as well for it to ignore `TERM`.
    pub(crate) const IGNORE_TERM: &str = "WF_TEST_IGNORE_TERM";

    /// Starts this test binary with `variables`, as a daemon starts: by a shell in a session of
    /// its own, which exits at once, so that it is reparented to `launchd` or init. It sleeps
    /// until it is stopped. Returns its pid once it runs.
    pub(crate) fn daemonized(variables: &[(&str, String)], directory: &Path) -> Pid {
        let pid_file = directory.join(format!("{}.pid", rand::random::<u64>()));
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                "\"$0\" --exact processes::testing::sleeper --ignored </dev/null >/dev/null 2>&1 &",
            ])
            .arg(std::env::current_exe().unwrap())
            .env(SLEEP, &pid_file)
            .envs(variables.iter().map(|(name, value)| (name, value)));
        // SAFETY: `setsid` is async-signal-safe and touches no memory of this process.
        unsafe {
            command.pre_exec(|| {
                nix::unistd::setsid()?;
                Ok(())
            });
        }
        assert!(command.status().unwrap().success());
        sleeper_pid(&pid_file)
    }

    /// The pid the sleeper wrote to `pid_file`, once it did.
    pub(crate) fn sleeper_pid(pid_file: &Path) -> Pid {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(pid) = std::fs::read_to_string(pid_file)
                && let Ok(pid) = pid.trim().parse()
            {
                return Pid::from_raw(pid);
            }
            assert!(Instant::now() < deadline, "the sleeper did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Whether `pid` runs, given a moment to be reaped by `launchd` or init once it ended.
    pub(crate) fn running(pid: Pid) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        while nix::sys::signal::kill(pid, None).is_ok() {
            if Instant::now() > deadline {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// What a daemonized process runs: it sleeps, and ignores `TERM` when asked to.
    #[test]
    #[ignore = "started by `daemonized`"]
    fn sleeper() {
        let Some(pid_file) = std::env::var_os(SLEEP) else {
            return;
        };
        if std::env::var_os(IGNORE_TERM).is_some() {
            // SAFETY: ignoring a signal touches no memory.
            unsafe {
                libc::signal(libc::SIGTERM, libc::SIG_IGN);
            }
        }
        // Written through a rename, so that whoever waits for it reads all of it.
        let written = Path::new(&pid_file).with_extension("written");
        std::fs::write(&written, std::process::id().to_string()).unwrap();
        std::fs::rename(&written, &pid_file).unwrap();
        std::thread::sleep(Duration::from_secs(600));
    }
}

#[cfg(test)]
mod tests {
    use std::process::{Command, Stdio};

    use super::testing::{IGNORE_TERM, daemonized, running};
    use super::*;

    #[tokio::test]
    async fn a_daemonized_process_is_found_by_its_mark_and_stopped_and_no_other() {
        let directory = tempfile::tempdir().unwrap();
        let (mark, other) = (Mark::new_command_run(), Mark::new_command_run());
        let escaped = daemonized(&mark.variables(), directory.path());
        let others = daemonized(&other.variables(), directory.path());
        let mut unmarked = Command::new("sleep")
            .arg("600")
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        assert_eq!(marked(mark).await, BTreeSet::from([escaped]));

        assert_eq!(stop(mark).await, 1);
        assert!(!running(escaped));
        assert!(running(others), "another mark's process runs on");
        assert!(
            unmarked.try_wait().unwrap().is_none(),
            "an unmarked process runs on"
        );

        assert_eq!(stop(other).await, 1);
        assert!(!running(others));
        unmarked.kill().unwrap();
        unmarked.wait().unwrap();
    }

    #[tokio::test]
    async fn a_process_that_ignores_term_is_killed() {
        let directory = tempfile::tempdir().unwrap();
        let mark = Mark::new_command_run();
        let mut variables = mark.variables();
        variables.push((IGNORE_TERM, "1".to_string()));
        let stubborn = daemonized(&variables, directory.path());
        let started = Instant::now();
        assert_eq!(stop(mark).await, 1);
        assert!(started.elapsed() >= GRACE);
        assert!(!running(stubborn));
    }

    #[tokio::test]
    async fn a_session_is_marked_by_its_workforce_and_its_id() {
        let directory = tempfile::tempdir().unwrap();
        let workforce = WorkforceId::from_bytes(rand::random());
        let session = SessionId::from_bytes(rand::random());
        let mark = Mark::Session { workforce, session };
        let pid = daemonized(&mark.variables(), directory.path());
        assert_eq!(marked_sessions(workforce).await, BTreeSet::from([session]));
        let elsewhere = WorkforceId::from_bytes(rand::random());
        assert_eq!(
            stop_session(elsewhere, session).await,
            0,
            "only in its workforce"
        );
        assert!(running(pid));

        assert_eq!(stop_session(workforce, session).await, 1);
        assert!(!running(pid));
        assert!(marked_sessions(workforce).await.is_empty());
    }
}

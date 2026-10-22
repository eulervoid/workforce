//! The processes a session or a command left running: also those that left its process group or
//! session and were reparented to `launchd` or init, as a daemonized server or a job in the
//! background of a harness does, which stopping the group misses (SPEC §9).
//!
//! They are found by the variables they inherited, in the environment they were started with: on
//! Linux from `/proc/<pid>/environ`, on macOS through `sysctl` `KERN_PROCARGS2`, only for the
//! processes of this user. macOS hides the environment of Apple's own programs (`sh`, `sleep`,
//! `yes`); an orphan of those is found by where it runs and when it started instead. A process
//! that cleared its environment carries no mark (SPEC §13).

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use jiff::Timestamp;
use nix::sys::signal::Signal;
use nix::unistd::Pid;
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use wf_core::id::{HashId, SessionId, WorkforceId};

use crate::caller::{SESSION_VARIABLE, WORKFORCE_VARIABLE};

/// Set for a command the daemon or `wf check` runs, to a value of its own for each run.
pub(crate) const COMMAND_RUN_VARIABLE: &str = "WF_COMMAND_RUN";

/// How long a process has to end after `TERM` before it is killed.
const GRACE: Duration = Duration::from_secs(2);
/// How long a process has to end after `KILL` before it is left running: one in uninterruptible
/// sleep, as on a mount that hangs, does not end, and the end of a session must not wait for it.
const KILLED: Duration = Duration::from_secs(5);
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

/// A session or a run of a command, which the processes it started belong to.
#[derive(Debug, Clone)]
pub(crate) struct Owner {
    pub(crate) mark: Mark,
    /// Where it runs, which only it does; `None` for a command that runs where a session does,
    /// whose orphans the session's end finds.
    pub(crate) place: Option<Place>,
    /// The places of the sessions that run, whose orphans are never this owner's, even when their
    /// times overlap: the times of the log can run ahead of this machine's clock.
    pub(crate) spared: Vec<Place>,
}

/// The directory a session or a command runs in, and when it ran: an orphan whose environment is
/// hidden belongs to it if it runs inside the directory and started while it ran. The work sessions
/// of an item run in the same repository, one after the other.
#[derive(Debug, Clone)]
pub(crate) struct Place {
    /// Canonical, as macOS gives the working directory of a process.
    directory: PathBuf,
    since: Timestamp,
    /// When it ended; `None` while it runs.
    until: Option<Timestamp>,
}

impl Place {
    /// `directory`, which may be gone already, made canonical as far as it is there.
    pub(crate) fn new(directory: &Path, since: Timestamp, until: Option<Timestamp>) -> Self {
        let mut missing = Vec::new();
        let mut existing = directory;
        let canonical = loop {
            if let Ok(canonical) = existing.canonicalize() {
                break canonical;
            }
            match (existing.parent(), existing.file_name()) {
                (Some(parent), Some(name)) => {
                    missing.push(name.to_os_string());
                    existing = parent;
                }
                _ => break directory.to_path_buf(),
            }
        };
        Self {
            directory: missing
                .iter()
                .rev()
                .fold(canonical, |path, name| path.join(name)),
            since,
            until,
        }
    }
}

/// A process of this user, other than the daemon.
struct Process {
    pid: Pid,
    seen: Seen,
}

/// What tells whom a process belongs to.
enum Seen {
    /// The environment it was started with.
    Environment(Vec<OsString>),
    /// macOS hides the environment of Apple's own programs: for an orphan of those, reparented to
    /// `launchd`, when it started and its working directory, if macOS gives one.
    HiddenOrphan {
        started: Timestamp,
        directory: Option<PathBuf>,
    },
}

/// Whether a process belongs to an owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Belongs {
    Yes,
    No,
    /// An orphan whose environment is hidden, started since the owner did, whose working
    /// directory macOS does not give: it may be the owner's, and is left running.
    Unknown,
}

impl Process {
    /// The value of the variable `name` in its environment.
    fn variable(&self, name: &str) -> Option<&str> {
        match &self.seen {
            Seen::Environment(environment) => environment
                .iter()
                .find_map(|entry| entry.to_str()?.strip_prefix(name)?.strip_prefix('=')),
            Seen::HiddenOrphan { .. } => None,
        }
    }

    /// Whether it belongs to `owner`. A process whose environment can be read is judged by its
    /// mark alone, wherever it runs.
    fn belongs(&self, owner: &Owner) -> Belongs {
        match &self.seen {
            Seen::Environment(_) => {
                let carries = owner
                    .mark
                    .variables()
                    .iter()
                    .all(|(name, value)| self.variable(name) == Some(value));
                if carries { Belongs::Yes } else { Belongs::No }
            }
            Seen::HiddenOrphan { started, directory } => {
                let Some(place) = &owner.place else {
                    return Belongs::No;
                };
                let spared = owner
                    .spared
                    .iter()
                    .any(|spared| spared.holds(*started, directory.as_deref()) != Belongs::No);
                if spared {
                    return Belongs::No;
                }
                place.holds(*started, directory.as_deref())
            }
        }
    }
}

impl Place {
    /// Whether an orphan whose environment is hidden, started at `started` in `directory`, is
    /// what ran here.
    fn holds(&self, started: Timestamp, directory: Option<&Path>) -> Belongs {
        if started < self.since || self.until.is_some_and(|until| started > until) {
            return Belongs::No;
        }
        match directory {
            // Component by component, so that `/x/sess-ab` holds nothing of `/x/sess-abc`.
            Some(directory) if directory.starts_with(&self.directory) => Belongs::Yes,
            Some(_) => Belongs::No,
            None => Belongs::Unknown,
        }
    }
}

/// The processes of this user other than the daemon: those whose environment can be read, and on
/// macOS the orphans whose environment is hidden. Takes some milliseconds, as it reads every
/// process.
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
        })
        .filter_map(|(pid, process)| {
            let pid = Pid::from_raw(i32::try_from(pid.as_u32()).ok()?);
            let seen = if process.environ().is_empty() {
                hidden_orphan(pid, user)?
            } else {
                Seen::Environment(process.environ().to_vec())
            };
            Some(Process { pid, seen })
        })
        .collect()
}

/// `pid`, whose environment is hidden, if it is an orphan of `user`.
#[cfg(target_os = "macos")]
fn hidden_orphan(pid: Pid, user: libc::uid_t) -> Option<Seen> {
    use std::os::unix::ffi::OsStrExt;

    // SAFETY: both structs are plain data that `proc_pidinfo` fills up to the size it is given.
    let (info, paths) = unsafe {
        let mut info: libc::proc_bsdinfo = std::mem::zeroed();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let read = libc::proc_pidinfo(
            pid.as_raw(),
            libc::PROC_PIDTBSDINFO,
            0,
            (&raw mut info).cast(),
            size,
        );
        if read != size {
            return None;
        }
        let mut paths: libc::proc_vnodepathinfo = std::mem::zeroed();
        let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
        let read = libc::proc_pidinfo(
            pid.as_raw(),
            libc::PROC_PIDVNODEPATHINFO,
            0,
            (&raw mut paths).cast(),
            size,
        );
        (info, (read == size).then_some(paths))
    };
    if info.pbi_uid != user || info.pbi_ppid != 1 {
        return None;
    }
    let started = Timestamp::new(
        i64::try_from(info.pbi_start_tvsec).ok()?,
        i32::try_from(info.pbi_start_tvusec).ok()? * 1000,
    )
    .ok()?;
    // A directory that was deleted keeps its path; an empty one says nothing.
    let directory = paths.and_then(|paths| {
        let path = &paths.pvi_cdir.vip_path;
        // SAFETY: `vip_path` is `MAXPATHLEN` bytes, which libc declares as rows of `c_char`.
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(path.as_ptr().cast(), std::mem::size_of_val(path))
        };
        let bytes = &bytes[..bytes
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(bytes.len())];
        (!bytes.is_empty()).then(|| PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
    });
    Some(Seen::HiddenOrphan { started, directory })
}

/// Linux shows the environment of every process of the user: one with none carries no mark.
#[cfg(not(target_os = "macos"))]
fn hidden_orphan(_pid: Pid, _user: libc::uid_t) -> Option<Seen> {
    None
}

/// The running processes that belong to `owner`, and how many may.
async fn belonging(owner: Owner) -> (BTreeSet<Pid>, usize) {
    tokio::task::spawn_blocking(move || {
        let mut unknown = 0;
        let mut belonging = BTreeSet::new();
        for process in list() {
            match process.belongs(&owner) {
                Belongs::Yes => {
                    belonging.insert(process.pid);
                }
                Belongs::No => {}
                Belongs::Unknown => unknown += 1,
            }
        }
        (belonging, unknown)
    })
    .await
    .unwrap_or_default()
}

/// What stopping the processes of an owner came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stopped {
    pub(crate) stopped: usize,
    /// The orphans that may be the owner's but were left running, as their working directory is
    /// unknown.
    pub(crate) unknown: usize,
    /// Of those stopped, those still running after `KILL` was given time to end them.
    pub(crate) running: usize,
}

/// Stops the processes that belong to `owner`: `TERM` first, and `KILL` for those still running
/// after [`GRACE`]. Those they start meanwhile are stopped too. Gives up on those still running
/// [`KILLED`] after that.
pub(crate) async fn stop(owner: Owner) -> Stopped {
    stop_found(|| belonging(owner.clone()), GRACE, KILLED).await
}

/// [`stop`], for the processes `find` finds each time it looks.
async fn stop_found<Found>(
    mut find: impl FnMut() -> Found,
    grace: Duration,
    killed: Duration,
) -> Stopped
where
    Found: Future<Output = (BTreeSet<Pid>, usize)>,
{
    let mut stopped = BTreeSet::new();
    let mut unknown = None;
    let killed_after = Instant::now() + grace;
    let given_up_after = killed_after + killed;
    loop {
        let (running, may) = find().await;
        let unknown = *unknown.get_or_insert(may);
        if running.is_empty() || Instant::now() >= given_up_after {
            return Stopped {
                stopped: stopped.len(),
                unknown,
                running: running.len(),
            };
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

/// Stops what `session` of `workforce`, which ran in `place`, left running, as [`stop`] does, and
/// logs how many there were. The daemon itself may have been started in a session; that session's
/// processes are left alone, as what the daemon starts with its own environment, such as git,
/// would be among them.
pub(crate) async fn stop_session(
    workforce: WorkforceId,
    session: SessionId,
    place: Place,
    spared: Vec<Place>,
) -> Stopped {
    if std::env::var(SESSION_VARIABLE).is_ok_and(|own| own == session.to_string()) {
        return Stopped {
            stopped: 0,
            unknown: 0,
            running: 0,
        };
    }
    let mark = Mark::Session { workforce, session };
    let stopped = stop(Owner {
        mark,
        place: Some(place),
        spared,
    })
    .await;
    if stopped.stopped > 0 {
        tracing::info!(
            "session {}: stopped {} it left running",
            session.short(),
            processes(stopped.stopped)
        );
    }
    if stopped.running > 0 {
        tracing::warn!(
            "session {}: {} still running after KILL",
            session.short(),
            processes(stopped.running)
        );
    }
    if stopped.unknown > 0 {
        tracing::warn!(
            "session {}: left {} running that may be its own: orphans whose directory macOS does \
             not tell",
            session.short(),
            processes(stopped.unknown)
        );
    }
    stopped
}

/// "1 process", "2 processes".
pub(crate) fn processes(count: usize) -> String {
    format!("{count} process{}", if count == 1 { "" } else { "es" })
}

/// Of `owners`, those that a running process belongs to, read at once.
pub(crate) async fn with_processes(owners: Vec<Owner>) -> Vec<Owner> {
    tokio::task::spawn_blocking(move || {
        let running = list();
        owners
            .into_iter()
            .filter(|owner| {
                running
                    .iter()
                    .any(|process| process.belongs(owner) != Belongs::No)
            })
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

    /// An orphaned `/bin/sleep 120`, an Apple program whose environment macOS hides, started by a
    /// double fork in `directory`. Killed when dropped; one a failed test leaves ends by itself.
    pub(crate) struct OrphanedSleep(pub(crate) Pid);

    impl OrphanedSleep {
        pub(crate) fn start(directory: &Path) -> Self {
            let pid_file = directory.join(format!(".{}.pid", rand::random::<u64>()));
            let daemonize = "exit if fork; POSIX::setsid(); exit if fork; \
                select undef, undef, undef, 0.01 while getppid() != 1; \
                open F, '>', $ARGV[0] . '.written'; print F $$; close F; \
                rename $ARGV[0] . '.written', $ARGV[0]; \
                open STDIN, '<', '/dev/null'; open STDOUT, '>', '/dev/null'; \
                exec '/bin/sleep', '120'";
            let status = Command::new("/usr/bin/perl")
                .args(["-MPOSIX", "-e", daemonize])
                .arg(&pid_file)
                .current_dir(directory)
                .status()
                .unwrap();
            assert!(status.success());
            let orphan = Self(sleeper_pid(&pid_file));
            std::fs::remove_file(&pid_file).unwrap();
            // Until it runs `sleep`, it is perl.
            let deadline = Instant::now() + Duration::from_secs(20);
            while !runs_sleep(orphan.0) {
                assert!(Instant::now() < deadline, "the orphan did not run sleep");
                std::thread::sleep(Duration::from_millis(10));
            }
            orphan
        }

        /// Asserts that it ended, and forgets it, so that the drop does not kill a process that
        /// was given its pid since.
        pub(crate) fn assert_ended(self) {
            assert!(!running(self.0), "the orphan {} runs on", self.0);
            std::mem::forget(self);
        }
    }

    /// Whether `pid` runs `/bin/sleep`.
    fn runs_sleep(pid: Pid) -> bool {
        use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
        let pid = sysinfo::Pid::from_u32(pid.as_raw() as u32);
        let mut system = System::new();
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::nothing().with_exe(UpdateKind::Always),
        );
        system
            .process(pid)
            .and_then(|process| process.exe())
            .is_some_and(|executable| executable == Path::new("/bin/sleep"))
    }

    impl Drop for OrphanedSleep {
        fn drop(&mut self) {
            let _ = nix::sys::signal::kill(self.0, nix::sys::signal::Signal::SIGKILL);
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
        std::thread::sleep(Duration::from_secs(120));
    }
}

#[cfg(test)]
mod tests {
    use std::process::{Command, Stdio};

    use super::testing::{IGNORE_TERM, daemonized, running};
    use super::*;

    /// The owner a mark alone finds the processes of.
    fn marked(mark: Mark) -> Owner {
        Owner {
            mark,
            place: None,
            spared: Vec::new(),
        }
    }

    fn stopped(stopped: usize) -> Stopped {
        Stopped {
            stopped,
            unknown: 0,
            running: 0,
        }
    }

    #[tokio::test]
    async fn a_daemonized_process_is_found_by_its_mark_and_stopped_and_no_other() {
        let directory = tempfile::tempdir().unwrap();
        let (mark, other) = (Mark::new_command_run(), Mark::new_command_run());
        let escaped = daemonized(&mark.variables(), directory.path());
        let others = daemonized(&other.variables(), directory.path());
        let mut unmarked = Command::new("sleep")
            .arg("120")
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        assert_eq!(
            belonging(marked(mark)).await,
            (BTreeSet::from([escaped]), 0)
        );

        assert_eq!(stop(marked(mark)).await, stopped(1));
        assert!(!running(escaped));
        assert!(running(others), "another mark's process runs on");
        assert!(
            unmarked.try_wait().unwrap().is_none(),
            "an unmarked process runs on"
        );

        assert_eq!(stop(marked(other)).await, stopped(1));
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
        assert_eq!(stop(marked(mark)).await, stopped(1));
        assert!(started.elapsed() >= GRACE);
        assert!(!running(stubborn));
    }

    #[tokio::test]
    async fn a_process_that_kill_does_not_end_is_given_up_on() {
        // Above any pid macOS or Linux gives, so `kill` finds no such process.
        let unending = Pid::from_raw(i32::MAX);
        let started = Instant::now();
        let given_up = stop_found(
            || async { (BTreeSet::from([unending]), 0) },
            Duration::from_millis(200),
            Duration::from_millis(300),
        )
        .await;
        assert!(started.elapsed() >= Duration::from_millis(500));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(
            given_up,
            Stopped {
                stopped: 1,
                unknown: 0,
                running: 1
            }
        );
    }

    fn hidden_orphan(started: Timestamp, directory: Option<&str>) -> Process {
        Process {
            pid: Pid::from_raw(1),
            seen: Seen::HiddenOrphan {
                started,
                directory: directory.map(PathBuf::from),
            },
        }
    }

    fn at(seconds: i64) -> Timestamp {
        Timestamp::from_second(seconds).unwrap()
    }

    fn owner_in(directory: &str, since: i64, until: Option<i64>) -> Owner {
        Owner {
            mark: Mark::new_command_run(),
            place: Some(Place {
                directory: PathBuf::from(directory),
                since: at(since),
                until: until.map(at),
            }),
            spared: Vec::new(),
        }
    }

    #[test]
    fn a_hidden_orphan_belongs_where_it_runs_while_its_owner_ran() {
        let owner = owner_in("/data/repos/sess-ab", 100, Some(200));
        let judged = |started, directory| hidden_orphan(at(started), directory).belongs(&owner);
        assert_eq!(judged(150, Some("/data/repos/sess-ab")), Belongs::Yes);
        assert_eq!(judged(150, Some("/data/repos/sess-ab/src")), Belongs::Yes);
        assert_eq!(
            judged(150, Some("/data/repos/sess-abc")),
            Belongs::No,
            "on a component boundary"
        );
        assert_eq!(judged(150, Some("/data/repos")), Belongs::No);
        assert_eq!(judged(150, Some("/data/check")), Belongs::No);
        assert_eq!(
            judged(99, Some("/data/repos/sess-ab")),
            Belongs::No,
            "started before it"
        );
        assert_eq!(
            judged(201, Some("/data/repos/sess-ab")),
            Belongs::No,
            "started after it ended"
        );
        assert_eq!(
            judged(150, None),
            Belongs::Unknown,
            "a directory macOS does not give"
        );
        assert_eq!(judged(99, None), Belongs::No);
    }

    #[test]
    fn a_hidden_orphan_where_a_live_session_runs_is_never_an_ended_ones() {
        let mut ended = owner_in("/data/repos/item", 100, Some(200));
        ended.spared = vec![Place {
            directory: PathBuf::from("/data/repos/item"),
            since: at(190),
            until: None,
        }];
        let judged =
            |started| hidden_orphan(at(started), Some("/data/repos/item/src")).belongs(&ended);
        assert_eq!(judged(150), Belongs::Yes);
        assert_eq!(
            judged(195),
            Belongs::No,
            "the live session's, though the times overlap"
        );
        assert_eq!(
            hidden_orphan(at(195), None).belongs(&ended),
            Belongs::No,
            "nor one that may be the live session's"
        );
    }

    #[test]
    fn a_process_whose_environment_is_read_is_judged_by_its_mark_alone() {
        let owner = owner_in("/data/repos/sess-ab", 100, None);
        let unmarked = Process {
            pid: Pid::from_raw(1),
            seen: Seen::Environment(vec!["HOME=/home/ada".into()]),
        };
        assert_eq!(unmarked.belongs(&owner), Belongs::No);
        let marked = Process {
            pid: Pid::from_raw(1),
            seen: Seen::Environment(
                owner
                    .mark
                    .variables()
                    .into_iter()
                    .map(|(name, value)| format!("{name}={value}").into())
                    .collect(),
            ),
        };
        assert_eq!(marked.belongs(&owner), Belongs::Yes);
    }

    #[test]
    fn a_place_that_is_gone_is_canonical_as_far_as_it_is_there() {
        let directory = tempfile::tempdir().unwrap();
        let gone = directory.path().join("repos").join("sess-ab");
        let place = Place::new(&gone, at(0), None);
        let canonical = directory.path().canonicalize().unwrap();
        assert_eq!(place.directory, canonical.join("repos").join("sess-ab"));
    }

    /// The owner of what runs in `directory` from now on.
    #[cfg(target_os = "macos")]
    fn owner_from_now(directory: &Path) -> Owner {
        Owner {
            mark: Mark::new_command_run(),
            place: Some(Place::new(directory, Timestamp::now(), None)),
            spared: Vec::new(),
        }
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn an_orphaned_apple_program_in_the_owners_directory_is_stopped() {
        use super::testing::OrphanedSleep;
        let root = tempfile::tempdir().unwrap();
        let (inside, outside) = (root.path().join("sess-ab"), root.path().join("sess-abc"));
        std::fs::create_dir_all(inside.join("src")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let before = OrphanedSleep::start(&inside);
        tokio::time::sleep(Duration::from_millis(20)).await;
        let owner = owner_from_now(&inside);
        let orphan = OrphanedSleep::start(&inside.join("src"));
        let elsewhere = OrphanedSleep::start(&outside);
        // Its environment is readable: only a mark makes it the owner's.
        let unmarked = daemonized_in(&inside);

        assert_eq!(stop(owner).await, stopped(1));
        orphan.assert_ended();
        assert!(running(before.0), "an orphan older than its owner runs on");
        assert!(
            running(elsewhere.0),
            "an orphan outside its directory runs on"
        );
        assert!(running(unmarked), "an unmarked process runs on");
        let _ = nix::sys::signal::kill(unmarked, Signal::SIGKILL);
    }

    /// A daemonized test binary without a mark, which runs in `directory`.
    #[cfg(target_os = "macos")]
    fn daemonized_in(directory: &Path) -> Pid {
        let pid_file = directory.join(".sleeper.pid");
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                "\"$0\" --exact processes::testing::sleeper --ignored </dev/null >/dev/null 2>&1 &",
            ])
            .arg(std::env::current_exe().unwrap())
            .current_dir(directory)
            .env(super::testing::SLEEP, &pid_file);
        assert!(command.status().unwrap().success());
        let pid = super::testing::sleeper_pid(&pid_file);
        std::fs::remove_file(&pid_file).unwrap();
        pid
    }

    #[tokio::test]
    async fn a_session_is_marked_by_its_workforce_and_its_id() {
        let directory = tempfile::tempdir().unwrap();
        let workforce = WorkforceId::from_bytes(rand::random());
        let session = SessionId::from_bytes(rand::random());
        let mark = Mark::Session { workforce, session };
        let pid = daemonized(&mark.variables(), directory.path());
        let place = || Place::new(directory.path(), Timestamp::now(), None);
        let elsewhere = WorkforceId::from_bytes(rand::random());
        assert_eq!(
            stop_session(elsewhere, session, place(), Vec::new()).await,
            stopped(0),
            "only in its workforce"
        );
        assert!(running(pid));

        assert_eq!(
            stop_session(workforce, session, place(), Vec::new()).await,
            stopped(1)
        );
        assert!(!running(pid));
    }
}

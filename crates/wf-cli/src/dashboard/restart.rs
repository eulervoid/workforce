//! What the dashboard does when the daemon restarts: connect again, and when the new daemon
//! speaks another protocol, either run the rebuilt `wf` or say which side to restart.

use std::os::unix::fs::MetadataExt;
use std::time::SystemTime;

/// What a daemon that refuses this build's protocol calls for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mismatch {
    /// The daemon is newer and the executable on disk is not the one that started: the rebuilt
    /// `wf` speaks its protocol.
    RunRebuilt,
    /// The daemon is older than this build.
    RestartDaemon,
    /// The daemon is newer, but the executable on disk is the one that started: running it
    /// again would meet the same refusal.
    RebuildAndStart,
}

pub(super) fn on_mismatch(daemon: u32, client: u32, executable_changed: bool) -> Mismatch {
    if daemon <= client {
        Mismatch::RestartDaemon
    } else if executable_changed {
        Mismatch::RunRebuilt
    } else {
        Mismatch::RebuildAndStart
    }
}

/// Which file the executable was when it started, to tell whether a build replaced it since.
pub(super) struct Executable {
    started: Option<Stamp>,
}

#[derive(PartialEq, Eq)]
struct Stamp {
    modified: SystemTime,
    inode: u64,
}

impl Executable {
    pub(super) fn start() -> Self {
        Self { started: stamp() }
    }

    /// Whether the file at the path of the executable is another than the one that started. An
    /// executable that cannot be read counts as unchanged.
    pub(super) fn changed(&self) -> bool {
        match (&self.started, stamp()) {
            (Some(started), Some(now)) => *started != now,
            _ => false,
        }
    }
}

fn stamp() -> Option<Stamp> {
    let metadata = std::fs::metadata(std::env::current_exe().ok()?).ok()?;
    Some(Stamp {
        modified: metadata.modified().ok()?,
        inode: metadata.ino(),
    })
}

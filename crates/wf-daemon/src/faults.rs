//! What is wrong with this machine: writes that fail for lack of space, and harnesses whose login
//! lapsed or that cannot be reached. The daemon remembers since when, so that the inbox tells the
//! principal once what is wrong instead of a comment per session, and so that no session is
//! started that would fail for it. It forgets a fault once the machine is fine again.

use std::collections::BTreeMap;
use std::error::Error;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use wf_core::name::{AgentName, HarnessName};
use wf_core::session::{HarnessFault, MachineFault};

/// What is wrong with this machine now.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineFaults {
    pub out_of_space: Option<OutOfSpace>,
    /// The harnesses whose sessions cannot run.
    pub harnesses: BTreeMap<HarnessName, HarnessAtFault>,
}

impl MachineFaults {
    pub fn is_empty(&self) -> bool {
        self.out_of_space.is_none() && self.harnesses.is_empty()
    }
}

/// The disk the daemon writes to is full.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutOfSpace {
    /// When a write first failed for lack of space, in milliseconds since the Unix epoch.
    pub since_ms: u64,
    /// What could not be written then, with the error.
    pub what: String,
}

/// A harness whose sessions cannot run. Its agents are started again every
/// [`RETRY_HARNESS`]; the first turn it answers shows that it is fine again. Unless no command is
/// known for it: then they wait until there is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessAtFault {
    pub fault: HarnessFault,
    /// When a session of the harness first failed for it, in milliseconds since the Unix epoch.
    pub since_ms: u64,
    /// When one last did.
    pub last_ms: u64,
    /// What the harness said then.
    pub what: String,
}

/// How long the agents of a harness at fault wait before one is started again, to learn whether
/// the principal has fixed it.
pub const RETRY_HARNESS: Duration = Duration::from_secs(10 * 60);

/// The file a probe writes, in the data directory, to learn whether there is space again.
const PROBE_FILE: &str = ".space-probe";

/// As much as a probe writes: a little more than a session needs to go on.
const PROBE_BYTES: usize = 64 * 1024;

#[derive(Default)]
pub(crate) struct Faults(Mutex<MachineFaults>);

impl Faults {
    /// Remembers that `what` could not be written if `error` says the disk is full; the first
    /// such failure is logged, the ones after it are not. Whether it says so.
    pub(crate) fn note_space(&self, what: &str, error: &(dyn Error + 'static)) -> bool {
        if !is_out_of_space(error) {
            return false;
        }
        let mut faults = self.lock();
        if faults.out_of_space.is_none() {
            let what = format!("{what}: {}", wf_core::error_chain(error));
            tracing::warn!("the disk is full; nothing moves until there is space: {what}");
            faults.out_of_space = Some(OutOfSpace {
                since_ms: crate::now_ms(),
                what,
            });
        }
        true
    }

    /// Remembers that a session of `harness` failed for `fault`, as `what` says.
    pub(crate) fn note_harness(&self, harness: &HarnessName, fault: HarnessFault, what: String) {
        let now_ms = crate::now_ms();
        let mut faults = self.lock();
        match faults.harnesses.get_mut(harness) {
            Some(known) if known.fault == fault => known.last_ms = now_ms,
            Some(_) | None => {
                tracing::warn!(
                    "{harness}: {}; its agents are tried again every {}: {what}",
                    MachineFault::from(fault).name().replace('_', " "),
                    humantime::format_duration(RETRY_HARNESS)
                );
                faults.harnesses.insert(
                    harness.clone(),
                    HarnessAtFault {
                        fault,
                        since_ms: now_ms,
                        last_ms: now_ms,
                        what,
                    },
                );
            }
        }
    }

    /// Makes the harnesses that no command is known for, with the agents that use each, the
    /// unknown ones at fault: logs when one starts and when one clears, and nothing in between.
    pub(crate) fn sync_unknown_harnesses(&self, unknown: BTreeMap<HarnessName, Vec<AgentName>>) {
        let now_ms = crate::now_ms();
        let mut faults = self.lock();
        let cleared: Vec<_> = faults
            .harnesses
            .iter()
            .filter(|(harness, at_fault)| {
                at_fault.fault == HarnessFault::Unknown && !unknown.contains_key(*harness)
            })
            .map(|(harness, _)| harness.clone())
            .collect();
        for harness in cleared {
            tracing::info!("{harness} is known again");
            faults.harnesses.remove(&harness);
        }
        for (harness, agents) in unknown {
            let names: Vec<_> = agents.iter().map(ToString::to_string).collect();
            let what = format!("used by {}", names.join(", "));
            match faults.harnesses.get_mut(&harness) {
                Some(known) if known.fault == HarnessFault::Unknown => {
                    known.last_ms = now_ms;
                    known.what = what;
                }
                Some(_) | None => {
                    tracing::warn!(
                        "{harness} is not a harness wf knows how to start; its agents do not \
                         start: {what}"
                    );
                    faults.harnesses.insert(
                        harness,
                        HarnessAtFault {
                            fault: HarnessFault::Unknown,
                            since_ms: now_ms,
                            last_ms: now_ms,
                            what,
                        },
                    );
                }
            }
        }
    }

    /// Forgets what was wrong with `harness`: it has answered a turn.
    pub(crate) fn harness_answered(&self, harness: &HarnessName) {
        if self.lock().harnesses.remove(harness).is_some() {
            tracing::info!("{harness} answers again");
        }
    }

    /// The fault that keeps any session from going on: a full disk.
    pub(crate) fn blocking_all(&self) -> Option<MachineFault> {
        self.lock()
            .out_of_space
            .as_ref()
            .map(|_| MachineFault::NoSpace)
    }

    /// Whether an agent of `harness` is to wait before it is started: while the disk is full, for
    /// as long as no command is known for the harness, and for [`RETRY_HARNESS`] after a session
    /// of the harness failed for another fault of it.
    pub(crate) fn holds(&self, harness: &HarnessName, now_ms: u64) -> bool {
        let faults = self.lock();
        let retry_ms = u64::try_from(RETRY_HARNESS.as_millis()).unwrap_or(u64::MAX);
        faults.out_of_space.is_some()
            || faults.harnesses.get(harness).is_some_and(|at_fault| {
                at_fault.fault == HarnessFault::Unknown
                    || now_ms.saturating_sub(at_fault.last_ms) < retry_ms
            })
    }

    pub(crate) fn current(&self) -> MachineFaults {
        self.lock().clone()
    }

    /// Forgets that the disk is full once a probe write in `directory` succeeds.
    pub(crate) fn check_space(&self, directory: &Path) {
        if self.lock().out_of_space.is_none() {
            return;
        }
        let probe = directory.join(PROBE_FILE);
        let written = std::fs::write(&probe, vec![0u8; PROBE_BYTES]);
        let _ = std::fs::remove_file(&probe);
        if written.is_ok() {
            tracing::info!("there is space on the disk again");
            self.lock().out_of_space = None;
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MachineFaults> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Whether `error`, or an error it comes from, says the disk is full. Git only says so in its
/// output, so the message is read as well.
pub(crate) fn is_out_of_space(error: &(dyn Error + 'static)) -> bool {
    std::iter::successors(Some(error), |error| (*error).source()).any(|error| {
        let full = error.downcast_ref::<std::io::Error>().is_some_and(|error| {
            matches!(
                error.kind(),
                std::io::ErrorKind::StorageFull | std::io::ErrorKind::QuotaExceeded
            )
        });
        full || error.to_string().contains("No space left on device")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, thiserror::Error)]
    #[error("writing the log failed")]
    struct Wrapped(#[source] std::io::Error);

    #[test]
    fn a_full_disk_is_remembered_once_until_there_is_space() {
        let directory = tempfile::tempdir().unwrap();
        let faults = Faults::default();
        assert!(!faults.note_space("a transcript", &std::io::Error::other("permission denied")));
        assert_eq!(
            faults.current(),
            MachineFaults::default(),
            "other failures are not remembered"
        );

        assert!(faults.note_space(
            "the log",
            &Wrapped(std::io::Error::from(std::io::ErrorKind::StorageFull)),
        ));
        let first = faults
            .current()
            .out_of_space
            .expect("a full disk is remembered");
        assert!(first.what.starts_with("the log: writing the log failed"));
        faults.note_space(
            "a worktree",
            &std::io::Error::other("fatal: Unable to create 'x.lock': No space left on device"),
        );
        assert_eq!(
            faults.current().out_of_space,
            Some(first),
            "the first failure is kept"
        );
        assert_eq!(faults.blocking_all(), Some(MachineFault::NoSpace));

        faults.check_space(&directory.path().join("missing"));
        assert!(
            faults.current().out_of_space.is_some(),
            "a probe that fails keeps it"
        );
        faults.check_space(directory.path());
        assert!(faults.current().is_empty());
        assert!(!directory.path().join(PROBE_FILE).exists());
    }

    #[test]
    fn an_unknown_harness_is_held_until_a_command_is_known() {
        let faults = Faults::default();
        let typo = HarnessName::new("claud-code").unwrap();
        let betty = AgentName::new("betty").unwrap();
        let unknown = BTreeMap::from([(typo.clone(), vec![betty])]);
        faults.sync_unknown_harnesses(unknown.clone());
        let first = faults.current().harnesses[&typo].clone();
        assert_eq!(first.fault, HarnessFault::Unknown);
        assert_eq!(first.what, "used by betty");
        faults.sync_unknown_harnesses(unknown);
        assert_eq!(faults.current().harnesses[&typo].since_ms, first.since_ms);
        assert!(faults.holds(&typo, first.last_ms + 10 * RETRY_HARNESS.as_millis() as u64));

        faults.sync_unknown_harnesses(BTreeMap::new());
        assert!(faults.current().is_empty());
    }

    #[test]
    fn a_harness_at_fault_is_announced_once_and_held_until_it_is_tried_again() {
        let faults = Faults::default();
        let claude = HarnessName::new("claude-code").unwrap();
        let codex = HarnessName::new("codex").unwrap();
        faults.note_harness(
            &claude,
            HarnessFault::LoginLapsed,
            "Authentication required".to_string(),
        );
        let first = faults.current().harnesses[&claude].clone();
        faults.note_harness(&claude, HarnessFault::LoginLapsed, "again".to_string());
        let again = faults.current().harnesses[&claude].clone();
        assert_eq!(
            (again.since_ms, again.what.as_str()),
            (first.since_ms, "Authentication required"),
            "the fault is announced once, since it first happened"
        );

        let retry_ms = RETRY_HARNESS.as_millis() as u64;
        assert!(faults.holds(&claude, again.last_ms + retry_ms - 1));
        assert!(
            !faults.holds(&claude, again.last_ms + retry_ms),
            "tried again"
        );
        assert!(!faults.holds(&codex, again.last_ms), "other harnesses run");
        assert_eq!(faults.blocking_all(), None);

        faults.harness_answered(&claude);
        assert!(faults.current().is_empty());
    }
}

//! Writes that fail for lack of space. The daemon remembers since when, so that the inbox tells
//! the principal once why nothing moves, and forgets it once a write succeeds again.

use std::error::Error;
use std::path::Path;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// The disk the daemon writes to is full.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutOfSpace {
    /// When a write first failed for lack of space, in milliseconds since the Unix epoch.
    pub since_ms: u64,
    /// What could not be written then, with the error.
    pub what: String,
}

/// The file a probe writes, in the data directory, to learn whether there is space again.
const PROBE_FILE: &str = ".space-probe";

/// As much as a probe writes: a little more than a session needs to go on.
const PROBE_BYTES: usize = 64 * 1024;

#[derive(Default)]
pub(crate) struct Space(Mutex<Option<OutOfSpace>>);

impl Space {
    /// Remembers that `what` could not be written if `error` says the disk is full; the first
    /// such failure is logged, the ones after it are not.
    pub(crate) fn note(&self, what: &str, error: &(dyn Error + 'static)) {
        if !is_out_of_space(error) {
            return;
        }
        let mut current = self.lock();
        if current.is_none() {
            let what = format!("{what}: {}", wf_core::error_chain(error));
            tracing::warn!("the disk is full; nothing moves until there is space: {what}");
            *current = Some(OutOfSpace {
                since_ms: crate::now_ms(),
                what,
            });
        }
    }

    pub(crate) fn current(&self) -> Option<OutOfSpace> {
        self.lock().clone()
    }

    /// Forgets that the disk is full once a probe write in `directory` succeeds.
    pub(crate) fn check(&self, directory: &Path) {
        if self.lock().is_none() {
            return;
        }
        let probe = directory.join(PROBE_FILE);
        let written = std::fs::write(&probe, vec![0u8; PROBE_BYTES]);
        let _ = std::fs::remove_file(&probe);
        if written.is_ok() {
            tracing::info!("there is space on the disk again");
            *self.lock() = None;
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<OutOfSpace>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Whether `error`, or an error it comes from, says the disk is full. Git only says so in its
/// output, so the message is read as well.
fn is_out_of_space(error: &(dyn Error + 'static)) -> bool {
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
        let space = Space::default();
        space.note("a transcript", &std::io::Error::other("permission denied"));
        assert_eq!(space.current(), None, "other failures are not remembered");

        space.note(
            "the log",
            &Wrapped(std::io::Error::from(std::io::ErrorKind::StorageFull)),
        );
        let first = space.current().expect("a full disk is remembered");
        assert!(first.what.starts_with("the log: writing the log failed"));
        space.note(
            "a worktree",
            &std::io::Error::other("fatal: Unable to create 'x.lock': No space left on device"),
        );
        assert_eq!(space.current(), Some(first), "the first failure is kept");

        space.check(&directory.path().join("missing"));
        assert!(space.current().is_some(), "a probe that fails keeps it");
        space.check(directory.path());
        assert_eq!(space.current(), None);
        assert!(!directory.path().join(PROBE_FILE).exists());
    }
}

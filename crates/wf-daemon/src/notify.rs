//! Tells the principal when something new waits on them, with the command under `[notify]` in
//! the machine config: once for each new entry of the inbox. Permission requests are told of
//! where they are held, as they are not part of the state.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use wf_core::id::{HashId, WorkforceId};
use wf_core::inbox::InboxEntry;
use wf_core::state::State;

use crate::machine_config::{Notice, NotifyConfig};
use crate::{Daemon, Loaded};

/// The file in the directory of a workforce with the inbox entries the principal was told of, so
/// that a restart does not tell of them again.
const ANNOUNCED_FILE: &str = "announced.json";

/// A notify command that takes longer is stopped.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);

/// Tells of new inbox entries of a workforce for as long as the daemon runs and the storage of
/// the workforce does: once when it starts, and whenever the log changes.
pub(crate) async fn run_notifier(
    daemon: std::sync::Weak<Daemon>,
    workforce: WorkforceId,
    loaded: Arc<Loaded>,
) {
    let mut heads = loaded.storage.watch_heads();
    loop {
        let Some(strong) = daemon.upgrade() else {
            return;
        };
        if *strong.halted.borrow() {
            return;
        }
        strong.announce_new_entries(workforce, &loaded).await;
        let halted = async { strong.until_halted().await };
        tokio::select! {
            changed = heads.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            () = halted => return,
        }
    }
}

impl Daemon {
    async fn announce_new_entries(&self, workforce: WorkforceId, loaded: &Loaded) {
        let notify = match self.machine_config() {
            Ok(config) => config.notify,
            Err(error) => {
                tracing::warn!("not notifying: {}", wf_core::error_chain(&error));
                return;
            }
        };
        if notify.command.is_none() {
            return;
        }
        let Ok(state) = self.snapshot(loaded).await else {
            return;
        };
        let announced = Announced::new(self.workforce_directory(workforce));
        for entry in announced.take_new(state.inbox()) {
            run_command(&notify, &notice(&state, &entry));
        }
    }
}

fn notice(state: &State, entry: &InboxEntry) -> Notice {
    Notice {
        kind: entry.waiting.kind(),
        item: entry.item.short(),
        title: state.inbox_title(entry.item),
        what: state.what_waits(entry),
    }
}

/// The inbox entries the principal was told of, kept beside the store of the workforce.
struct Announced {
    path: PathBuf,
}

impl Announced {
    fn new(workforce_directory: PathBuf) -> Self {
        Announced {
            path: workforce_directory.join(ANNOUNCED_FILE),
        }
    }

    /// The entries of `inbox` that were not told of yet, oldest first. Remembers `inbox` as told
    /// of, which forgets entries that are gone: an item that comes back to wait is told of again.
    fn take_new(&self, inbox: Vec<InboxEntry>) -> Vec<InboxEntry> {
        let announced = self.load();
        let new: Vec<InboxEntry> = inbox
            .iter()
            .rev()
            .filter(|entry| !announced.contains(entry))
            .cloned()
            .collect();
        if announced != inbox
            && let Err(error) = save(&self.path, &inbox)
        {
            tracing::warn!(
                "{}: {}; what waits may be told of again",
                self.path.display(),
                error
            );
        }
        new
    }

    /// What was told of; nothing if the file is missing or unreadable.
    fn load(&self) -> Vec<InboxEntry> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(error) => {
                tracing::warn!("{}: {error}", self.path.display());
                return Vec::new();
            }
        };
        serde_json::from_str(&text).unwrap_or_else(|error| {
            tracing::warn!("{}: {error}", self.path.display());
            Vec::new()
        })
    }
}

/// Replaces the file in one step, so a crash leaves the old list or the new one.
fn save(path: &Path, entries: &[InboxEntry]) -> std::io::Result<()> {
    let text = serde_json::to_string(entries).map_err(std::io::Error::other)?;
    let temporary = path.with_extension(format!("json.{}", std::process::id()));
    std::fs::write(&temporary, text)?;
    std::fs::rename(temporary, path)
}

/// Runs the notify command for `notice` in the background, without a shell. A command that fails
/// is logged and nothing waits for it.
pub(crate) fn run_command(config: &NotifyConfig, notice: &Notice) {
    let Some(command) = config.command_for(notice) else {
        return;
    };
    tokio::spawn(async move {
        let Some((program, arguments)) = command.split_first() else {
            return;
        };
        let output = tokio::process::Command::new(program)
            .args(arguments)
            .kill_on_drop(true)
            .output();
        match tokio::time::timeout(COMMAND_TIMEOUT, output).await {
            Ok(Ok(output)) if output.status.success() => {}
            Ok(Ok(output)) => tracing::warn!(
                "the notify command `{program}` failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            Ok(Err(error)) => {
                tracing::warn!("could not run the notify command `{program}`: {error}")
            }
            Err(_) => tracing::warn!(
                "the notify command `{program}` did not finish within {}s and was stopped",
                COMMAND_TIMEOUT.as_secs()
            ),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use wf_core::id::{CommentId, ItemId};
    use wf_core::inbox::Waiting;

    fn blocker(byte: u8) -> InboxEntry {
        InboxEntry {
            item: ItemId::from_bytes([byte; 32]),
            waiting: Waiting::Blocker {
                comment: CommentId::from_bytes([byte; 32]),
            },
        }
    }

    #[test]
    fn each_entry_is_told_of_once_across_restarts() {
        let directory = tempfile::tempdir().unwrap();
        let (first, second) = (blocker(1), blocker(2));
        let announced = Announced::new(directory.path().to_path_buf());
        assert_eq!(announced.take_new(vec![first.clone()]), vec![first.clone()]);
        assert_eq!(announced.take_new(vec![first.clone()]), []);

        let restarted = Announced::new(directory.path().to_path_buf());
        assert_eq!(
            restarted.take_new(vec![second.clone(), first.clone()]),
            vec![second.clone()]
        );
        assert_eq!(restarted.take_new(vec![second.clone()]), []);
        assert_eq!(
            restarted.take_new(vec![first.clone(), second]),
            [first],
            "an entry that was gone and is back is told of again"
        );
    }
}

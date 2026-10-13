//! Tells the principal when something new waits on them, with the command under `[notify]` in
//! the machine config: once for each new entry of the inbox. Permission requests are told of
//! where they are held, as they are not part of the state.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use wf_core::id::{HashId, ItemId};
use wf_core::inbox::{InboxEntry, Waiting};
use wf_core::state::State;

use crate::machine_config::{Notice, NotifyConfig};
use crate::{Daemon, Loaded};

/// The file in the directory of a workforce with the inbox entries the principal was told of, so
/// that a restart does not tell of them again.
const ANNOUNCED_FILE: &str = "announced.json";

/// A notify command that takes longer is stopped.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);

/// Tells the principal of the new entries of the inbox of one workforce. The dispatcher runs it
/// after each of its turns, so that work it is about to merge or finish is not told of.
pub(crate) struct Notifier {
    path: PathBuf,
    /// The entries told of, kept in memory so that a file that cannot be written does not make
    /// every entry be told of again.
    announced: Vec<InboxEntry>,
    /// What was wrong with the machine config when it was last read, so it is logged once.
    broken_config: Option<String>,
}

impl Notifier {
    /// Reads what was told of from the directory of the workforce.
    pub(crate) fn load(workforce_directory: &Path) -> Self {
        let path = workforce_directory.join(ANNOUNCED_FILE);
        let announced = load(&path);
        Notifier {
            path,
            announced,
            broken_config: None,
        }
    }

    /// Tells of what waits on the principal and was not told of yet. `waiting_merges` are the
    /// items whose merge the dispatcher just tried and found the checkout with changes.
    pub(crate) async fn announce(
        &mut self,
        daemon: &Daemon,
        loaded: &Loaded,
        waiting_merges: &BTreeSet<ItemId>,
    ) {
        let notify = match daemon.machine_config() {
            Ok(config) => {
                self.broken_config = None;
                config.notify
            }
            Err(error) => {
                let error = wf_core::error_chain(&error);
                if self.broken_config.as_ref() != Some(&error) {
                    tracing::warn!("not notifying: {error}");
                    self.broken_config = Some(error);
                }
                return;
            }
        };
        if notify.command.is_none() {
            return;
        }
        let Ok(state) = daemon.snapshot(loaded).await else {
            return;
        };
        let waits_on_principal = |entry: &InboxEntry| match &entry.waiting {
            Waiting::Merge { .. } => waiting_merges.contains(&entry.item),
            Waiting::Proposal
            | Waiting::Blocker { .. }
            | Waiting::PrincipalReview { .. }
            | Waiting::Recovery { .. } => true,
        };
        for entry in self.take_new(state.inbox(), waits_on_principal) {
            run_command(&notify, &notice(&state, &entry));
        }
    }

    /// The entries of `inbox` that wait on the principal and were not told of yet, oldest first.
    /// Remembers what of `inbox` was told of, which forgets entries that are gone: an item that
    /// comes back to wait is told of again. An entry that does not wait on the principal yet is
    /// neither told of nor forgotten.
    fn take_new(
        &mut self,
        inbox: Vec<InboxEntry>,
        waits_on_principal: impl Fn(&InboxEntry) -> bool,
    ) -> Vec<InboxEntry> {
        let (told, untold): (Vec<InboxEntry>, Vec<InboxEntry>) = inbox
            .into_iter()
            .filter(|entry| self.announced.contains(entry) || waits_on_principal(entry))
            .partition(|entry| self.announced.contains(entry));
        let new: Vec<InboxEntry> = untold.iter().rev().cloned().collect();
        if new.is_empty() && told.len() == self.announced.len() {
            return new;
        }
        self.announced = told;
        self.announced.extend(untold);
        if let Err(error) = save(&self.path, &self.announced) {
            tracing::warn!(
                "{}: {error}; after a restart, what waits may be told of again",
                self.path.display()
            );
        }
        new
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

/// What was told of; nothing if the file is missing or unreadable.
fn load(path: &Path) -> Vec<InboxEntry> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            tracing::warn!("{}: {error}", path.display());
            return Vec::new();
        }
    };
    serde_json::from_str(&text).unwrap_or_else(|error| {
        tracing::warn!("{}: {error}", path.display());
        Vec::new()
    })
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

    fn blocker(byte: u8) -> InboxEntry {
        InboxEntry {
            item: ItemId::from_bytes([byte; 32]),
            waiting: Waiting::Blocker {
                comment: CommentId::from_bytes([byte; 32]),
            },
        }
    }

    fn waits(_: &InboxEntry) -> bool {
        true
    }

    #[test]
    fn each_entry_is_told_of_once_across_restarts() {
        let directory = tempfile::tempdir().unwrap();
        let (first, second) = (blocker(1), blocker(2));
        let mut notifier = Notifier::load(directory.path());
        assert_eq!(
            notifier.take_new(vec![first.clone()], waits),
            vec![first.clone()]
        );
        assert_eq!(notifier.take_new(vec![first.clone()], waits), []);

        let mut restarted = Notifier::load(directory.path());
        assert_eq!(
            restarted.take_new(vec![second.clone(), first.clone()], waits),
            vec![second.clone()]
        );
        assert_eq!(restarted.take_new(vec![second.clone()], waits), []);
        assert_eq!(
            restarted.take_new(vec![first.clone(), second], waits),
            [first],
            "an entry that was gone and is back is told of again"
        );
    }

    #[test]
    fn what_cannot_be_saved_is_still_told_of_once() {
        let directory = tempfile::tempdir().unwrap();
        let mut notifier = Notifier::load(&directory.path().join("missing"));
        let first = blocker(1);
        assert_eq!(
            notifier.take_new(vec![first.clone()], waits),
            vec![first.clone()]
        );
        assert_eq!(notifier.take_new(vec![first], waits), []);
    }

    #[test]
    fn an_entry_that_does_not_wait_yet_is_neither_told_of_nor_forgotten() {
        let directory = tempfile::tempdir().unwrap();
        let (first, second) = (blocker(1), blocker(2));
        let mut notifier = Notifier::load(directory.path());
        let only_first = |entry: &InboxEntry| *entry == first;
        assert_eq!(
            notifier.take_new(vec![first.clone(), second.clone()], only_first),
            vec![first.clone()]
        );
        assert_eq!(
            notifier.take_new(vec![first.clone(), second.clone()], |_: &InboxEntry| false),
            [],
            "what was told of stays told of"
        );
        assert_eq!(
            notifier.take_new(vec![first, second.clone()], waits),
            [second]
        );
    }
}

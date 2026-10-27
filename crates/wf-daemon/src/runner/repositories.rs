//! Tidying the repositories of a workforce: the directories of ended sessions and of items that
//! are finished removed, and the caches of the rest kept under the budget of the machine config.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use wf_core::id::{HashId, ItemId, SessionId, WorkforceId};
use wf_core::item::ItemEntry;
use wf_core::session::{MachineFault, SessionRole};
use wf_core::state::State;
use wf_vcs::{item_repository, remove_agent_repository, repositories};

use super::blocking;
use crate::{Daemon, Loaded, caches};

impl Daemon {
    /// Removes the directory an ended session ran in, unless it is the repository of the item,
    /// which later sessions continue.
    pub(super) async fn discard_session_directory(
        &self,
        workforce: WorkforceId,
        item: ItemId,
        session: SessionId,
        role: SessionRole,
    ) {
        let directory = match role {
            SessionRole::Work => return,
            SessionRole::Review { .. } | SessionRole::Supervise | SessionRole::Diagnose => {
                self.session_directory(workforce, item, session, &role)
            }
        };
        let removed = match blocking(move || remove_agent_repository(&directory)).await {
            Ok(removed) => removed.map_err(|error| wf_core::error_chain(&error)),
            Err(error) => Err(error.to_string()),
        };
        if let Err(error) = removed {
            tracing::warn!(
                "could not remove the directory of a session on #{}: {error}",
                item.short()
            );
        }
    }

    /// The directories the live sessions of this machine run in.
    fn busy_directories(&self, workforce: WorkforceId, state: &State) -> BTreeSet<PathBuf> {
        state
            .live_sessions()
            .filter(|session| session.device() == self.device())
            .map(|session| {
                self.session_directory(workforce, session.item(), session.id(), &session.role())
            })
            .collect()
    }

    /// Removes the directories in the repositories of `workforce` that belong to no unfinished
    /// item and no live session of this machine, such as those an earlier daemon left behind.
    pub(crate) async fn remove_stray_repositories(&self, workforce: WorkforceId, loaded: &Loaded) {
        let Ok(state) = self.snapshot(loaded).await else {
            return;
        };
        // Without a genesis, which a workforce taken in from another device lacks at first, no
        // item is known to be unfinished, so nothing can be told stray.
        if state.workforce_id().is_none() {
            return;
        }
        let workforce_directory = self.workforce_directory(workforce);
        let mut kept = self.busy_directories(workforce, &state);
        for view in state.items() {
            match view {
                ItemEntry::Live(item) if !item.state().is_terminal() => {
                    kept.insert(item_repository(&workforce_directory, item.id()));
                }
                ItemEntry::Live(_) | ItemEntry::Archived(_) => {}
            }
        }
        let directory = repositories(&workforce_directory);
        let _ = blocking(move || {
            let Ok(entries) = std::fs::read_dir(&directory) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if kept.contains(&path) || !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    continue;
                }
                match std::fs::remove_dir_all(&path) {
                    Ok(()) => tracing::info!("removed the stray directory {}", path.display()),
                    Err(error) => tracing::warn!(
                        "could not remove the stray directory {}: {error}",
                        path.display()
                    ),
                }
            }
        })
        .await;
    }

    /// While the caches in the agent repositories of `workforce` take more than the budget, deletes those
    /// of the least recently used repository that no live session runs in. Without `[storage]` in
    /// the machine config, nothing is deleted. Measuring walks every cache, so it happens at most
    /// every [`MEASURE_CACHES_EVERY`], unless the disk is full.
    pub(crate) async fn keep_caches_in_budget(&self, workforce: WorkforceId, loaded: &Loaded) {
        let disk_full = self.faults.blocking_all() == Some(MachineFault::NoSpace);
        {
            let mut measured = self
                .caches_measured
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !due_for_measuring(measured.get(&workforce).copied(), disk_full) {
                return;
            }
            measured.insert(workforce, Instant::now());
        }
        let storage = match self.machine_config() {
            Ok(config) => config.storage,
            Err(error) => {
                tracing::warn!("caches not checked: {}", wf_core::error_chain(&error));
                return;
            }
        };
        let Some(storage) = storage else {
            self.cache_usage().remove(&workforce);
            return;
        };
        let Ok(state) = self.snapshot(loaded).await else {
            return;
        };
        let busy = self.busy_directories(workforce, &state);
        let directory = self.launch.data_dir.repositories(&workforce);
        let budget_bytes = storage.budget.as_u64();
        let Ok(used_bytes) = blocking(move || {
            let measured = caches::measure(&directory, &storage.caches);
            let mut used_bytes = caches::total(&measured);
            for repository in caches::over_budget(&measured, budget_bytes, &busy) {
                match caches::delete(repository, &storage.caches) {
                    Ok(()) => {
                        used_bytes -= measured
                            .iter()
                            .find(|caches| caches.repository == repository)
                            .map_or(0, |caches| caches.bytes);
                        tracing::info!(
                            "deleted the caches of {} to stay under the budget of {}",
                            repository.display(),
                            storage.budget.display().si()
                        );
                    }
                    Err(error) => tracing::warn!(
                        "could not delete the caches of {}: {error}",
                        repository.display()
                    ),
                }
            }
            used_bytes
        })
        .await
        else {
            return;
        };
        self.cache_usage().insert(
            workforce,
            caches::CacheUsage {
                used_bytes,
                budget_bytes,
            },
        );
    }

    /// What the caches of `workforce` took when they were last checked against the budget.
    pub(crate) fn cache_usage_of(&self, workforce: WorkforceId) -> Option<caches::CacheUsage> {
        self.cache_usage().get(&workforce).copied()
    }

    fn cache_usage(&self) -> std::sync::MutexGuard<'_, BTreeMap<WorkforceId, caches::CacheUsage>> {
        self.cache_usage
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// How long the caches are left unmeasured after they were.
const MEASURE_CACHES_EVERY: Duration = Duration::from_secs(300);

/// Whether the caches are measured again: when they were last measured at least
/// [`MEASURE_CACHES_EVERY`] ago or never, and at once while the disk is full.
fn due_for_measuring(last_measured: Option<Instant>, disk_full: bool) -> bool {
    disk_full || last_measured.is_none_or(|at| at.elapsed() >= MEASURE_CACHES_EVERY)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caches_are_measured_again_after_the_interval_or_while_the_disk_is_full() {
        let just_now = Some(Instant::now());
        let long_ago = Instant::now().checked_sub(MEASURE_CACHES_EVERY * 2);
        assert!(due_for_measuring(None, false));
        assert!(!due_for_measuring(just_now, false));
        assert!(due_for_measuring(long_ago, false));
        assert!(due_for_measuring(just_now, true));
    }
}

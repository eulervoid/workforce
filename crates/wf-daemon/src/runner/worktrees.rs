//! Tidying the worktrees of a workforce: their caches kept under the budget of the machine config.

use std::collections::BTreeSet;
use std::path::PathBuf;

use wf_core::id::WorkforceId;
use wf_core::state::State;
use wf_vcs::worktrees;

use super::blocking;
use crate::{Daemon, Loaded, caches};

impl Daemon {
    /// The directories the live sessions of this machine run in.
    fn busy_directories(&self, workforce: WorkforceId, state: &State) -> BTreeSet<PathBuf> {
        state
            .live_sessions()
            .filter(|session| session.device() == self.device())
            .map(|session| self.session_directory(workforce, session.item(), session.role()))
            .collect()
    }

    /// While the caches in the worktrees of `workforce` take more than the budget, deletes those
    /// of the least recently used worktree that no live session runs in. Without `[storage]` in
    /// the machine config, nothing is deleted.
    pub(crate) async fn keep_caches_in_budget(&self, workforce: WorkforceId, loaded: &Loaded) {
        let storage = match self.machine_config() {
            Ok(config) => config.storage,
            Err(error) => {
                tracing::warn!("caches not checked: {}", wf_core::error_chain(&error));
                return;
            }
        };
        let Some(storage) = storage else {
            return;
        };
        let Ok(state) = self.snapshot(loaded).await else {
            return;
        };
        let busy = self.busy_directories(workforce, &state);
        let directory = worktrees(&self.workforce_directory(workforce));
        let _ = blocking(move || {
            let measured = caches::measure(&directory, &storage.caches);
            for worktree in caches::over_budget(&measured, storage.budget.as_u64(), &busy) {
                match caches::delete(worktree, &storage.caches) {
                    Ok(()) => tracing::info!(
                        "deleted the caches of {} to stay under the budget of {}",
                        worktree.display(),
                        storage.budget.display().si()
                    ),
                    Err(error) => tracing::warn!(
                        "could not delete the caches of {}: {error}",
                        worktree.display()
                    ),
                }
            }
        })
        .await;
    }
}

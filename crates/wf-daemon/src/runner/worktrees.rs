//! Tidying the worktrees of a workforce: the directories of ended sessions removed, and their
//! caches kept under the budget of the machine config.

use std::collections::BTreeSet;
use std::path::PathBuf;

use wf_core::id::{HashId, ItemId, WorkforceId};
use wf_core::session::SessionRole;
use wf_core::state::State;
use wf_vcs::{GitCli, VcsAdapter, worktrees};

use super::blocking;
use crate::{Daemon, Loaded, caches};

impl Daemon {
    /// Removes the directory an ended session ran in, unless it is the worktree of the item,
    /// which later sessions continue.
    pub(super) async fn discard_session_directory(
        &self,
        workforce: WorkforceId,
        item: ItemId,
        role: SessionRole,
    ) {
        let directory = self.session_directory(workforce, item, role);
        let removed = match role {
            SessionRole::Work => return,
            SessionRole::Review { commit: Some(_) } => {
                let Some(repository) = self.repository_of(workforce) else {
                    return;
                };
                blocking(move || GitCli::open(&repository)?.remove_worktree(&directory))
                    .await
                    .map(|removed| removed.map_err(|error| wf_core::error_chain(&error)))
            }
            SessionRole::Review { commit: None }
            | SessionRole::Supervise
            | SessionRole::Diagnose => {
                blocking(move || match std::fs::remove_dir_all(&directory) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    removed => removed,
                })
                .await
                .map(|removed| removed.map_err(|error| error.to_string()))
            }
        };
        if let Ok(Err(error)) = removed {
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

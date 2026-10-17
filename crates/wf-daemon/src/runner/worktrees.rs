//! Tidying the worktrees of a workforce: the directories of ended sessions and of items that are
//! finished removed, and the caches of the rest kept under the budget of the machine config.

use std::collections::BTreeSet;
use std::path::PathBuf;

use wf_core::id::{HashId, ItemId, SessionId, WorkforceId};
use wf_core::item::ItemView;
use wf_core::session::SessionRole;
use wf_core::state::State;
use wf_vcs::{GitCli, VcsAdapter, item_worktree, worktrees};

use super::blocking;
use crate::{Daemon, Loaded, caches};

impl Daemon {
    /// Removes the directory an ended session ran in, unless it is the worktree of the item,
    /// which later sessions continue: there only a rebase stopped by a refused submission is
    /// given up, and not while another work session of the item is live.
    pub(super) async fn discard_session_directory(
        &self,
        workforce: WorkforceId,
        loaded: &Loaded,
        item: ItemId,
        session: SessionId,
        role: SessionRole,
    ) {
        let directory = self.session_directory(workforce, item, session, role);
        let removed = match role {
            SessionRole::Work => {
                let another_live = self.snapshot(loaded).await.map_or(true, |state| {
                    another_work_session_live(
                        state
                            .live_sessions()
                            .map(|live| (live.item(), live.id(), live.role())),
                        item,
                        session,
                    )
                });
                if !another_live {
                    self.abort_stopped_rebase(workforce, item).await;
                }
                return;
            }
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

    /// Gives up a rebase that a refused submission left stopped in the worktree of `item`, once
    /// its session ended without submitting, so the next session finds the branch checked out.
    async fn abort_stopped_rebase(&self, workforce: WorkforceId, item: ItemId) {
        let Some(repository) = self.repository_of(workforce) else {
            return;
        };
        let worktree = item_worktree(&self.workforce_directory(workforce), item);
        if !worktree.exists() {
            return;
        }
        let aborted = blocking(move || GitCli::open(&repository)?.abort_rebase(&worktree)).await;
        if let Ok(Err(error)) = aborted {
            tracing::warn!(
                "could not give up the stopped rebase of #{}: {}",
                item.short(),
                wf_core::error_chain(&error)
            );
        }
    }

    /// The directories the live sessions of this machine run in.
    fn busy_directories(&self, workforce: WorkforceId, state: &State) -> BTreeSet<PathBuf> {
        state
            .live_sessions()
            .filter(|session| session.device() == self.device())
            .map(|session| {
                self.session_directory(workforce, session.item(), session.id(), session.role())
            })
            .collect()
    }

    /// Removes the directories in the worktrees of `workforce` that belong to no unfinished item
    /// and no live session of this machine, such as those an earlier daemon left behind, and
    /// makes git forget them.
    pub(crate) async fn remove_stray_worktrees(&self, workforce: WorkforceId, loaded: &Loaded) {
        let Ok(state) = self.snapshot(loaded).await else {
            return;
        };
        // Without a policy, which a workforce taken in from another device lacks at first, no
        // item is known to be unfinished, so nothing can be told stray.
        let Some(policy) = state.policy().map(|config| &config.policy) else {
            return;
        };
        let workforce_directory = self.workforce_directory(workforce);
        let mut kept = self.busy_directories(workforce, &state);
        for view in state.items() {
            match view {
                ItemView::Live(item) if !item.state(policy).is_terminal() => {
                    kept.insert(item_worktree(&workforce_directory, item.id()));
                }
                ItemView::Live(_) | ItemView::Archived(_) => {}
            }
        }
        let directory = worktrees(&workforce_directory);
        let repository = self.repository_of(workforce);
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
                    Ok(()) => tracing::info!("removed the stray worktree {}", path.display()),
                    Err(error) => tracing::warn!(
                        "could not remove the stray worktree {}: {error}",
                        path.display()
                    ),
                }
            }
            if let Some(repository) = repository
                && let Err(error) = GitCli::open(&repository).and_then(|git| git.prune_worktrees())
            {
                tracing::warn!(
                    "could not prune the worktrees of {}: {}",
                    repository.display(),
                    wf_core::error_chain(&error)
                );
            }
        })
        .await;
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

/// Whether a live session other than `session` works in the worktree of `item`: only work
/// sessions do, every other role runs in a directory of its own.
fn another_work_session_live(
    live: impl IntoIterator<Item = (ItemId, SessionId, SessionRole)>,
    item: ItemId,
    session: SessionId,
) -> bool {
    live.into_iter().any(|(live_item, live_session, role)| {
        live_item == item
            && live_session != session
            && match role {
                SessionRole::Work => true,
                SessionRole::Review { .. } | SessionRole::Supervise | SessionRole::Diagnose => {
                    false
                }
            }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_another_live_work_session_keeps_the_item_worktree_in_use() {
        let item = ItemId::from_bytes([1; 32]);
        let other_item = ItemId::from_bytes([2; 32]);
        let ended = SessionId::from_bytes([3; 32]);
        let other = SessionId::from_bytes([4; 32]);
        let review = (item, other, SessionRole::Review { commit: None });
        let supervise = (item, other, SessionRole::Supervise);
        let diagnose = (item, other, SessionRole::Diagnose);
        for live in [review, supervise, diagnose] {
            assert!(!another_work_session_live([live], item, ended));
        }
        assert!(another_work_session_live(
            [review, (item, other, SessionRole::Work)],
            item,
            ended
        ));
        assert!(!another_work_session_live(
            [(item, ended, SessionRole::Work)],
            item,
            ended
        ));
        assert!(!another_work_session_live(
            [(other_item, other, SessionRole::Work)],
            item,
            ended
        ));
    }
}

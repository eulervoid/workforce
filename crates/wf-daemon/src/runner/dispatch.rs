//! The dispatcher: merges what is ready and starts agents on what they may take.

use std::sync::Arc;
use std::time::Duration;

use wf_core::id::{HashId, ItemId, WorkforceId};
use wf_core::name::AgentName;
use wf_core::review::PendingMerge;
use wf_core::session::SessionRole;

use super::merge::{Attempt, MergeRequest};
use crate::{Daemon, Loaded};

/// How often the dispatcher looks for work even when nothing changed.
const DISPATCH_TICK: Duration = Duration::from_secs(30);

/// Starts agents on the ready items of a workforce for as long as the daemon runs and the
/// storage of the workforce does: whenever the log changes, and on a tick.
pub(crate) async fn run_dispatcher(
    daemon: std::sync::Weak<Daemon>,
    workforce: WorkforceId,
    loaded: Arc<Loaded>,
) {
    let mut heads = loaded.storage.watch_heads();
    let mut failed: std::collections::BTreeSet<(ItemId, AgentName)> = Default::default();
    loop {
        let Some(strong) = daemon.upgrade() else {
            return;
        };
        dispatch_once(&strong, workforce, &loaded, &mut failed).await;
        drop(strong);
        tokio::select! {
            changed = heads.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            () = tokio::time::sleep(DISPATCH_TICK) => {}
        }
    }
}

/// Merges what is ready, then gives every hosted auto agent with a free slot a review first, then
/// a blocker to supervise, then work to diagnose, and otherwise the first item it may take. Whatever could not be started is not tried again by
/// this daemon.
async fn dispatch_once(
    daemon: &Arc<Daemon>,
    workforce: WorkforceId,
    loaded: &Arc<Loaded>,
    failed: &mut std::collections::BTreeSet<(ItemId, AgentName)>,
) {
    let Some(repository) = daemon.repository_of(workforce) else {
        return;
    };
    let device = daemon.device();
    let Ok(mut state) = daemon.snapshot(loaded).await else {
        return;
    };
    for PendingMerge { item, commit, done } in state.pending_merges() {
        if let Err(error) = daemon
            .merge_item(
                workforce,
                loaded,
                MergeRequest {
                    item,
                    commit,
                    done,
                    because: None,
                    attempt: Attempt::Retry,
                },
            )
            .await
        {
            tracing::warn!(
                "could not merge #{}: {}",
                item.short(),
                wf_core::report::report(&error)
            );
        }
    }
    for (item, done) in state.approved_without_commits() {
        let Ok(principal) = daemon.principal_of_device(&state) else {
            continue;
        };
        if let Err(error) = daemon
            .move_item(loaded, principal, item, done, None, None)
            .await
        {
            tracing::warn!(
                "could not finish #{}: {}",
                item.short(),
                wf_core::report::report(&error)
            );
        }
    }
    let Ok(fresh) = daemon.snapshot(loaded).await else {
        return;
    };
    state = fresh;

    let agents: Vec<_> = state
        .auto_agents(device)
        .into_iter()
        .map(|(agent, _)| agent)
        .collect();
    for agent in agents {
        while state.has_free_slot(device, &agent) {
            let review = state
                .reviewable(device, &agent)
                .into_iter()
                .find(|(item, _)| !failed.contains(&(*item, agent.name.clone())));
            let supervision = state
                .supervisable(&agent)
                .into_iter()
                .find(|item| !failed.contains(&(*item, agent.name.clone())));
            let diagnosis = state
                .diagnosable(device, &agent)
                .into_iter()
                .find(|item| !failed.contains(&(*item, agent.name.clone())));
            let (item, role) = match (review, supervision, diagnosis) {
                (Some((item, commit)), _, _) => (item, SessionRole::Review { commit }),
                (None, Some(item), _) => (item, SessionRole::Supervise),
                (None, None, Some(item)) => (item, SessionRole::Diagnose),
                (None, None, None) => {
                    let Some(item) = state
                        .dispatchable(device, &agent)
                        .into_iter()
                        .map(|item| item.id())
                        .find(|item| !failed.contains(&(*item, agent.name.clone())))
                    else {
                        break;
                    };
                    (item, SessionRole::Work)
                }
            };
            let started = daemon
                .start_session(
                    workforce,
                    loaded,
                    agent.name.clone(),
                    item,
                    role,
                    repository.clone(),
                )
                .await;
            if let Err(error) = started {
                failed.insert((item, agent.name.clone()));
                tracing::warn!(
                    "could not start {} on #{}: {}",
                    agent.name,
                    item.short(),
                    wf_core::report::report(&error)
                );
            }
            let Ok(fresh) = daemon.snapshot(loaded).await else {
                return;
            };
            state = fresh;
        }
    }
}

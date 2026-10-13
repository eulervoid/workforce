//! The dispatcher: merges what is ready and starts agents on what they may take.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use wf_core::id::{HashId, ItemId, WorkforceId};
use wf_core::name::AgentName;
use wf_core::review::PendingMerge;
use wf_core::session::SessionRole;

use super::merge::{Attempt, MergeEnd, MergeRequest};
use crate::notify::Notifier;
use crate::{Daemon, Loaded};

/// How often the dispatcher looks for work even when nothing changed.
const DISPATCH_TICK: Duration = Duration::from_secs(30);

/// Starts agents on the ready items of a workforce for as long as the daemon runs and the
/// storage of the workforce does: whenever the log changes, and on a tick. After each turn, tells
/// the principal of what now waits on them.
pub(crate) async fn run_dispatcher(
    daemon: std::sync::Weak<Daemon>,
    workforce: WorkforceId,
    loaded: Arc<Loaded>,
) {
    let mut heads = loaded.storage.watch_heads();
    let mut failed: std::collections::BTreeSet<(ItemId, AgentName)> = Default::default();
    let mut notifier = match daemon.upgrade() {
        Some(strong) => Notifier::load(&strong.workforce_directory(workforce)),
        None => return,
    };
    loop {
        let Some(strong) = daemon.upgrade() else {
            return;
        };
        if *strong.halted.borrow() {
            return;
        }
        let waiting_merges = dispatch_once(&strong, workforce, &loaded, &mut failed).await;
        notifier.announce(&strong, &loaded, &waiting_merges).await;
        let halted = async { strong.until_halted().await };
        tokio::select! {
            changed = heads.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            () = halted => return,
            () = tokio::time::sleep(DISPATCH_TICK) => {}
        }
    }
}

/// Merges what is ready, then gives every hosted auto agent with a free slot a review first, then
/// a blocker to supervise, then work to diagnose, and otherwise the first item it may take. Whatever could not be started is not tried again by
/// this daemon. Returns the items whose merge waits for the target branch to be checked out
/// without changes.
async fn dispatch_once(
    daemon: &Arc<Daemon>,
    workforce: WorkforceId,
    loaded: &Arc<Loaded>,
    failed: &mut std::collections::BTreeSet<(ItemId, AgentName)>,
) -> BTreeSet<ItemId> {
    let mut waiting_merges = BTreeSet::new();
    let Some(repository) = daemon.repository_of(workforce) else {
        return waiting_merges;
    };
    let device = daemon.device();
    let Ok(mut state) = daemon.snapshot(loaded).await else {
        return waiting_merges;
    };
    for PendingMerge { item, commit, done } in state.pending_merges() {
        match daemon
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
            Ok(MergeEnd::Handled) => {}
            Ok(MergeEnd::WaitsForCleanCheckout) => {
                waiting_merges.insert(item);
            }
            Err(error) => tracing::warn!(
                "could not merge #{}: {}",
                item.short(),
                wf_core::error_chain(&error)
            ),
        }
    }
    for (item, done) in state.finished_reviews() {
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
                wf_core::error_chain(&error)
            );
        }
    }
    let Ok(fresh) = daemon.snapshot(loaded).await else {
        return waiting_merges;
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
                    wf_core::error_chain(&error)
                );
            }
            let Ok(fresh) = daemon.snapshot(loaded).await else {
                return waiting_merges;
            };
            state = fresh;
        }
    }
    waiting_merges
}

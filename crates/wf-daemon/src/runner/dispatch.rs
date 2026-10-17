//! The dispatcher: queues what is ready to merge and starts agents on what they may take.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use wf_core::id::{HashId, ItemId, WorkforceId};
use wf_core::item::ItemState;
use wf_core::name::{AgentName, HarnessName};
use wf_core::review::PendingMerge;
use wf_core::session::SessionRole;

use super::merge::{Attempt, MergeRequest};
use crate::notify::Notifier;
use crate::{Daemon, Loaded};

/// How often the dispatcher looks for work even when nothing changed.
const DISPATCH_TICK: Duration = Duration::from_secs(30);

/// How long an agent is not started again on an item it could not be started on. A cause such as
/// a full disk passes, so the item is tried again after it.
const RETRY_FAILED_START: Duration = Duration::from_secs(10 * 60);

/// When each agent last failed to start on an item.
type FailedStarts = std::collections::BTreeMap<(ItemId, AgentName), Instant>;

/// Starts agents on the ready items of a workforce for as long as the daemon runs and the
/// storage of the workforce does: whenever the log changes, and on a tick. After each turn, tells
/// the principal of what now waits on them.
pub(crate) async fn run_dispatcher(
    daemon: std::sync::Weak<Daemon>,
    workforce: WorkforceId,
    loaded: Arc<Loaded>,
) {
    let mut heads = loaded.storage.watch_heads();
    let mut failed = FailedStarts::new();
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
        strong.faults.check_space(&strong.config.data_dir);
        dispatch_once(&strong, workforce, &loaded, &mut failed).await;
        notifier
            .announce(&strong, &loaded, &loaded.merges.waiting())
            .await;
        let halted = async { strong.until_halted().await };
        tokio::select! {
            changed = heads.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            () = loaded.merges.waiting_changed() => {}
            () = halted => return,
            () = tokio::time::sleep(DISPATCH_TICK) => {}
        }
    }
}

/// Queues what is ready to merge for the merge task, then gives every hosted auto agent with a
/// free slot a review first, then a blocker to supervise, then work to diagnose, and otherwise the
/// first item it may take. Whatever could not be started is tried again after
/// [`RETRY_FAILED_START`]. Nothing is started while the disk is full, and no agent of a harness
/// whose login lapsed or that cannot be reached until it is tried again.
async fn dispatch_once(
    daemon: &Arc<Daemon>,
    workforce: WorkforceId,
    loaded: &Arc<Loaded>,
    failed: &mut FailedStarts,
) {
    failed.retain(|_, at| at.elapsed() < RETRY_FAILED_START);
    let Some(repository) = daemon.repository_of(workforce) else {
        return;
    };
    let device = daemon.device();
    let Ok(mut state) = daemon.snapshot(loaded).await else {
        return;
    };
    for PendingMerge { item, commit } in state.pending_merges() {
        loaded.merges.push(MergeRequest {
            item,
            commit,
            because: None,
            attempt: Attempt::Retry,
        });
    }
    for item in state.finished_reviews() {
        let Ok(principal) = daemon.principal_of_device(&state) else {
            continue;
        };
        if let Err(error) = daemon
            .move_item(loaded, principal, item, ItemState::Done, None, None)
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
        return;
    };
    state = fresh;

    // An agent whose session would fail for what is wrong with the machine waits until it is
    // fine, or until it is time to try again whether it is.
    let now_ms = crate::now_ms();
    let auto_agents = state.auto_agents(device);
    let mut unknown: BTreeMap<HarnessName, Vec<AgentName>> = BTreeMap::new();
    for (agent, definition) in &auto_agents {
        if daemon.harness_command(&definition.harness).is_none() {
            unknown
                .entry(definition.harness.clone())
                .or_default()
                .push(agent.name.clone());
        }
    }
    daemon.faults.sync_unknown_harnesses(unknown);
    let agents: Vec<_> = auto_agents
        .into_iter()
        .filter(|(_, definition)| !daemon.faults.holds(&definition.harness, now_ms))
        .map(|(agent, _)| agent)
        .collect();
    for agent in agents {
        while state.has_free_slot(device, &agent) {
            let review = state
                .reviewable(device, &agent)
                .into_iter()
                .find(|(item, _)| !failed.contains_key(&(*item, agent.name.clone())));
            let supervision = state
                .supervisable(&agent)
                .into_iter()
                .find(|item| !failed.contains_key(&(*item, agent.name.clone())));
            let diagnosis = state
                .diagnosable(device, &agent)
                .into_iter()
                .find(|item| !failed.contains_key(&(*item, agent.name.clone())));
            let (item, role) = match (review, supervision, diagnosis) {
                (Some((item, commit)), _, _) => (item, SessionRole::Review { commit }),
                (None, Some(item), _) => (item, SessionRole::Supervise),
                (None, None, Some(item)) => (item, SessionRole::Diagnose),
                (None, None, None) => {
                    let Some(item) = state
                        .dispatchable(device, &agent)
                        .into_iter()
                        .map(|item| item.id())
                        .find(|item| !failed.contains_key(&(*item, agent.name.clone())))
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
                daemon.faults.note_space(
                    &format!("the start of {} on #{}", agent.name, item.short()),
                    &error,
                );
                failed.insert((item, agent.name.clone()), Instant::now());
                tracing::warn!(
                    "could not start {} on #{}: {}",
                    agent.name,
                    item.short(),
                    wf_core::error_chain(&error)
                );
            }
            let Ok(fresh) = daemon.snapshot(loaded).await else {
                return;
            };
            state = fresh;
        }
    }
}

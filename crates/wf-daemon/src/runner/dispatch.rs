//! The dispatcher: queues what is ready to merge and starts agents on what they may take.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use wf_core::id::{HashId, ItemId, SessionId, WorkforceId};
use wf_core::identity::AgentId;
use wf_core::identity::DeviceKey;
use wf_core::item::ItemState;
use wf_core::name::{AgentName, HarnessName};
use wf_core::review::PendingMerge;
use wf_core::session::SessionRole;
use wf_core::state::State;

use wf_vcs::GitCli;

use super::SessionError;
use super::merge::{Attempt, MergeRequest};
use super::offered::CheckedReviews;
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
    dispatch_loop(&daemon, workforce, &loaded).await;
    // Its agents are not looked at any more, so they cannot be what a harness is at fault for.
    if let Some(strong) = daemon.upgrade() {
        strong
            .faults
            .sync_unknown_harnesses(workforce, BTreeMap::new());
    }
}

async fn dispatch_loop(
    daemon: &std::sync::Weak<Daemon>,
    workforce: WorkforceId,
    loaded: &Arc<Loaded>,
) {
    let mut heads = loaded.storage.watch_heads();
    let mut failed = FailedStarts::new();
    let mut checked = CheckedReviews::new();
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
        strong.faults.check_space(strong.launch.data_dir.root());
        // Before anything starts on what a sync just brought.
        strong.end_lost_claims(workforce, loaded).await;
        strong
            .stop_sessions_ended_elsewhere(workforce, loaded)
            .await;
        dispatch_once(&strong, workforce, loaded, &mut failed, &mut checked).await;
        notifier.announce(&strong, loaded).await;
        let halted = async { strong.until_halted().await };
        tokio::select! {
            changed = heads.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            () = loaded.commits.arrived.notified() => {}
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
    checked: &mut CheckedReviews,
) {
    failed.retain(|_, at| at.elapsed() < RETRY_FAILED_START);
    let device = daemon.device();
    daemon
        .ensure_bare_repository(workforce, &loaded.storage)
        .await;
    let repository = daemon.bare_repository(workforce);
    let Ok((pending, finished, principal)) = daemon
        .read(loaded, move |state| {
            (
                state.pending_merges(),
                state.finished_reviews(),
                state.principal_of(&device),
            )
        })
        .await
    else {
        return;
    };
    // A merge starts once the commit it merges is here.
    let commits: Vec<_> = pending.iter().map(|pending| pending.commit).collect();
    let missing = match tokio::task::spawn_blocking({
        let repository = repository.clone();
        move || GitCli::open(&repository)?.missing_commits(&commits)
    })
    .await
    {
        Ok(Ok(missing)) => missing,
        Ok(Err(error)) => {
            tracing::warn!(
                "could not tell which commits to merge are here: {}",
                wf_core::error_chain(&error)
            );
            return;
        }
        Err(_) => return,
    };
    for PendingMerge { item, commit } in pending
        .into_iter()
        .filter(|pending| !missing.contains(&pending.commit))
    {
        loaded.merges.push(MergeRequest {
            item,
            commit,
            because: None,
            attempt: Attempt::Retry,
        });
    }
    // Before a reviewer is started on work that no longer merges.
    if let Err(error) = daemon
        .check_offered_reviews(loaded, &repository, checked)
        .await
    {
        tracing::warn!(
            "could not check the offered reviews: {}",
            wf_core::error_chain(&error)
        );
    }
    for item in finished {
        let Some(principal) = principal else {
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

    // An agent whose session would fail for what is wrong with the machine waits until it is
    // fine, or until it is time to try again whether it is.
    let now_ms = wf_store::now_ms();
    let Ok((auto_agents, unknown_harnesses)) = daemon
        .read(loaded, move |state| {
            (
                state
                    .auto_agents(device)
                    .into_iter()
                    .map(|(agent, definition)| (agent, definition.harness.clone()))
                    .collect::<Vec<_>>(),
                auto_agent_harnesses(state, device),
            )
        })
        .await
    else {
        return;
    };
    sync_unknown_harnesses(daemon, workforce, unknown_harnesses);
    let agents: Vec<_> = auto_agents
        .into_iter()
        .filter(|(_, harness)| !daemon.faults.holds(harness, now_ms))
        .map(|(agent, _)| agent)
        .collect();
    // Items that wait for a commit to arrive; the next pass looks again.
    let mut waiting = BTreeSet::new();
    for agent in agents {
        loop {
            let skipped: BTreeSet<ItemId> = failed
                .keys()
                .filter(|(_, name)| *name == agent.name)
                .map(|(item, _)| *item)
                .chain(waiting.iter().copied())
                .collect();
            let candidate_of = agent.clone();
            let presence = daemon.presence_now();
            let now_ms = wf_store::now_ms();
            let Ok(candidate) = daemon
                .read(loaded, move |state| {
                    let expired = state.expired(device, &presence, now_ms);
                    next_start(state, device, &candidate_of, &skipped, &expired)
                })
                .await
            else {
                return;
            };
            let Some((item, role)) = candidate else {
                break;
            };
            let started = daemon
                .start_session(workforce, loaded, agent.name.clone(), item, role)
                .await;
            if let Err(SessionError::AwaitingCommit(commit)) = &started {
                tracing::info!(
                    "#{} waits for commit {} to arrive",
                    item.short(),
                    commit.short()
                );
                waiting.insert(item);
            } else if let Err(error) = started {
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
        }
    }
}

/// What `agent` starts next on this device, unless it has no free slot or nothing to take that is
/// not in `skipped`: a review first, then a blocker to supervise, then work to diagnose, and
/// otherwise the first item it may take. What only `expired` sessions hold is free to take over.
fn next_start(
    state: &State,
    device: DeviceKey,
    agent: &AgentId,
    skipped: &BTreeSet<ItemId>,
    expired: &BTreeSet<SessionId>,
) -> Option<(ItemId, SessionRole)> {
    if !state.has_free_slot(device, agent) {
        return None;
    }
    let review = state
        .reviewable(device, agent, expired)
        .into_iter()
        .find(|(item, _, _)| !skipped.contains(item));
    let supervision = state
        .supervisable(agent, expired)
        .into_iter()
        .find(|item| !skipped.contains(item));
    let diagnosis = state
        .diagnosable(device, agent, expired)
        .into_iter()
        .find(|item| !skipped.contains(item));
    match (review, supervision, diagnosis) {
        (Some((item, commit, step)), _, _) => Some((item, SessionRole::Review { commit, step })),
        (None, Some(item), _) => Some((item, SessionRole::Supervise)),
        (None, None, Some(item)) => Some((item, SessionRole::Diagnose)),
        (None, None, None) => state
            .dispatchable(device, agent, expired)
            .into_iter()
            .map(|item| item.id())
            .find(|item| !skipped.contains(item))
            .map(|item| (item, SessionRole::Work)),
    }
}

/// The auto agents of this device with the harness each runs on.
fn auto_agent_harnesses(state: &State, device: DeviceKey) -> Vec<(HarnessName, AgentName)> {
    state
        .auto_agents(device)
        .into_iter()
        .map(|(agent, definition)| (definition.harness.clone(), agent.name))
        .collect()
}

/// Makes the harnesses that no command is known for, among the auto agents of `workforce` on
/// this device, machine faults.
fn sync_unknown_harnesses(
    daemon: &Daemon,
    workforce: WorkforceId,
    agent_harnesses: Vec<(HarnessName, AgentName)>,
) {
    let mut unknown: BTreeMap<HarnessName, Vec<AgentName>> = BTreeMap::new();
    for (harness, agent) in agent_harnesses {
        if daemon.harness_command(&harness).is_none() {
            unknown.entry(harness).or_default().push(agent);
        }
    }
    daemon.faults.sync_unknown_harnesses(workforce, unknown);
}

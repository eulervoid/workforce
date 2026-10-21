//! The dispatcher: queues what is ready to merge and starts agents on what they may take.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use wf_core::claims::Presence;
use wf_core::id::{HashId, ItemId, WorkforceId};
use wf_core::identity::AgentId;
use wf_core::identity::DeviceKey;
use wf_core::item::ItemState;
use wf_core::name::{AgentName, HarnessName};
use wf_core::placement::Placement;
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

/// When this device first saw each claimable thing, an item in a role, so that it waits as long
/// as its rank asks ([`Placement::delay`]) from then on. A thing it no longer sees, because a
/// claim arrived or it cannot take it any more, is forgotten, so its wait starts over once it is
/// claimable again.
#[derive(Debug, Default)]
struct Waits {
    since: BTreeMap<(ItemId, SessionRole), Instant>,
    seen: BTreeSet<(ItemId, SessionRole)>,
}

impl Waits {
    /// Whether this device may start `item` in `role` at `now`, being of `rank` for it, and
    /// otherwise when it may.
    fn due(&mut self, item: ItemId, role: &SessionRole, rank: usize, now: Instant) -> Due {
        let thing = (item, role.clone());
        self.seen.insert(thing.clone());
        let since = *self.since.entry(thing).or_insert(now);
        let at = since + Placement::delay(rank);
        if at <= now { Due::Now } else { Due::At(at) }
    }

    /// What an agent starts of its `candidates` (each with this device's rank), taken in the
    /// order it takes them: the first, once it is due. Until then it waits for it, rather than
    /// fill its slot with something less urgent that this device ranks first. The rest count as
    /// seen, so their waits run meanwhile.
    fn choose(&mut self, candidates: Vec<(ItemId, SessionRole, usize)>, now: Instant) -> Choice {
        let mut choice = Choice::Nothing;
        for (index, (item, role, rank)) in candidates.into_iter().enumerate() {
            let due = self.due(item, &role, rank, now);
            if index == 0 {
                choice = match due {
                    Due::Now => Choice::Start(item, role),
                    Due::At(at) => Choice::Wait(at),
                };
            }
        }
        choice
    }

    /// Forgets what was not seen since the last call.
    fn end_pass(&mut self) {
        let seen = std::mem::take(&mut self.seen);
        self.since.retain(|thing, _| seen.contains(thing));
    }
}

/// When a device may start a claimable thing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Due {
    Now,
    At(Instant),
}

/// What an agent does next ([`Waits::choose`]).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Choice {
    Start(ItemId, SessionRole),
    /// Its first candidate is due at this instant.
    Wait(Instant),
    Nothing,
}

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
    let mut waits = Waits::default();
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
        let next = dispatch_once(
            &strong,
            workforce,
            loaded,
            &mut failed,
            &mut checked,
            &mut waits,
        )
        .await;
        notifier.announce(&strong, loaded).await;
        let tick = next.map_or(DISPATCH_TICK, |at| {
            at.saturating_duration_since(Instant::now())
                .min(DISPATCH_TICK)
        });
        tracing::debug!("dispatched: waiting for a change, or {tick:?}");
        let halted = async { strong.until_halted().await };
        tokio::select! {
            changed = heads.changed() => {
                if changed.is_err() {
                    return;
                }
                tracing::debug!("dispatching: the log changed");
            }
            () = loaded.commits.arrived.notified() => {
                tracing::debug!("dispatching: commits arrived");
            }
            () = halted => return,
            () = tokio::time::sleep(tick) => {
                tracing::debug!("dispatching: {tick:?} passed");
            }
        }
    }
}

/// Queues what is ready to merge for the merge task, then gives every hosted auto agent with a
/// free slot a review first, then a blocker to supervise, then work to diagnose, and otherwise the
/// first item it may take. Whatever could not be started is tried again after
/// [`RETRY_FAILED_START`]. Nothing is started while the disk is full, and no agent of a harness
/// whose login lapsed or that cannot be reached until it is tried again. What another device
/// ranks before this one waits its turn, and the agent with it ([`Waits::choose`]); returns when
/// the first of those is due.
async fn dispatch_once(
    daemon: &Arc<Daemon>,
    workforce: WorkforceId,
    loaded: &Arc<Loaded>,
    failed: &mut FailedStarts,
    checked: &mut CheckedReviews,
    waits: &mut Waits,
) -> Option<Instant> {
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
        return None;
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
            return None;
        }
        Err(_) => return None,
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
        return None;
    };
    sync_unknown_harnesses(daemon, workforce, unknown_harnesses);
    let agents: Vec<_> = auto_agents
        .into_iter()
        .filter(|(_, harness)| !daemon.faults.holds(harness, now_ms))
        .map(|(agent, _)| agent)
        .collect();
    // Items that wait for a commit to arrive; the next pass looks again.
    let mut waiting = BTreeSet::new();
    // When the first thing that waits for its rank is due.
    let mut next = None;
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
            let Ok(candidates) = daemon
                .read(loaded, move |state| {
                    ranked_starts(state, device, &candidate_of, &skipped, &presence, now_ms)
                })
                .await
            else {
                return None;
            };
            let (item, role) = match waits.choose(candidates, Instant::now()) {
                Choice::Start(item, role) => (item, role),
                Choice::Wait(at) => {
                    next = Some(next.map_or(at, |next: Instant| next.min(at)));
                    break;
                }
                Choice::Nothing => break,
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
    waits.end_pass();
    next
}

/// What `agent` could start on this device, in the order it takes them, leaving out what is in
/// `skipped`, each with the rank of this device for it ([`State::placement`]): nothing when it has
/// no free slot; a review first, then a blocker to supervise, then work to diagnose, then the
/// items it may take. What only sessions whose lease expired hold is free to take over.
fn ranked_starts(
    state: &State,
    device: DeviceKey,
    agent: &AgentId,
    skipped: &BTreeSet<ItemId>,
    presence: &Presence,
    now_ms: u64,
) -> Vec<(ItemId, SessionRole, usize)> {
    let expired = state.expired(device, presence, now_ms);
    let startable: Vec<_> = state
        .startable(device, agent, &expired)
        .into_iter()
        .filter(|(item, _)| !skipped.contains(item))
        .collect();
    if startable.is_empty() {
        return Vec::new();
    }
    let placement = state.placement(device, presence, now_ms);
    startable
        .into_iter()
        .map(|(item, role)| {
            let rank = placement.rank(item, &role, device);
            (item, role, rank)
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use wf_core::id::EventId;
    use wf_core::placement::RANK_DELAY;

    use super::*;

    fn item(seed: u8) -> ItemId {
        EventId::of(&[seed]).into()
    }

    #[test]
    fn rank_0_starts_at_once_and_rank_r_waits_r_delays_from_when_it_was_first_seen() {
        let mut waits = Waits::default();
        let seen = Instant::now();
        assert_eq!(waits.due(item(1), &SessionRole::Work, 0, seen), Due::Now);
        assert_eq!(
            waits.due(item(2), &SessionRole::Work, 2, seen),
            Due::At(seen + RANK_DELAY * 2)
        );
        waits.end_pass();

        // Seen again later, it still counts from the first time.
        let later = seen + RANK_DELAY;
        assert_eq!(
            waits.due(item(2), &SessionRole::Work, 2, later),
            Due::At(seen + RANK_DELAY * 2)
        );
        waits.end_pass();
        assert_eq!(
            waits.due(item(2), &SessionRole::Work, 2, seen + RANK_DELAY * 2),
            Due::Now
        );
    }

    #[test]
    fn a_thing_not_seen_in_a_pass_waits_again_from_when_it_is_seen_next() {
        let mut waits = Waits::default();
        let seen = Instant::now();
        let review = SessionRole::Review {
            commit: None,
            step: None,
        };
        waits.due(item(1), &review, 1, seen);
        waits.due(item(1), &SessionRole::Work, 1, seen);
        waits.end_pass();
        // A claim arrived for the review, so it is not seen in this pass.
        waits.due(item(1), &SessionRole::Work, 1, seen);
        waits.end_pass();

        let again = seen + RANK_DELAY * 3;
        assert_eq!(
            waits.due(item(1), &review, 1, again),
            Due::At(again + RANK_DELAY)
        );
        assert_eq!(waits.due(item(1), &SessionRole::Work, 1, again), Due::Now);
    }

    #[test]
    fn an_agent_waits_for_its_most_urgent_candidate_rather_than_start_a_later_one() {
        let mut waits = Waits::default();
        let seen = Instant::now();
        let (urgent, other) = (item(1), item(2));
        let both = || {
            vec![
                (urgent, SessionRole::Work, 1),
                (other, SessionRole::Work, 0),
            ]
        };
        let wait = Choice::Wait(seen + RANK_DELAY);
        assert_eq!(waits.choose(both(), seen), wait);
        waits.end_pass();
        assert_eq!(waits.choose(both(), seen + RANK_DELAY / 2), wait);
        waits.end_pass();
        assert_eq!(
            waits.choose(both(), seen + RANK_DELAY),
            Choice::Start(urgent, SessionRole::Work)
        );

        // Claimed by the device ahead, the urgent one is no candidate any more.
        let mut waits = Waits::default();
        assert_eq!(waits.choose(both(), seen), wait);
        waits.end_pass();
        assert_eq!(
            waits.choose(vec![(other, SessionRole::Work, 0)], seen + RANK_DELAY / 2),
            Choice::Start(other, SessionRole::Work)
        );
        assert_eq!(waits.choose(Vec::new(), seen), Choice::Nothing);
    }
}

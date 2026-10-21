//! Keeping in touch with the other devices: one-shot rounds of syncing, and the loop that
//! repeats them while the daemon runs.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tokio::task::{AbortHandle, JoinSet};
use tracing::instrument::WithSubscriber;
use wf_core::claims::Presence;
use wf_core::id::WorkforceId;
use wf_core::identity::DeviceKey;
use wf_core::state::State;
use wf_net::{HeadsTopic, Heard, NetError, Node, SyncLog, SyncReport, heads_digest};

use crate::StorageHandle;

/// Even without news, ask the other devices how they are doing this often.
const PERIODIC_ROUND: Duration = Duration::from_secs(60);
/// Writes that follow each other closely are sent in one round.
const SETTLE: Duration = Duration::from_millis(300);
const FIRST_RETRY: Duration = Duration::from_secs(5);
const RETRY_ON_NEWS_AFTER: Duration = Duration::from_secs(1);
const LONGEST_RETRY: Duration = Duration::from_secs(300);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerSync {
    pub device: DeviceKey,
    pub label: String,
    pub outcome: PeerOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PeerOutcome {
    Synced {
        sent: usize,
        received: usize,
        rejected: usize,
    },
    Unreachable(String),
}

impl PeerSync {
    fn from_result(device: DeviceKey, label: String, result: Result<SyncReport, NetError>) -> Self {
        let outcome = match result {
            Ok(report) => PeerOutcome::Synced {
                sent: report.sent,
                received: report.applied,
                rejected: report.rejected,
            },
            Err(error) => PeerOutcome::Unreachable(wf_core::error_chain(&error)),
        };
        Self {
            device,
            label,
            outcome,
        }
    }

    pub fn reached(&self) -> bool {
        matches!(self.outcome, PeerOutcome::Synced { .. })
    }
}

/// The other bound devices of the workforce.
pub fn peers_of(state: &State, own: DeviceKey) -> Vec<DeviceKey> {
    state
        .devices()
        .map(|info| info.device)
        .filter(|device| *device != own)
        .collect()
}

/// Whether the workforce needs the network: another device is bound, or one may join with an
/// open invite.
pub fn needs_network(state: &State, own: DeviceKey, now_ms: u64) -> bool {
    !peers_of(state, own).is_empty() || state.has_open_invite(now_ms)
}

/// Notes in `presence` that `device` was heard from just now. What a daemon heard is local to its
/// machine, and not part of the workforce state.
fn saw(presence: &Mutex<Presence>, device: DeviceKey) {
    presence
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .saw(device, wf_store::now_ms());
}

/// Syncs `workforce` with each of `peers` at once. `peers` pairs a device with how to call it.
pub async fn sync_all<L>(
    node: &Arc<Node<L>>,
    workforce: WorkforceId,
    peers: Vec<(DeviceKey, String)>,
    presence: Option<&Mutex<Presence>>,
) -> Vec<PeerSync>
where
    L: SyncLog + Clone + 'static,
{
    let mut tasks = JoinSet::new();
    for (index, (device, label)) in peers.into_iter().enumerate() {
        let node = Arc::clone(node);
        tasks.spawn(
            async move {
                let result = node.sync_with(workforce, wf_net::endpoint_id(device)).await;
                (index, PeerSync::from_result(device, label, result))
            }
            .with_current_subscriber(),
        );
    }
    let mut results = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        if let Ok((index, result)) = joined {
            if result.reached()
                && let Some(presence) = presence
            {
                saw(presence, result.device);
            }
            results.push((index, result));
        }
    }
    results.sort_by_key(|(index, _)| *index);
    results.into_iter().map(|(_, result)| result).collect()
}

/// When to try an unreachable device again: later and later, up to a few minutes.
#[derive(Default)]
pub(crate) struct Backoff {
    failures: u32,
    retry_at: Option<Instant>,
}

impl Backoff {
    pub(crate) fn record(&mut self, reached: bool, now: Instant) {
        if reached {
            *self = Self::default();
            return;
        }
        self.failures = self.failures.saturating_add(1);
        let delay = FIRST_RETRY
            .saturating_mul(1u32 << self.failures.min(10).saturating_sub(1))
            .min(LONGEST_RETRY);
        self.retry_at = Some(now + delay);
    }

    /// Leaves it alone for the longest time there is: for what failed everywhere.
    pub(crate) fn wait_longest(&mut self, now: Instant) {
        self.retry_at = Some(now + LONGEST_RETRY);
    }

    /// How long to wait before the next attempt: until the retry time of a device that failed,
    /// and a periodic round otherwise.
    pub(crate) fn wait(&self, now: Instant) -> Duration {
        self.retry_at
            .map_or(PERIODIC_ROUND, |at| at.saturating_duration_since(now))
    }

    /// Whether it is time to try again.
    pub(crate) fn is_due(&self, now: Instant) -> bool {
        self.retry_at.is_none_or(|at| at <= now)
    }
}

/// Keeps one device in sync: right away, whenever the heads here change and a minute after the
/// last time. A device that cannot be reached is left alone for longer each time, unless it
/// announces itself over gossip.
async fn keep_in_sync(
    node: Arc<Node<StorageHandle>>,
    workforce: WorkforceId,
    storage: StorageHandle,
    device: DeviceKey,
    presence: Arc<Mutex<Presence>>,
    kick: Arc<Notify>,
) {
    let mut heads = storage.watch_heads();
    let mut backoff = Backoff::default();
    loop {
        let attempted = Instant::now();
        let label = peer_label_of(&storage, device).await;
        let results = sync_all(&node, workforce, vec![(device, label)], Some(&presence)).await;
        let reached = results.iter().all(PeerSync::reached);
        backoff.record(reached, Instant::now());
        loop {
            tokio::select! {
                _ = kick.notified() => {
                    backoff = Backoff::default();
                    break;
                }
                changed = heads.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    tokio::time::sleep(SETTLE).await;
                    heads.borrow_and_update();
                    // A device that answered gets another round at once, since what changed may
                    // be what it lacks. One that was out of reach a moment ago is left alone
                    // until that was not just now.
                    if reached || Instant::now() >= attempted + RETRY_ON_NEWS_AFTER {
                        break;
                    }
                }
                _ = tokio::time::sleep(backoff.wait(Instant::now())) => break,
            }
        }
    }
}

/// Keeps a workforce in touch with its other devices: one task per device, gossip to announce
/// our heads and to hear theirs.
pub async fn run_peer_loop(
    node: Arc<Node<StorageHandle>>,
    workforce: WorkforceId,
    storage: StorageHandle,
    presence: Arc<Mutex<Presence>>,
) {
    let mut heads = storage.watch_heads();
    let own = node.device();
    let mut syncers: BTreeMap<DeviceKey, (Arc<Notify>, AbortHandle)> = BTreeMap::new();
    let mut tasks = JoinSet::new();
    let mut topic: Option<HeadsTopic> = None;
    loop {
        let Ok(devices) = storage
            .with(move |replica| peers_of(replica.state(), own))
            .await
        else {
            return;
        };
        syncers.retain(|device, (_, task)| {
            let still_bound = devices.contains(device);
            if !still_bound {
                task.abort();
            }
            still_bound
        });
        for device in &devices {
            syncers.entry(*device).or_insert_with(|| {
                let kick = Arc::new(Notify::new());
                let task = tasks.spawn(
                    keep_in_sync(
                        Arc::clone(&node),
                        workforce,
                        storage.clone(),
                        *device,
                        Arc::clone(&presence),
                        Arc::clone(&kick),
                    )
                    .with_current_subscriber(),
                );
                (kick, task)
            });
        }
        match &topic {
            Some(topic) => {
                let _ = topic.join(&devices).await;
            }
            None => topic = node.heads_topic(&workforce, &devices).await.ok(),
        }
        let current = heads.borrow().clone();
        if let Some(topic) = &topic {
            let _ = topic.announce(&current).await;
        }

        loop {
            tokio::select! {
                changed = heads.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    tokio::time::sleep(SETTLE).await;
                    heads.borrow_and_update();
                    break;
                }
                _ = tokio::time::sleep(PERIODIC_ROUND) => break,
                heard = next_heard(&mut topic) => {
                    let Some(heard) = heard else {
                        topic = None;
                        break;
                    };
                    let Some((kick, _)) = syncers.get(&heard.device) else {
                        continue;
                    };
                    saw(&presence, heard.device);
                    if heard.digest != heads_digest(&heads.borrow()) {
                        kick.notify_one();
                    }
                }
            }
        }
    }
}

async fn next_heard(topic: &mut Option<HeadsTopic>) -> Option<Heard> {
    match topic {
        Some(topic) => topic.heard.recv().await,
        None => std::future::pending().await,
    }
}

async fn peer_label_of(storage: &StorageHandle, device: DeviceKey) -> String {
    storage
        .with(move |replica| peer_label(replica.state(), device))
        .await
        .unwrap_or_else(|_| device.fmt_short())
}

/// What `device` is called: its profile's name, or its short key.
pub(crate) fn peer_label(state: &State, device: DeviceKey) -> String {
    let name = state
        .devices()
        .find(|info| info.device == device)
        .and_then(|info| info.profile.map(|profile| profile.name.clone()));
    name.unwrap_or_else(|| device.fmt_short())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waiting_is_a_periodic_round_until_a_failure_sets_a_retry_time() {
        let now = Instant::now();
        let mut backoff = Backoff::default();
        assert_eq!(backoff.wait(now), PERIODIC_ROUND);
        backoff.record(false, now);
        assert_eq!(backoff.wait(now), FIRST_RETRY);
        assert_eq!(backoff.wait(now + FIRST_RETRY * 2), Duration::ZERO);
    }

    #[test]
    fn unreachable_devices_are_retried_later_each_time() {
        let now = Instant::now();
        let mut backoff = Backoff::default();
        backoff.record(false, now);
        let first = backoff.wait(now);
        backoff.record(false, now);
        let second = backoff.wait(now);
        assert_eq!(first, FIRST_RETRY);
        assert_eq!(second, FIRST_RETRY * 2);
        for _ in 0..20 {
            backoff.record(false, now);
        }
        assert_eq!(backoff.wait(now), LONGEST_RETRY);
        backoff.record(true, now);
        assert_eq!(backoff.wait(now), PERIODIC_ROUND);
    }
}

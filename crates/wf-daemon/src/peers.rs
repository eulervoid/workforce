//! Keeping in touch with the other devices: one-shot rounds of syncing, and the loop that
//! repeats them while the daemon runs.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tokio::task::JoinSet;
use wf_core::id::WorkforceId;
use wf_core::identity::DeviceKey;
use wf_core::state::State;
use wf_net::{NetError, Node, SyncLog, SyncReport};

use crate::storage::StorageHandle;

/// Even without news, ask the other devices how they are doing this often.
const PERIODIC_ROUND: Duration = Duration::from_secs(60);
/// Writes that follow each other closely are sent in one round.
const SETTLE: Duration = Duration::from_millis(300);
const FIRST_RETRY: Duration = Duration::from_secs(5);
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
            Err(error) => PeerOutcome::Unreachable(error.to_string()),
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

/// When each device was last heard from, in milliseconds since the epoch. Local to this
/// machine, and not part of the workforce state.
#[derive(Clone, Default)]
pub struct Presence(Arc<Mutex<BTreeMap<DeviceKey, u64>>>);

impl Presence {
    pub fn saw(&self, device: DeviceKey) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| {
                u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
            });
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(device, now);
    }

    pub fn snapshot(&self) -> Vec<(DeviceKey, u64)> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .map(|(device, seen)| (*device, *seen))
            .collect()
    }
}

/// Syncs `workforce` with each of `peers` at once. `peers` pairs a device with how to call it.
pub async fn sync_all<L>(
    node: &Arc<Node<L>>,
    workforce: WorkforceId,
    peers: Vec<(DeviceKey, String)>,
    presence: Option<&Presence>,
) -> Vec<PeerSync>
where
    L: SyncLog + Clone + 'static,
{
    let mut tasks = JoinSet::new();
    for (index, (device, label)) in peers.into_iter().enumerate() {
        let node = Arc::clone(node);
        tasks.spawn(async move {
            let result = node.sync_with(workforce, wf_net::endpoint_id(device)).await;
            (index, PeerSync::from_result(device, label, result))
        });
    }
    let mut results = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        if let Ok((index, result)) = joined {
            if result.reached()
                && let Some(presence) = presence
            {
                presence.saw(result.device);
            }
            results.push((index, result));
        }
    }
    results.sort_by_key(|(index, _)| *index);
    results.into_iter().map(|(_, result)| result).collect()
}

/// When to try an unreachable device again: later and later, up to a few minutes.
#[derive(Default)]
struct Backoff {
    failures: u32,
    retry_at: Option<Instant>,
}

impl Backoff {
    fn ready(&self, now: Instant) -> bool {
        self.retry_at.is_none_or(|at| now >= at)
    }

    fn record(&mut self, reached: bool, now: Instant) {
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
}

/// Syncs a workforce with its other devices when something changes and now and then in between,
/// leaving unreachable devices alone for longer each time they fail.
pub async fn run_peer_loop(
    node: Arc<Node<StorageHandle>>,
    workforce: WorkforceId,
    storage: StorageHandle,
    presence: Presence,
    nudge: Arc<Notify>,
) {
    let mut heads = storage.watch_heads();
    let own = node.device();
    let mut backoff: BTreeMap<DeviceKey, Backoff> = BTreeMap::new();
    loop {
        let peers = match storage
            .with(move |replica| {
                let state = replica.state();
                peers_of(state, own)
                    .into_iter()
                    .map(|device| (device, peer_label(state, device)))
                    .collect::<Vec<_>>()
            })
            .await
        {
            Ok(peers) => peers,
            Err(_) => return,
        };
        let now = Instant::now();
        let due: Vec<_> = peers
            .into_iter()
            .filter(|(device, _)| backoff.entry(*device).or_default().ready(now))
            .collect();
        for result in sync_all(&node, workforce, due, Some(&presence)).await {
            backoff
                .entry(result.device)
                .or_default()
                .record(result.reached(), Instant::now());
        }
        tokio::select! {
            changed = heads.changed() => {
                if changed.is_err() {
                    return;
                }
                tokio::time::sleep(SETTLE).await;
                heads.borrow_and_update();
            }
            _ = nudge.notified() => {}
            _ = tokio::time::sleep(PERIODIC_ROUND) => {}
        }
    }
}

fn peer_label(state: &State, device: DeviceKey) -> String {
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
    fn unreachable_devices_are_retried_later_each_time() {
        let now = Instant::now();
        let mut backoff = Backoff::default();
        assert!(backoff.ready(now));
        backoff.record(false, now);
        assert!(!backoff.ready(now));
        assert!(backoff.ready(now + FIRST_RETRY));
        backoff.record(false, now);
        assert!(!backoff.ready(now + FIRST_RETRY));
        assert!(backoff.ready(now + FIRST_RETRY * 2));
        for _ in 0..20 {
            backoff.record(false, now);
        }
        assert!(backoff.ready(now + LONGEST_RETRY));
        backoff.record(true, now);
        assert!(backoff.ready(now));
    }
}

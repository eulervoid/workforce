//! How the workforce is doing: counts over items and sessions, at a time given by the caller.

use serde::Serialize;

use crate::id::ItemId;
use crate::identity::AgentId;
use crate::item::{Item, ItemState};
use crate::policy::Policy;
use crate::session::{Outcome, Session, SessionRole};
use crate::state::State;

/// The span "lately" covers: the last seven days.
pub const RECENT_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// How the workforce is doing as a whole.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProjectStats {
    /// Items that reached a done state in the last seven days.
    pub done_lately: usize,
    /// For those items, the median time from their first work session to done, in milliseconds.
    pub median_lead_ms: Option<u64>,
    /// Work sessions that ended as attempts: all but the interrupted and the cancelled.
    pub attempts: usize,
    /// Of those, the ones that did not submit: released, timed out or failed.
    pub failed_attempts: usize,
    /// Items whose work was merged.
    pub merged: usize,
    /// The change requests written on those items.
    pub change_requests: usize,
}

/// How an agent is doing, over all its sessions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct AgentStats {
    pub sessions: usize,
    pub live: usize,
    /// Sessions that ended doing what they were started for: submitted, reviewed, supervised or
    /// diagnosed.
    pub finished: usize,
    /// Sessions that released, timed out or failed.
    pub failed: usize,
    /// The median time an ended session ran, in milliseconds.
    pub median_duration_ms: Option<u64>,
}

/// How long the work on an item took, and how many rounds it went through.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ItemStats {
    /// Its sessions of every role, live ones included.
    pub sessions: usize,
    /// The work sessions that submitted: each starts a round of review.
    pub submissions: usize,
    pub change_requests: usize,
    /// From the start of its first work session to done, in milliseconds; `None` until it is
    /// done.
    pub lead_ms: Option<u64>,
}

fn median(mut values: Vec<u64>) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    Some(values[values.len() / 2])
}

impl State {
    /// How the workforce is doing at `now_ms`, archived items included.
    pub fn project_stats(&self, now_ms: u64) -> ProjectStats {
        let mut stats = ProjectStats::default();
        let Some(policy) = self.policy().map(|document| &document.policy) else {
            return stats;
        };
        let mut leads = Vec::new();
        for item in self.items_including_archived() {
            let done_at = item.state.stamp().hlc.wall_ms();
            let done = item.state(policy) == ItemState::Done;
            if done && now_ms.saturating_sub(done_at) <= RECENT_MS {
                stats.done_lately += 1;
                leads.extend(self.lead_ms(item, policy));
            }
            if !item.merges().is_empty() {
                stats.merged += 1;
                stats.change_requests += item
                    .comments()
                    .values()
                    .filter(|comment| comment.kind.is_change_request())
                    .count();
            }
        }
        stats.median_lead_ms = median(leads);
        for session in self.sessions() {
            let Some(ended) = session.ended() else {
                continue;
            };
            if session.role() != SessionRole::Work {
                continue;
            }
            match ended.reason.outcome() {
                Outcome::Submitted | Outcome::Finished => stats.attempts += 1,
                Outcome::Failed => {
                    stats.attempts += 1;
                    stats.failed_attempts += 1;
                }
                Outcome::Neither => {}
            }
        }
        stats
    }

    /// How the work on `item` went, archived or not. `None` for an item that does not exist.
    pub fn item_stats(&self, item: ItemId) -> Option<ItemStats> {
        let policy = &self.policy()?.policy;
        let item = self.workforce.as_ref()?.items.get(&item)?;
        let mut stats = ItemStats {
            lead_ms: self.lead_ms(item, policy),
            change_requests: item
                .comments()
                .values()
                .filter(|comment| comment.kind.is_change_request())
                .count(),
            ..ItemStats::default()
        };
        for session in self.sessions_of(item.id()) {
            stats.sessions += 1;
            if let Some(ended) = session.ended()
                && matches!(ended.reason.outcome(), Outcome::Submitted)
            {
                stats.submissions += 1;
            }
        }
        Some(stats)
    }

    /// From the start of the first work session of `item` to its move to done; `None` unless it
    /// is done and was worked on.
    fn lead_ms(&self, item: &Item, policy: &Policy) -> Option<u64> {
        if item.state(policy) != ItemState::Done {
            return None;
        }
        let first_work = self
            .sessions_of(item.id())
            .filter(|session| session.role() == SessionRole::Work)
            .map(|session| session.started().hlc.wall_ms())
            .min()?;
        Some(item.state.stamp().hlc.wall_ms().saturating_sub(first_work))
    }

    /// How `agent` is doing, over all its sessions.
    pub fn agent_stats(&self, agent: &AgentId) -> AgentStats {
        let mut stats = AgentStats::default();
        let mut durations = Vec::new();
        for session in self.sessions().filter(|session| session.agent() == agent) {
            stats.sessions += 1;
            let Some(ended) = session.ended() else {
                stats.live += 1;
                continue;
            };
            match ended.reason.outcome() {
                Outcome::Submitted | Outcome::Finished => stats.finished += 1,
                Outcome::Failed => stats.failed += 1,
                Outcome::Neither => {}
            }
            durations.push(duration_ms(session));
        }
        stats.median_duration_ms = median(durations);
        stats
    }
}

fn duration_ms(session: &Session) -> u64 {
    let ended = session
        .ended()
        .map_or(session.started().hlc, |ended| ended.at());
    ended
        .wall_ms()
        .saturating_sub(session.started().hlc.wall_ms())
}

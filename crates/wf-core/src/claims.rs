//! Claims: what a session alone may work on. A session claims its item in its role: a work
//! session the item's work, a review session its step of the submitted commit, a supervise
//! session the item's open blockers, a diagnose session the item's diagnosis. Two sessions with
//! the same item and the same role (for a review: the same commit and step) claim the same thing,
//! and of their live claims the earliest by `(hlc, EventId)` holds it. Pure functions of the
//! state: nothing about a claim is stored.
//!
//! A claim lasts while its session is live, and a session is live until its `SessionEnded`. A
//! session that dies on a running device is ended by that device's daemon, so the only death the
//! others must notice is the device's: its lease, read from the [`Presence`] of devices. A daemon
//! of the principal takes over the claims of a gone device by ending its sessions `LeaseExpired`
//! ([`State::claimable`]).

use std::collections::BTreeMap;
use std::time::Duration;

use crate::id::{HashId, ItemId, SessionId};
use crate::identity::DeviceKey;
use crate::policy::Policy;
use crate::session::{Session, SessionRole};
use crate::state::State;

/// A session cannot start on what a live session holds.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("#{} already has a live {} session, {holder}; stop it first", .item.short(), .role.name())]
pub struct ItemClaimed {
    pub item: ItemId,
    pub role: SessionRole,
    /// The session that holds the item, as [`State::actor_label`] names it.
    pub holder: String,
}

/// What one daemon knows of which devices are still there: when it last heard from each and when it
/// started. A device not heard from for the policy's `[claims] lease` is gone, counted by this
/// daemon's clock and only once the daemon has run that long, since before that it could not have
/// heard from anyone. Computed when asked, never stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Presence {
    /// When the daemon started, in milliseconds since the epoch.
    started_ms: u64,
    /// When each device was last heard from, in milliseconds since the epoch.
    heard_ms: BTreeMap<DeviceKey, u64>,
}

impl Presence {
    pub fn new(started_ms: u64, heard_ms: impl IntoIterator<Item = (DeviceKey, u64)>) -> Self {
        Self {
            started_ms,
            heard_ms: heard_ms.into_iter().collect(),
        }
    }

    /// Whether `device` has been silent for longer than `lease` at `now_ms`.
    pub fn gone(&self, device: DeviceKey, lease: Duration, now_ms: u64) -> bool {
        let heard = self.heard_ms.get(&device).copied().unwrap_or_default();
        let silent_since = heard.max(self.started_ms);
        u128::from(now_ms.saturating_sub(silent_since)) > lease.as_millis()
    }
}

impl State {
    /// The session that holds the claim on `item` in `role`: of the live sessions that claim it,
    /// the earliest by `(hlc, EventId)`. The others have lost it and are meant to end
    /// `Outclaimed` ([`State::outclaimed`]).
    pub fn holder(&self, item: ItemId, role: &SessionRole) -> Option<&Session> {
        self.sessions_of(item)
            .filter(|session| session.is_live() && session.role == *role)
            .min_by_key(|session| session.started())
    }

    /// Refuses a session on `item` in `role` while a live one holds it.
    pub fn ensure_unclaimed(&self, item: ItemId, role: &SessionRole) -> Result<(), ItemClaimed> {
        match self.holder(item, role) {
            Some(holder) => Err(ItemClaimed {
                item,
                role: role.clone(),
                holder: self.actor_label(&holder.actor()),
            }),
            None => Ok(()),
        }
    }

    /// The live sessions on `device` whose claim an earlier session holds, such as after two
    /// devices claimed the same item while they could not reach each other. Its daemon ends them
    /// `Outclaimed`.
    pub fn outclaimed(&self, device: DeviceKey) -> Vec<SessionId> {
        self.live_sessions()
            .filter(|session| session.device() == device)
            .filter(|session| {
                self.holder(session.item(), &session.role)
                    .is_some_and(|holder| holder.id() != session.id())
            })
            .map(Session::id)
            .collect()
    }

    /// Whether a daemon on `device` may start a session on `item` in `role` at `now_ms`, and which
    /// sessions it must end `LeaseExpired` first to take their claim over: the live ones that claim
    /// it on other devices of its principal that are gone. Refused while any other live session
    /// claims it, since its device is there or belongs to another principal, whose daemons alone
    /// may end it.
    pub fn claimable(
        &self,
        item: ItemId,
        role: &SessionRole,
        device: DeviceKey,
        presence: &Presence,
        now_ms: u64,
    ) -> Result<Vec<SessionId>, ItemClaimed> {
        let principal = self.principal_of(&device);
        let lease = self.policy().map_or_else(
            || Policy::default_policy().claims.lease,
            |document| document.policy.claims.lease,
        );
        let mut claimants: Vec<&Session> = self
            .sessions_of(item)
            .filter(|session| session.is_live() && session.role == *role)
            .collect();
        claimants.sort_by_key(|session| session.started());
        let mut takeover = Vec::new();
        for claimant in claimants {
            let expired = claimant.device() != device
                && principal.is_some()
                && self.principal_of(&claimant.device()) == principal
                && presence.gone(claimant.device(), lease, now_ms);
            if !expired {
                return Err(ItemClaimed {
                    item,
                    role: role.clone(),
                    holder: self.actor_label(&claimant.actor()),
                });
            }
            takeover.push(claimant.id());
        }
        Ok(takeover)
    }
}

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

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crate::id::{HashId, ItemId, SessionId};
use crate::identity::DeviceKey;
use crate::policy::Policy;
use crate::session::{EndReason, Session, SessionRole};
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
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

    /// When the daemon started, in milliseconds since the epoch.
    pub fn started_ms(&self) -> u64 {
        self.started_ms
    }

    /// Notes that `device` was heard from at `at_ms`.
    pub fn saw(&mut self, device: DeviceKey, at_ms: u64) {
        self.heard_ms.insert(device, at_ms);
    }

    /// When each device was last heard from, in milliseconds since the epoch.
    pub fn heard_ms(&self) -> impl Iterator<Item = (DeviceKey, u64)> + '_ {
        self.heard_ms.iter().map(|(device, at)| (*device, *at))
    }

    /// Since when `device` has been silent as far as this daemon knows: since it was last heard
    /// from, or since the daemon started when it was not heard from since.
    pub fn silent_since_ms(&self, device: DeviceKey) -> u64 {
        let heard = self.heard_ms.get(&device).copied().unwrap_or_default();
        heard.max(self.started_ms)
    }

    /// Whether `device` has been silent for longer than `lease` at `now_ms`.
    pub fn gone(&self, device: DeviceKey, lease: Duration, now_ms: u64) -> bool {
        u128::from(now_ms.saturating_sub(self.silent_since_ms(device))) > lease.as_millis()
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

    /// For a session that lost its claim, the session that has it instead. For one ended
    /// `LeaseExpired`, the first of the same item and role that started after the end: the
    /// takeover's. For one ended `Outclaimed`, the earliest of them that started before it and had
    /// not ended by then: the holder. `None` for a live session, for the other ends, and while the
    /// session that has it has not arrived.
    pub fn taken_over_by(&self, session: &Session) -> Option<&Session> {
        let ended = session.ended()?;
        let rivals = self
            .sessions_of(session.item())
            .filter(|other| other.role == session.role && other.id() != session.id());
        match ended.reason {
            EndReason::LeaseExpired => rivals
                .filter(|other| other.started() > ended.stamp)
                .min_by_key(|other| other.started()),
            EndReason::Outclaimed => rivals
                .filter(|other| {
                    other.started() < session.started()
                        && other
                            .ended()
                            .is_none_or(|end| end.stamp > session.started())
                })
                .min_by_key(|other| other.started()),
            EndReason::Submitted
            | EndReason::Reviewed
            | EndReason::Supervised
            | EndReason::Released
            | EndReason::TimedOut
            | EndReason::Failed
            | EndReason::Cancelled
            | EndReason::Interrupted
            | EndReason::Diagnosed
            | EndReason::MachineFault(_) => None,
        }
    }

    /// The policy's `[claims] lease`.
    pub fn lease(&self) -> Duration {
        self.policy().map_or_else(
            || Policy::default_policy().claims.lease,
            |document| document.policy.claims.lease,
        )
    }

    /// The live sessions whose claims a daemon on `device` may take over at `now_ms`: those on
    /// other devices of its principal that are gone. A session of another principal never
    /// expires here, since only that principal's daemons may end it.
    pub fn expired(
        &self,
        device: DeviceKey,
        presence: &Presence,
        now_ms: u64,
    ) -> BTreeSet<SessionId> {
        let lease = self.lease();
        self.live_sessions()
            .filter(|session| {
                self.may_take_over(device, session)
                    && presence.gone(session.device(), lease, now_ms)
            })
            .map(Session::id)
            .collect()
    }

    /// Whether a daemon on `device` takes over `session` once its device is gone: whether it runs
    /// on another device of the same principal.
    fn may_take_over(&self, device: DeviceKey, session: &Session) -> bool {
        session.device() != device
            && self
                .principal_of(&device)
                .is_some_and(|principal| self.principal_of(&session.device()) == Some(principal))
    }

    /// Refuses a session on `item` in `role` from a daemon on `device` while a live session holds
    /// it that no takeover could end: one on `device` itself, or one of another principal. Whether
    /// the devices of the others are gone only the daemon knows ([`State::claimable`]).
    pub fn ensure_takeable(
        &self,
        item: ItemId,
        role: &SessionRole,
        device: DeviceKey,
    ) -> Result<(), ItemClaimed> {
        let kept = self
            .sessions_of(item)
            .filter(|session| session.is_live() && session.role == *role)
            .filter(|session| !self.may_take_over(device, session))
            .min_by_key(|session| session.started());
        match kept {
            Some(holder) => Err(ItemClaimed {
                item,
                role: role.clone(),
                holder: self.actor_label(&holder.actor()),
            }),
            None => Ok(()),
        }
    }

    /// Whether a live session holds the claim on `item` in `role`, leaving out the `expired` ones
    /// ([`State::expired`]), whose claims the asking daemon would take over.
    pub fn held(&self, item: ItemId, role: &SessionRole, expired: &BTreeSet<SessionId>) -> bool {
        self.sessions_of(item).any(|session| {
            session.is_live() && session.role == *role && !expired.contains(&session.id())
        })
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
        let expired = self.expired(device, presence, now_ms);
        let mut claimants: Vec<&Session> = self
            .sessions_of(item)
            .filter(|session| session.is_live() && session.role == *role)
            .collect();
        claimants.sort_by_key(|session| session.started());
        if let Some(holder) = claimants
            .iter()
            .find(|claimant| !expired.contains(&claimant.id()))
        {
            return Err(ItemClaimed {
                item,
                role: role.clone(),
                holder: self.actor_label(&holder.actor()),
            });
        }
        Ok(claimants.into_iter().map(Session::id).collect())
    }
}

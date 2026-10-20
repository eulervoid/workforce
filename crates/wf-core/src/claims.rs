//! Claims: what a session alone may work on. A session claims its item in its role: a work
//! session the item's work, a review session its step of the submitted commit, a supervise
//! session the item's open blockers, a diagnose session the item's diagnosis. Two sessions with
//! the same item and the same role (for a review: the same commit and step) claim the same thing,
//! and of their live claims the earliest by `(hlc, EventId)` holds it. Pure functions of the
//! state: nothing about a claim is stored.

use crate::id::{HashId, ItemId, SessionId};
use crate::identity::DeviceKey;
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
}

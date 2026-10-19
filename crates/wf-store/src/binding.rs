//! Making this device a device of the workforce it joins with an invite.

use wf_core::event::{DeviceBound, DeviceProfile, EventBody};
use wf_core::id::EventId;
use wf_core::identity::{Actor, DeviceKey, DeviceSigner, Signature};

use crate::replica::{AuthorError, Replica};
use crate::store::EventStore;

/// How far a device is in joining a workforce with an invite.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceBinding {
    NotBound,
    /// Bound by redeeming this invite: a join that may not have finished.
    BoundByInvite,
    /// Bound, but not by redeeming this invite.
    BoundWithoutInvite,
}

#[derive(Debug, thiserror::Error)]
pub enum BindError {
    #[error("the invite is not in the log the member sent")]
    UnknownInvite,
    #[error("this machine has already joined the workforce")]
    AlreadyJoined,
    #[error(transparent)]
    Author(#[from] AuthorError),
}

impl<S: EventStore> Replica<S> {
    pub fn binding_of(&self, device: DeviceKey, invite: EventId) -> DeviceBinding {
        let state = self.state();
        if state.principal_of(&device).is_none() {
            return DeviceBinding::NotBound;
        }
        match state.invite(&invite) {
            Some(known) if known.redeemed_by.contains(&device) => DeviceBinding::BoundByInvite,
            Some(_) | None => DeviceBinding::BoundWithoutInvite,
        }
    }

    /// Writes the events that make `signer` a device of the workforce: the binding with the
    /// invite, then its profile. What is already there is not written again, so a join that
    /// stopped halfway can be run again.
    pub fn bind_device(
        &mut self,
        signer: &DeviceSigner,
        invite: EventId,
        proof: Signature,
        profile: DeviceProfile,
        now_ms: u64,
    ) -> Result<(), BindError> {
        let principal = self
            .state()
            .invite(&invite)
            .map(|known| known.principal)
            .ok_or(BindError::UnknownInvite)?;
        let actor = Actor::Principal(principal);
        match self.binding_of(signer.key(), invite) {
            DeviceBinding::BoundWithoutInvite => return Err(BindError::AlreadyJoined),
            DeviceBinding::BoundByInvite => {}
            DeviceBinding::NotBound => {
                self.author(
                    signer,
                    actor.clone(),
                    now_ms,
                    EventBody::DeviceBound(DeviceBound { invite, proof }),
                )?;
            }
        }
        let has_profile = self
            .state()
            .devices()
            .any(|info| info.device == signer.key() && info.profile.is_some());
        if !has_profile {
            self.author(signer, actor, now_ms, EventBody::DeviceProfile(profile))?;
        }
        Ok(())
    }
}

//! Invite codes and the rules a member applies before letting a new device join.

use std::fmt;
use std::str::FromStr;

use data_encoding::BASE32_NOPAD;
use iroh_base::Signature;
use serde::{Deserialize, Serialize};

use crate::encoding;
use crate::id::{EventId, WorkforceId};
use crate::identity::{DeviceKey, InviteSigner, KeyError};
use crate::state::State;

const PREFIX: &str = "wfi1";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InviteCodeError {
    #[error("an invite code starts with `{PREFIX}`")]
    WrongPrefix,
    #[error("the invite code is not valid base32")]
    NotBase32,
    #[error("the invite code is damaged")]
    Malformed,
}

/// Why a member refuses to bind a device with an invite. Checked when the device asks to join,
/// not when its `DeviceBound` is admitted: expiry needs a clock and single use needs
/// coordination.
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq, Serialize, Deserialize)]
pub enum JoinRefusal {
    #[error("this device does not know the invite")]
    UnknownInvite,
    #[error("the invite has expired")]
    Expired,
    #[error("the invite has been used")]
    Redeemed,
    #[error("another device is joining with this invite right now")]
    Reserved,
    #[error("the proof is not valid for this device")]
    BadProof,
    #[error("this device does not serve that workforce")]
    NotServed,
    #[error("this device could not read its log")]
    Unreadable,
}

#[derive(Serialize, Deserialize)]
struct Payload {
    workforce: WorkforceId,
    invite: EventId,
    secret: [u8; 32],
    bootstrap: Vec<DeviceKey>,
}

/// What `wf invite device` prints and `wf join` reads: everything a new device needs to find the
/// workforce and prove it was invited.
#[derive(Clone, PartialEq, Eq)]
pub struct InviteCode {
    pub workforce: WorkforceId,
    /// The `InviteCreated` event.
    pub invite: EventId,
    secret: [u8; 32],
    /// Devices to dial, the inviting one first.
    pub bootstrap: Vec<DeviceKey>,
}

impl fmt::Debug for InviteCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InviteCode")
            .field("workforce", &self.workforce)
            .field("invite", &self.invite)
            .field("secret", &"<redacted>")
            .field("bootstrap", &self.bootstrap)
            .finish()
    }
}

impl InviteCode {
    pub fn new(
        workforce: WorkforceId,
        invite: EventId,
        secret: &InviteSigner,
        bootstrap: Vec<DeviceKey>,
    ) -> Self {
        Self {
            workforce,
            invite,
            secret: secret.to_bytes(),
            bootstrap,
        }
    }

    /// Signs that `device` is the one meant by this invite.
    pub fn prove(&self, device: DeviceKey) -> Result<Signature, KeyError> {
        InviteSigner::from_bytes(&self.secret)?.prove(self.workforce, self.invite, device)
    }
}

impl fmt::Display for InviteCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let bytes = encoding::encode(&Payload {
            workforce: self.workforce,
            invite: self.invite,
            secret: self.secret,
            bootstrap: self.bootstrap.clone(),
        })
        .map_err(|_| fmt::Error)?;
        write!(
            formatter,
            "{PREFIX}{}",
            BASE32_NOPAD.encode(&bytes).to_ascii_lowercase()
        )
    }
}

impl FromStr for InviteCode {
    type Err = InviteCodeError;

    fn from_str(text: &str) -> Result<Self, InviteCodeError> {
        let body = text
            .trim()
            .strip_prefix(PREFIX)
            .ok_or(InviteCodeError::WrongPrefix)?;
        let bytes = BASE32_NOPAD
            .decode(body.to_ascii_uppercase().as_bytes())
            .map_err(|_| InviteCodeError::NotBase32)?;
        let payload: Payload = encoding::decode(&bytes).map_err(|_| InviteCodeError::Malformed)?;
        Ok(Self {
            workforce: payload.workforce,
            invite: payload.invite,
            secret: payload.secret,
            bootstrap: payload.bootstrap,
        })
    }
}

impl State {
    /// The handshake rules for letting `device` join with `invite`, as of `now_ms`. Reservations
    /// by joins in progress are tracked by whoever serves the handshake.
    pub fn check_join(
        &self,
        invite: &EventId,
        device: DeviceKey,
        proof: &Signature,
        now_ms: u64,
    ) -> Result<(), JoinRefusal> {
        let workforce = self.workforce_id().ok_or(JoinRefusal::NotServed)?;
        let known = self.invite(invite).ok_or(JoinRefusal::UnknownInvite)?;
        if known
            .key
            .verify_proof(workforce, *invite, device, proof)
            .is_err()
        {
            return Err(JoinRefusal::BadProof);
        }
        if now_ms >= known.expires_ms {
            return Err(JoinRefusal::Expired);
        }
        // The device that redeemed the invite may ask again: its join may not have finished.
        if known.redeemed_by.iter().any(|redeemer| *redeemer != device) {
            return Err(JoinRefusal::Redeemed);
        }
        Ok(())
    }
}

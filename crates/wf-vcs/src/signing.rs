//! Signing commits with the key of a workforce device, and checking them against the devices of
//! the workforce.
//!
//! The device key also signs the events of the workforce. The two never sign the same bytes: git's
//! SSH signatures sign a blob that starts with `SSHSIG` and names the namespace `git`, events sign
//! bytes that start with `wf/event/1\0`.

use ssh_key::private::{Ed25519Keypair, KeypairData};
use ssh_key::public::{Ed25519PublicKey, KeyData};
use ssh_key::{LineEnding, PrivateKey, PublicKey};
use wf_core::identity::{DeviceKey, DeviceSigner};

#[derive(Debug, thiserror::Error)]
#[error("the device key cannot be written as an SSH key")]
pub struct SigningKeyError(#[from] ssh_key::Error);

/// The device's key as an OpenSSH private key file without a passphrase, for `ssh-keygen` to sign
/// commits with.
pub fn device_private_key(device: &DeviceSigner) -> Result<String, SigningKeyError> {
    let keypair = KeypairData::Ed25519(Ed25519Keypair::from_seed(&device.to_bytes()));
    let key = PrivateKey::new(keypair, "wf device")?;
    Ok(key.to_openssh(LineEnding::LF)?.to_string())
}

/// A line of a git allowed-signers file (`gpg.ssh.allowedSignersFile`): the signatures of
/// `device` on commits are good for `principal`.
pub fn allowed_signer(principal: &str, device: DeviceKey) -> Result<String, SigningKeyError> {
    let key = PublicKey::new(KeyData::Ed25519(Ed25519PublicKey(*device.as_bytes())), "");
    Ok(format!(
        "{principal} namespaces=\"git\" {}",
        key.to_openssh()?.trim()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ssh_key_is_the_device_key() {
        let device = DeviceSigner::generate([7; 32]);
        let written = PrivateKey::from_openssh(device_private_key(&device).unwrap()).unwrap();
        let KeypairData::Ed25519(keypair) = written.key_data() else {
            panic!("an ed25519 key");
        };
        assert_eq!(&keypair.public.0, device.key().as_bytes());
        let line = allowed_signer("eulervoid@laptop", device.key()).unwrap();
        assert!(
            line.starts_with("eulervoid@laptop namespaces=\"git\" ssh-ed25519 AAAA"),
            "{line}"
        );
    }
}

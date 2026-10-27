//! Canonical encoding. Every hash and signature in the system covers bytes produced here.
//!
//! The encoding is postcard: deterministic as long as no hash maps are serialized. Keep
//! all encoding in this module so format changes are deliberate and caught by golden tests.
//!
//! Stored events are never rewritten, so every body must decode from the bytes earlier versions
//! of the same event version wrote. `event/golden_bodies.txt` holds one event per body tag. Until
//! v1, a layout change is a new event version, and an older log is signed again with
//! `wf import --resign` (SPEC §17.7).

use serde::{Serialize, de::DeserializeOwned};

#[derive(Debug, thiserror::Error)]
pub enum EncodingError {
    #[error("canonical encoding failed")]
    Postcard(#[from] postcard::Error),
    #[error(
        "an event of version {0}, written by an older wf: export the workforce with that version \
         and sign it again with `wf import --resign <export.jsonl>`"
    )]
    OlderEvent(u16),
    #[error("an event of version {0}, written by a newer wf")]
    NewerEvent(u16),
}

impl EncodingError {
    /// Bytes that decode but are not the canonical encoding of what they decode to.
    pub(crate) fn malformed() -> Self {
        EncodingError::Postcard(postcard::Error::SerdeDeCustom)
    }
}

pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, EncodingError> {
    Ok(postcard::to_stdvec(value)?)
}

/// Fails on trailing bytes, so one value has exactly one accepted encoding.
pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, EncodingError> {
    let (value, rest) = postcard::take_from_bytes(bytes)?;
    if rest.is_empty() {
        Ok(value)
    } else {
        Err(EncodingError::malformed())
    }
}

/// Decodes the value the bytes start with and leaves the rest unread: for the version of an
/// event, which says how to read what follows it.
pub(crate) fn decode_leading<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, EncodingError> {
    Ok(postcard::take_from_bytes(bytes)?.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_trailing_bytes() {
        let mut bytes = encode(&7u32).unwrap();
        assert_eq!(decode::<u32>(&bytes).unwrap(), 7);
        bytes.push(0);
        assert!(decode::<u32>(&bytes).is_err());
    }
}

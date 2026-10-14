//! Canonical encoding. Every hash and signature in the system covers bytes produced here.
//!
//! The encoding is postcard: deterministic as long as no hash maps are serialized. Keep
//! all encoding in this module so format changes are deliberate and caught by golden tests.

use serde::{Serialize, de::DeserializeOwned};

#[derive(Debug, thiserror::Error)]
#[error("canonical encoding failed")]
pub struct EncodingError(#[from] postcard::Error);

impl EncodingError {
    /// Bytes that decode but are not the canonical encoding of what they decode to.
    pub(crate) fn malformed() -> Self {
        EncodingError(postcard::Error::SerdeDeCustom)
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

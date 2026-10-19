//! Content-addressed ids. All of them are BLAKE3 hashes shown as lowercase base32.

use std::fmt;
use std::str::FromStr;

use data_encoding::BASE32_NOPAD;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

const HASH_LEN: usize = 32;
pub const MIN_PREFIX_LEN: usize = 4;
pub const SHORT_ID_LEN: usize = 6;

/// The short form of an id already written out as text.
pub fn short(id: &str) -> &str {
    &id[..id.len().min(SHORT_ID_LEN)]
}
/// Comments are looked up among the comments of one item, so fewer characters tell them apart.
pub const SHORT_COMMENT_ID_LEN: usize = 4;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IdError {
    #[error("not valid base32 id")]
    Malformed,
    #[error("an id is {HASH_LEN} bytes")]
    WrongLength,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PrefixError {
    #[error("id prefix `{0}` is too short (minimum {MIN_PREFIX_LEN} characters)")]
    TooShort(String),
    #[error("id prefix `{0}` matches nothing")]
    NotFound(String),
    #[error("id prefix `{prefix}` is ambiguous: {}", candidates.join(", "))]
    Ambiguous {
        prefix: String,
        candidates: Vec<String>,
    },
}

fn encode(bytes: &[u8; HASH_LEN]) -> String {
    BASE32_NOPAD.encode(bytes).to_ascii_lowercase()
}

fn decode(text: &str) -> Result<[u8; HASH_LEN], IdError> {
    let bytes = BASE32_NOPAD
        .decode(text.to_ascii_uppercase().as_bytes())
        .map_err(|_| IdError::Malformed)?;
    bytes.try_into().map_err(|_| IdError::WrongLength)
}

/// Implemented by every id type so prefix resolution works over any of them.
pub trait HashId: Copy {
    /// How many characters of the base32 text are shown.
    const SHORT_LEN: usize = SHORT_ID_LEN;

    fn as_bytes(&self) -> &[u8; HASH_LEN];

    fn to_base32(&self) -> String {
        encode(self.as_bytes())
    }

    fn short(&self) -> String {
        self.to_base32()[..Self::SHORT_LEN].to_string()
    }
}

/// Reads a hash as one byte string. A tuple of 32 `u8`s costs 32 serde calls per id, which
/// dominates snapshot decoding.
pub(crate) struct HashVisitor;

impl serde::de::Visitor<'_> for HashVisitor {
    type Value = [u8; HASH_LEN];

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{HASH_LEN} bytes")
    }

    fn visit_bytes<E: serde::de::Error>(self, bytes: &[u8]) -> Result<Self::Value, E> {
        bytes
            .try_into()
            .map_err(|_| E::invalid_length(bytes.len(), &self))
    }
}

macro_rules! hash_id {
    ($(#[$meta:meta])* $name:ident) => {
        hash_id!($(#[$meta])* $name, short = SHORT_ID_LEN);
    };
    ($(#[$meta:meta])* $name:ident, short = $short:expr) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name([u8; HASH_LEN]);

        impl $name {
            pub const fn from_bytes(bytes: [u8; HASH_LEN]) -> Self {
                Self(bytes)
            }
        }

        impl HashId for $name {
            const SHORT_LEN: usize = $short;

            fn as_bytes(&self) -> &[u8; HASH_LEN] {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.to_base32())
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, "{}({})", stringify!($name), self.short())
            }
        }

        impl FromStr for $name {
            type Err = IdError;

            fn from_str(text: &str) -> Result<Self, IdError> {
                decode(text).map(Self)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                if serializer.is_human_readable() {
                    serializer.collect_str(self)
                } else {
                    serializer.serialize_bytes(&self.0)
                }
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                if deserializer.is_human_readable() {
                    let text = String::deserialize(deserializer)?;
                    text.parse().map_err(serde::de::Error::custom)
                } else {
                    deserializer.deserialize_bytes(HashVisitor).map(Self)
                }
            }
        }
    };
}

hash_id!(
    /// BLAKE3 of the canonical encoding of an event, signature included.
    EventId
);
hash_id!(
    /// The id of the genesis event. Identifies a workforce but is not a capability.
    WorkforceId
);
hash_id!(
    /// An item is identified by the event that created it.
    ItemId
);
hash_id!(
    /// A session is identified by the event that started it.
    SessionId
);
hash_id!(
    /// A comment is identified by its event. Shown short within its item.
    CommentId,
    short = SHORT_COMMENT_ID_LEN
);

impl EventId {
    pub fn of(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }
}

impl From<EventId> for WorkforceId {
    fn from(genesis: EventId) -> Self {
        Self(genesis.0)
    }
}

impl From<EventId> for ItemId {
    fn from(created_by: EventId) -> Self {
        Self(created_by.0)
    }
}

impl From<EventId> for SessionId {
    fn from(started_by: EventId) -> Self {
        Self(started_by.0)
    }
}

impl From<EventId> for CommentId {
    fn from(written_by: EventId) -> Self {
        Self(written_by.0)
    }
}

impl From<CommentId> for EventId {
    fn from(comment: CommentId) -> Self {
        Self(comment.0)
    }
}

/// Ids by their base32 text, for resolving a user supplied prefix like git does: unique match, at
/// least [`MIN_PREFIX_LEN`] characters.
pub struct PrefixIndex<T> {
    ids: std::collections::BTreeMap<String, T>,
}

impl<T: HashId + Copy> PrefixIndex<T> {
    pub fn new(ids: impl IntoIterator<Item = T>) -> Self {
        Self {
            ids: ids.into_iter().map(|id| (id.to_base32(), id)).collect(),
        }
    }

    pub fn resolve(&self, prefix: &str) -> Result<T, PrefixError> {
        let prefix = prefix.to_ascii_lowercase();
        if prefix.len() < MIN_PREFIX_LEN {
            return Err(PrefixError::TooShort(prefix));
        }
        let mut matches = self
            .ids
            .range(prefix.clone()..)
            .take_while(|(candidate, _)| candidate.starts_with(&prefix))
            .map(|(_, id)| *id);
        match (matches.next(), matches.next()) {
            (None, _) => Err(PrefixError::NotFound(prefix)),
            (Some(id), None) => Ok(id),
            (Some(first), Some(second)) => Err(PrefixError::Ambiguous {
                candidates: [first, second].iter().map(HashId::short).collect(),
                prefix,
            }),
        }
    }
}

/// Resolves a prefix among `candidates`; see [`PrefixIndex`].
pub fn resolve_prefix<T: HashId + Copy>(
    prefix: &str,
    candidates: impl IntoIterator<Item = T>,
) -> Result<T, PrefixError> {
    PrefixIndex::new(candidates).resolve(prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn golden_event_id() {
        let id = EventId::of(b"workforce");
        assert_eq!(
            id.to_string(),
            "tehllsu5fixh3xs7ign26cnsq7cojpuyepprljjctqwiopnq7k2q"
        );
        assert_eq!(id.short(), "tehlls");
    }

    #[test]
    fn base32_round_trip() {
        let id = EventId::of(b"round trip");
        assert_eq!(id.to_string().parse::<EventId>(), Ok(id));
        assert_eq!("aaaaaaaa".parse::<EventId>(), Err(IdError::WrongLength));
        assert_eq!("!!!!".parse::<EventId>(), Err(IdError::Malformed));
    }

    #[test]
    fn postcard_encoding_is_a_length_prefixed_byte_string() {
        let id = EventId::of(b"bytes");
        let encoded = postcard::to_stdvec(&id).unwrap();
        assert_eq!(encoded[0], 32);
        assert_eq!(&encoded[1..], id.as_bytes());
        assert_eq!(postcard::from_bytes::<EventId>(&encoded).unwrap(), id);
        assert!(postcard::from_bytes::<EventId>(&encoded[..20]).is_err());
    }

    #[test]
    fn json_uses_base32() {
        let id = ItemId::from(EventId::of(b"json"));
        assert_eq!(serde_json::to_string(&id).unwrap(), format!("\"{id}\""));
        assert_eq!(
            serde_json::from_str::<ItemId>(&format!("\"{id}\"")).unwrap(),
            id
        );
    }

    #[test]
    fn prefix_resolution() {
        let ids: Vec<ItemId> = (0..50u8)
            .map(|seed| ItemId::from(EventId::of(&[seed])))
            .collect();
        let wanted = ids[7];
        let prefix = &wanted.to_base32()[..8];
        assert_eq!(resolve_prefix(prefix, ids.clone()), Ok(wanted));
        assert_eq!(
            resolve_prefix("ab", ids.clone()),
            Err(PrefixError::TooShort("ab".into()))
        );
        assert!(matches!(
            resolve_prefix("2222222", ids.clone()),
            Err(PrefixError::NotFound(_))
        ));
        let first_char = &wanted.to_base32()[..1];
        let ambiguous = ids
            .iter()
            .filter(|id| id.to_base32().starts_with(first_char))
            .count();
        assert!(
            ambiguous > 1,
            "fixture should contain a shared first letter"
        );
    }

    #[test]
    fn ambiguous_prefix_is_reported() {
        let a = ItemId::from_bytes([0; 32]);
        let mut other = [0; 32];
        other[31] = 1;
        let b = ItemId::from_bytes(other);
        assert!(matches!(
            resolve_prefix("aaaa", [a, b]),
            Err(PrefixError::Ambiguous { .. })
        ));
    }
}

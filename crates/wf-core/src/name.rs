//! Validated names. Every name is lowercase ASCII so that `@mentions` and `#refs` parse unambiguously.

use serde::{Deserialize, Serialize};

const MAX_NAME_LEN: usize = 64;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NameError {
    #[error("{kind} must not be empty")]
    Empty { kind: &'static str },
    #[error("{kind} `{name}` is longer than {MAX_NAME_LEN} characters")]
    TooLong { kind: &'static str, name: String },
    #[error("{kind} `{name}` may only contain {allowed}, and must start with a letter or digit")]
    InvalidCharacter {
        kind: &'static str,
        name: String,
        allowed: &'static str,
    },
}

fn is_slug_char(character: char) -> bool {
    character.is_ascii_lowercase() || character.is_ascii_digit() || matches!(character, '-' | '_')
}

fn is_tag_char(character: char) -> bool {
    is_slug_char(character) || matches!(character, ':' | '.')
}

fn validate(
    kind: &'static str,
    name: &str,
    allowed: &'static str,
    is_allowed: fn(char) -> bool,
) -> Result<(), NameError> {
    if name.is_empty() {
        return Err(NameError::Empty { kind });
    }
    if name.len() > MAX_NAME_LEN {
        return Err(NameError::TooLong {
            kind,
            name: name.to_string(),
        });
    }
    let starts_well = name
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit());
    if !starts_well || !name.chars().all(is_allowed) {
        return Err(NameError::InvalidCharacter {
            kind,
            name: name.to_string(),
            allowed,
        });
    }
    Ok(())
}

macro_rules! name_type {
    ($(#[$meta:meta])* $name:ident, $kind:literal, $allowed:expr, $is_allowed:path) => {
        $(#[$meta])*
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub fn new(name: impl Into<String>) -> Result<Self, NameError> {
                let name = name.into();
                validate($kind, &name, $allowed, $is_allowed)?;
                Ok(Self(name))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = NameError;

            fn try_from(name: String) -> Result<Self, NameError> {
                Self::new(name)
            }
        }

        impl From<$name> for String {
            fn from(name: $name) -> String {
                name.0
            }
        }

        impl std::str::FromStr for $name {
            type Err = NameError;

            fn from_str(text: &str) -> Result<Self, NameError> {
                Self::new(text)
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl std::fmt::Debug for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(formatter, "{}({:?})", stringify!($name), self.0)
            }
        }
    };
}

const SLUG: &str = "a-z, 0-9, `-` and `_`";
const TAG: &str = "a-z, 0-9, `-`, `_`, `:` and `.`";

name_type!(
    /// A principal's handle, the `ada` in `@ada`.
    Handle, "handle", SLUG, is_slug_char
);
name_type!(
    /// An agent's name, the `grace` in `@ada/grace`. Unique per principal.
    AgentName, "agent name", SLUG, is_slug_char
);
name_type!(
    /// A name from the policy `[types.*]` table.
    TypeName, "type name", SLUG, is_slug_char
);
name_type!(
    /// A name from the policy `[lanes.*]` table.
    LaneName, "lane name", SLUG, is_slug_char
);
impl TypeName {
    /// The type of items that record a decision, which can supersede one another.
    pub fn decision() -> Self {
        Self("decision".to_owned())
    }
}

impl LaneName {
    /// The lane of agents that supervise: they may lower a review level and answer blockers.
    pub fn supervision() -> Self {
        Self("supervision".to_owned())
    }
}

name_type!(
    /// The harness an agent runs on, such as `claude-code`.
    HarnessName, "harness name", SLUG, is_slug_char
);
name_type!(
    /// A free-form item label.
    Label, "label", TAG, is_tag_char
);
name_type!(
    /// A review level of the policy, such as `standard`.
    ReviewLevelName, "review level", SLUG, is_slug_char
);
name_type!(
    /// The name of an agent step of a review level, such as `security`. Steps of one entry that
    /// run at once are told apart by it.
    ReviewStepName, "review step name", SLUG, is_slug_char
);
name_type!(
    /// A recovery level of the policy, such as `default`.
    RecoveryLevelName, "recovery level", SLUG, is_slug_char
);
name_type!(
    /// A capability or hardware requirement of an item, such as `gpu` or `os:linux`.
    Requirement, "requirement", TAG, is_tag_char
);
name_type!(
    /// The name of a command the daemon runs on code, such as the check `clippy`. Its logs are
    /// kept in a directory of that name.
    CommandName, "command name", SLUG, is_slug_char
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_slugs() {
        assert!(Handle::new("ada").is_ok());
        assert!(AgentName::new("coder-2").is_ok());
        assert!(Requirement::new("os:linux").is_ok());
    }

    #[test]
    fn rejects_bad_names() {
        assert_eq!(Handle::new(""), Err(NameError::Empty { kind: "handle" }));
        assert!(matches!(
            Handle::new("Josh"),
            Err(NameError::InvalidCharacter { .. })
        ));
        assert!(matches!(
            Handle::new("a/b"),
            Err(NameError::InvalidCharacter { .. })
        ));
        assert!(matches!(
            Handle::new("os:linux"),
            Err(NameError::InvalidCharacter { .. })
        ));
        assert!(matches!(
            AgentName::new("-x"),
            Err(NameError::InvalidCharacter { .. })
        ));
        assert!(matches!(
            AgentName::new("x".repeat(65)),
            Err(NameError::TooLong { .. })
        ));
    }

    #[test]
    fn decoding_validates() {
        let encoded = postcard::to_stdvec("Not Valid").unwrap();
        assert!(postcard::from_bytes::<Handle>(&encoded).is_err());
        let encoded = postcard::to_stdvec("valid").unwrap();
        assert_eq!(
            postcard::from_bytes::<Handle>(&encoded).unwrap().as_str(),
            "valid"
        );
    }
}

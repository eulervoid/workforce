//! How much an agent does without its principal: when it starts, how long it is left alone and what
//! it may do. Part of the agent definition, so only its principal can change it.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Whether the dispatcher starts the agent on ready items.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StartMode {
    /// Only through `wf session start`.
    Manual,
    Auto,
}

/// How the permission requests of a session are answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionMode {
    Allow,
    /// Passed through the deciders; a request none of them answers is held for the principal.
    Ask,
    Deny,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{kind} must be one of {allowed}")]
pub struct ModeError {
    kind: &'static str,
    allowed: &'static str,
}

impl fmt::Display for StartMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            StartMode::Manual => "manual",
            StartMode::Auto => "auto",
        })
    }
}

impl FromStr for StartMode {
    type Err = ModeError;

    fn from_str(text: &str) -> Result<Self, ModeError> {
        match text {
            "manual" => Ok(StartMode::Manual),
            "auto" => Ok(StartMode::Auto),
            _ => Err(ModeError {
                kind: "start",
                allowed: "manual, auto",
            }),
        }
    }
}

impl fmt::Display for PermissionMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            PermissionMode::Allow => "allow",
            PermissionMode::Ask => "ask",
            PermissionMode::Deny => "deny",
        })
    }
}

impl FromStr for PermissionMode {
    type Err = ModeError;

    fn from_str(text: &str) -> Result<Self, ModeError> {
        match text {
            "allow" => Ok(PermissionMode::Allow),
            "ask" => Ok(PermissionMode::Ask),
            "deny" => Ok(PermissionMode::Deny),
            _ => Err(ModeError {
                kind: "permissions",
                allowed: "allow, ask, deny",
            }),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Autonomy {
    pub start: StartMode,
    /// How many turns in a row without a new commit a session gets before it is released.
    pub nudges: u32,
    /// How long a session may wait for an answer before it ends.
    pub park_timeout: Duration,
    pub max_duration: Option<Duration>,
    pub permissions: PermissionMode,
}

impl Default for Autonomy {
    /// Nothing starts without the principal. Inside its sandbox an agent may do anything; a machine that
    /// runs agents without one asks instead.
    fn default() -> Self {
        Self {
            start: StartMode::Manual,
            nudges: 3,
            park_timeout: Duration::from_secs(2 * 60 * 60),
            max_duration: None,
            permissions: PermissionMode::Allow,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_round_trip_through_text() {
        for mode in [StartMode::Manual, StartMode::Auto] {
            assert_eq!(mode.to_string().parse(), Ok(mode));
        }
        for mode in [
            PermissionMode::Allow,
            PermissionMode::Ask,
            PermissionMode::Deny,
        ] {
            assert_eq!(mode.to_string().parse(), Ok(mode));
        }
        assert!("sometimes".parse::<StartMode>().is_err());
        assert!("maybe".parse::<PermissionMode>().is_err());
    }
}

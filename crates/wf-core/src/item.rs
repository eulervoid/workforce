//! The item model: the single universal document behind tasks, questions and decisions.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Priority {
    P0,
    P1,
    P2,
    P3,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("priority must be one of P0, P1, P2, P3")]
pub struct PriorityError;

impl fmt::Display for Priority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Priority::P0 => "P0",
            Priority::P1 => "P1",
            Priority::P2 => "P2",
            Priority::P3 => "P3",
        })
    }
}

impl FromStr for Priority {
    type Err = PriorityError;

    fn from_str(text: &str) -> Result<Self, PriorityError> {
        match text.to_ascii_uppercase().as_str() {
            "P0" => Ok(Priority::P0),
            "P1" => Ok(Priority::P1),
            "P2" => Ok(Priority::P2),
            "P3" => Ok(Priority::P3),
            _ => Err(PriorityError),
        }
    }
}

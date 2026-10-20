//! `wf-core` is pure: no IO, no async, no clock or randomness except through parameters.

pub mod activity;
pub mod autonomy;
pub mod body;
pub mod brief;
pub mod claims;
pub mod commits;
pub mod crdt;
pub mod dispatch;
pub mod encoding;
mod error_chain;
pub mod event;
mod fold;
pub mod graph;
pub mod hlc;
pub mod id;
pub mod identity;
pub mod inbox;
pub mod invite;
pub mod item;
pub mod lifecycle;
pub mod name;
pub mod policy;
pub mod recovery;
pub mod refs;
pub mod resolve;
pub mod review;
pub mod search;
pub mod session;
pub mod state;
pub mod stats;
pub mod supervision;
pub mod sync;
pub mod validate;

pub use error_chain::error_chain;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

#[cfg(test)]
mod tests;

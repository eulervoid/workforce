//! `wf-core` is pure: no IO, no async, no clock or randomness except through parameters.

pub mod activity;
pub mod autonomy;
pub mod body;
pub mod brief;
pub mod crdt;
pub mod dispatch;
pub mod encoding;
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
pub mod refs;
pub mod report;
pub mod resolve;
pub mod review;
pub mod session;
pub mod state;
pub mod supervision;
pub mod sync;
pub mod validate;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

#[cfg(test)]
mod tests;

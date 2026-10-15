//! What commands print. Each view is a plain serializable struct: `--json` prints it inside a
//! versioned [`Envelope`], text output is rendered from the same struct in `render`.

mod agent;
mod detail;
mod event;
mod inbox;
mod item;
mod session;
mod status;

pub use agent::*;
pub use detail::*;
pub use event::*;
pub use inbox::*;
pub use item::*;
pub use session::*;
pub use status::*;

use jiff::Timestamp;
use serde::Serialize;
use wf_core::hlc::Hlc;
use wf_core::identity::Actor;
use wf_core::resolve::MentionTarget;
use wf_core::state::State;

/// Bump when a view changes shape incompatibly. Snapshot tests pin the current shapes.
pub const SCHEMA_VERSION: u32 = 8;

#[derive(Serialize)]
pub struct Envelope<T> {
    pub schema: u32,
    pub data: T,
}

impl<T> Envelope<T> {
    pub fn new(data: T) -> Self {
        Self {
            schema: SCHEMA_VERSION,
            data,
        }
    }
}

pub fn timestamp(hlc: Hlc) -> String {
    timestamp_ms(hlc.wall_ms())
}

pub fn timestamp_ms(millis: u64) -> String {
    i64::try_from(millis)
        .ok()
        .and_then(|millis| Timestamp::from_millisecond(millis).ok())
        .map_or_else(|| "unknown".to_string(), |time| time.to_string())
}

/// Who did something: what to call them, and which kind of actor they are, for the color.
#[derive(Clone, Serialize)]
pub struct ActorView {
    pub kind: ActorKind,
    pub label: String,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    Principal,
    Agent,
    Daemon,
}

pub fn actor_view(state: &State, actor: &Actor) -> ActorView {
    let kind = match actor {
        Actor::Principal(_) => ActorKind::Principal,
        Actor::Agent { .. } => ActorKind::Agent,
        Actor::Daemon(_) => ActorKind::Daemon,
    };
    ActorView {
        kind,
        label: state.actor_label(actor),
    }
}

/// `@ada (3f9a2c)`: whose device it is and which one.
pub fn device_label(state: &State, device: wf_core::identity::DeviceKey) -> String {
    match state.principal_of(&device) {
        Some(principal) => format!(
            "{} ({})",
            state.actor_label(&Actor::Principal(principal)),
            device.fmt_short()
        ),
        None => device.fmt_short(),
    }
}

pub fn timestamp_now() -> String {
    Timestamp::now().to_string()
}

pub fn mention_view(state: &State, target: &MentionTarget) -> ActorView {
    let kind = match target {
        MentionTarget::Principal(_) => ActorKind::Principal,
        MentionTarget::Agent(_) => ActorKind::Agent,
    };
    ActorView {
        kind,
        label: mention_label(state, target),
    }
}

pub fn mention_label(state: &State, target: &MentionTarget) -> String {
    match target {
        MentionTarget::Principal(key) => state.actor_label(&Actor::Principal(*key)),
        MentionTarget::Agent(agent) => {
            let handle = state.member(&agent.principal).map_or_else(
                || agent.principal.fmt_short(),
                |member| member.handle.to_string(),
            );
            format!("@{handle}/{}", agent.name)
        }
    }
}

/// A history line for `wf item show --history`: like [`event_view`] but with the values written,
/// so an edit that lost a concurrent write is still readable.
fn set_change<T: std::fmt::Display>(change: &wf_core::crdt::SetChange<T>) -> String {
    match change {
        wf_core::crdt::SetChange::Add(value) => format!("+{value}"),
        wf_core::crdt::SetChange::Remove { value, .. } => format!("-{value}"),
    }
}

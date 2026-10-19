//! Runs an agent harness over ACP and records everything that crosses the connection.

mod permission;
mod run;
mod transcript;

pub use agent_client_protocol::{AcpAgentConfig, BoxFuture};
pub use permission::{Decider, Decision, Permission, PermissionRequest, Permissions};
pub use run::{
    AgentSessionId, CancelHandle, Conversation, Error, Launch, Model, Opening, StopReason,
    KnownHarness, models,
};
pub use transcript::{Direction, TranscriptEntry};

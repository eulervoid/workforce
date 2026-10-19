//! Runs an agent harness over ACP and records everything that crosses the connection.

mod permission;
mod run;
mod transcript;

pub use agent_client_protocol::schema::v1::ToolKind;
pub use agent_client_protocol::{AcpAgentConfig, BoxFuture};
pub use permission::{
    Decider, Decision, MissingOption, Permission, PermissionRequest, Permissions,
};
pub use run::{
    AgentSessionId, CancelHandle, Conversation, Error, KnownHarness, Launch, Model, Opening,
    StopReason, models,
};
pub use transcript::{Direction, TranscriptEntry};

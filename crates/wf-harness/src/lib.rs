//! Runs an agent harness over ACP and records everything that crosses the connection.

mod permission;
mod run;
mod transcript;

pub use agent_client_protocol::{AcpAgentConfig, BoxFuture};
pub use permission::{Decision, PermissionMode, PermissionRequest, Permissions};
pub use run::{CancelHandle, Conversation, Error, Launch, StopReason, command_for};
pub use transcript::{Direction, TranscriptEntry};

//! Runs an agent harness over ACP and records everything that crosses the connection.

mod package;
mod run;
mod transcript;

pub use agent_client_protocol::{AcpAgentConfig, BoxFuture};
pub use package::{CONTINUE, Dependency, Earlier, ReviewPackage, WorkPackage};
pub use run::{
    CancelHandle, Conversation, Error, Launch, PermissionMode, Permissions, StopReason, command_for,
};
pub use transcript::{Direction, TranscriptEntry};

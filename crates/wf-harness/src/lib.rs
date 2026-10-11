//! Runs an agent harness over ACP and records everything that crosses the connection.

mod package;
mod render;
mod run;
mod transcript;

pub use agent_client_protocol::AcpAgentConfig;
pub use package::{CONTINUE, Dependency, Earlier, WorkPackage};
pub use render::Renderer;
pub use run::{CancelHandle, Conversation, Error, Launch, PermissionMode, StopReason, command_for};
pub use transcript::{Direction, TranscriptEntry};

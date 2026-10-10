//! Runs an agent harness over ACP and records everything that crosses the connection.

mod render;
mod run;
mod transcript;

pub use render::Renderer;
pub use run::{Error, PermissionMode, Run, StopReason, command_for};
pub use transcript::{Direction, TranscriptEntry};

use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    ContentBlock, InitializeRequest, NewSessionRequest, PermissionOption, PermissionOptionKind,
    PromptRequest, RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionNotification, TextContent,
};
use agent_client_protocol::{AcpAgent, AcpAgentConfig, Agent, Client, ConnectionTo};
use jiff::Timestamp;

pub use agent_client_protocol::schema::v1::StopReason;

use crate::transcript::{Direction, TranscriptEntry, TranscriptWriter};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("agent connection failed: {0}")]
    Protocol(#[from] agent_client_protocol::Error),
    #[error("writing the transcript failed: {0}")]
    Transcript(#[from] io::Error),
}

/// How permission requests from the agent are answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionMode {
    Allow,
    Deny,
}

impl PermissionMode {
    fn answer(self, options: &[PermissionOption]) -> RequestPermissionOutcome {
        let wanted = match self {
            PermissionMode::Allow => [
                PermissionOptionKind::AllowOnce,
                PermissionOptionKind::AllowAlways,
            ],
            PermissionMode::Deny => [
                PermissionOptionKind::RejectOnce,
                PermissionOptionKind::RejectAlways,
            ],
        };
        wanted
            .iter()
            .find_map(|kind| options.iter().find(|option| option.kind == *kind))
            .map_or(RequestPermissionOutcome::Cancelled, |option| {
                RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                    option.option_id.clone(),
                ))
            })
    }
}

/// The command that starts a known harness in ACP mode.
pub fn command_for(harness: &str) -> Option<AcpAgentConfig> {
    match harness {
        "opencode" => Some(AcpAgentConfig::new("opencode").arg("acp")),
        "claude-code" => Some(AcpAgentConfig::new("claude-agent-acp")),
        "codex" => Some(AcpAgentConfig::new("codex-acp")),
        "gemini-cli" => Some(AcpAgentConfig::new("gemini").arg("--experimental-acp")),
        _ => None,
    }
}

/// One prompt to one agent in one directory. Every line that crosses the connection is appended
/// to `transcript`.
pub struct Run<'a, W> {
    pub agent: AcpAgentConfig,
    pub directory: &'a Path,
    pub prompt: &'a str,
    pub permissions: PermissionMode,
    pub transcript: W,
}

impl<W: Write + Send + 'static> Run<'_, W> {
    /// Starts the agent, sends the prompt, waits for the turn to end and stops the agent.
    pub async fn execute(self) -> Result<StopReason, Error> {
        let writer = Arc::new(TranscriptWriter::new(self.transcript));
        let recorder = Arc::clone(&writer);
        let agent = AcpAgent::new(self.agent).with_debug(move |line, direction| {
            let direction = Direction::from(direction);
            recorder.record(&TranscriptEntry::from_line(
                Timestamp::now(),
                direction,
                line,
            ));
        });
        let permissions = self.permissions;
        let directory = self.directory.to_path_buf();
        let prompt = self.prompt.to_owned();

        let outcome = Client
            .builder()
            .on_receive_notification(
                async move |_notification: SessionNotification, _connection| Ok(()),
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                async move |request: RequestPermissionRequest, responder, _connection| {
                    responder.respond(RequestPermissionResponse::new(
                        permissions.answer(&request.options),
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(agent, |connection: ConnectionTo<Agent>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let session = connection
                    .send_request(NewSessionRequest::new(directory))
                    .block_task()
                    .await?;
                let response = connection
                    .send_request(PromptRequest::new(
                        session.session_id,
                        vec![ContentBlock::Text(TextContent::new(prompt))],
                    ))
                    .block_task()
                    .await?;
                Ok(response.stop_reason)
            })
            .await;

        writer.finish()?;
        Ok(outcome?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> Vec<PermissionOption> {
        vec![
            PermissionOption::new("always", "Always", PermissionOptionKind::AllowAlways),
            PermissionOption::new("reject", "Reject", PermissionOptionKind::RejectOnce),
            PermissionOption::new("once", "Once", PermissionOptionKind::AllowOnce),
        ]
    }

    fn selected(outcome: RequestPermissionOutcome) -> String {
        match outcome {
            RequestPermissionOutcome::Selected(selected) => selected.option_id.to_string(),
            other => panic!("expected a selection, got {other:?}"),
        }
    }

    #[test]
    fn allow_prefers_once_over_always() {
        assert_eq!(selected(PermissionMode::Allow.answer(&options())), "once");
    }

    #[test]
    fn deny_picks_a_reject_option() {
        assert_eq!(selected(PermissionMode::Deny.answer(&options())), "reject");
    }

    #[test]
    fn missing_options_cancel() {
        assert!(matches!(
            PermissionMode::Deny.answer(&options()[..1]),
            RequestPermissionOutcome::Cancelled
        ));
    }

    #[test]
    fn known_harnesses_have_commands() {
        let opencode = command_for("opencode").unwrap();
        assert_eq!(opencode.arguments(), ["acp"]);
        assert!(command_for("unknown").is_none());
    }
}

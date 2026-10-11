use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock, InitializeRequest, NewSessionRequest, PromptRequest,
    RequestPermissionRequest, RequestPermissionResponse, SessionNotification, SessionUpdate,
    TextContent,
};
use agent_client_protocol::{AcpAgent, AcpAgentConfig, Agent, Client, ConnectionTo};
use jiff::Timestamp;
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::task::JoinHandle;

pub use agent_client_protocol::schema::v1::StopReason;

use crate::permission::{PermissionRequest, Permissions};
use crate::transcript::{Direction, TranscriptEntry, TranscriptWriter};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("agent connection failed")]
    Protocol(#[from] agent_client_protocol::Error),
    #[error("writing the transcript failed")]
    Transcript(#[from] io::Error),
    #[error("the agent is gone")]
    Closed,
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

/// What it takes to start an agent: the command, where it works and how it is answered. Every
/// line that crosses the connection is appended to `transcript`.
pub struct Launch<W> {
    pub agent: AcpAgentConfig,
    pub directory: PathBuf,
    pub permissions: Arc<dyn Permissions>,
    pub transcript: W,
}

struct Turn {
    prompt: String,
    reply: oneshot::Sender<Result<(StopReason, String), Error>>,
}

/// A running agent with one ACP session, which is prompted turn after turn and keeps its context
/// between them. Dropping it stops the agent.
pub struct Conversation {
    turns: mpsc::Sender<Turn>,
    cancel: Arc<Notify>,
    task: JoinHandle<Result<(), Error>>,
}

impl Conversation {
    /// Starts the agent and opens the session, ready for the first prompt.
    pub async fn start<W: Write + Send + 'static>(launch: Launch<W>) -> Result<Self, Error> {
        let writer = Arc::new(TranscriptWriter::new(launch.transcript));
        let recorder = Arc::clone(&writer);
        let decided = Arc::clone(&writer);
        let agent = AcpAgent::new(launch.agent).with_debug(move |line, direction| {
            let direction = Direction::from(direction);
            recorder.record(&TranscriptEntry::from_line(
                Timestamp::now(),
                direction,
                line,
            ));
        });
        let permissions = launch.permissions;
        let directory = launch.directory;
        let (turns, mut incoming) = mpsc::channel::<Turn>(1);
        let (ready, opened) = oneshot::channel();
        let cancel = Arc::new(Notify::new());
        let cancelled = Arc::clone(&cancel);
        // What the agent says in the turn that runs, as `reply` returns it.
        let said = Arc::new(Mutex::new(String::new()));
        let heard = Arc::clone(&said);

        let task = tokio::spawn(async move {
            let outcome = Client
                .builder()
                .on_receive_notification(
                    async move |notification: SessionNotification, _connection| {
                        if let SessionUpdate::AgentMessageChunk(chunk) = notification.update
                            && let ContentBlock::Text(text) = chunk.content
                        {
                            heard
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .push_str(&text.text);
                        }
                        Ok(())
                    },
                    agent_client_protocol::on_receive_notification!(),
                )
                .on_receive_request(
                    async move |request: RequestPermissionRequest, responder, _connection| {
                        let decision = permissions
                            .decide(PermissionRequest::of(&request.tool_call.fields))
                            .await;
                        decided.record(&TranscriptEntry::decision(Timestamp::now(), &decision));
                        responder.respond(RequestPermissionResponse::new(
                            decision.permission.answer(&request.options),
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
                    let _ = ready.send(());
                    while let Some(turn) = incoming.recv().await {
                        said.lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .clear();
                        let request = connection
                            .send_request(PromptRequest::new(
                                session.session_id.clone(),
                                vec![ContentBlock::Text(TextContent::new(turn.prompt))],
                            ))
                            .block_task();
                        tokio::pin!(request);
                        let response = loop {
                            tokio::select! {
                                response = &mut request => break response,
                                () = cancelled.notified() => {
                                    connection.send_notification(
                                        CancelNotification::new(session.session_id.clone()),
                                    )?;
                                }
                            }
                        };
                        let text = std::mem::take(
                            &mut *said.lock().unwrap_or_else(|poisoned| poisoned.into_inner()),
                        );
                        let _ = turn.reply.send(
                            response
                                .map(|response| (response.stop_reason, text))
                                .map_err(Error::from),
                        );
                    }
                    Ok(())
                })
                .await;
            writer.finish()?;
            Ok(outcome?)
        });

        match opened.await {
            Ok(()) => Ok(Self {
                turns,
                cancel,
                task,
            }),
            Err(_) => Err(match task.await {
                Ok(Err(error)) => error,
                Ok(Ok(())) | Err(_) => Error::Closed,
            }),
        }
    }

    /// Sends a prompt and waits for the turn to end.
    pub async fn prompt(&mut self, prompt: impl Into<String>) -> Result<StopReason, Error> {
        self.reply(prompt).await.map(|(stop, _)| stop)
    }

    /// Sends a prompt and waits for the turn to end, with what the agent said in it.
    pub async fn reply(
        &mut self,
        prompt: impl Into<String>,
    ) -> Result<(StopReason, String), Error> {
        let (reply, answer) = oneshot::channel();
        let turn = Turn {
            prompt: prompt.into(),
            reply,
        };
        self.turns.send(turn).await.map_err(|_| Error::Closed)?;
        answer.await.map_err(|_| Error::Closed)?
    }

    /// Asks the agent to stop the turn that is running. The turn then ends with
    /// [`StopReason::Cancelled`]. Does nothing between turns.
    pub fn cancel(&self) -> CancelHandle {
        CancelHandle(Arc::clone(&self.cancel))
    }

    /// Stops the agent and waits until it is gone.
    pub async fn close(self) -> Result<(), Error> {
        drop(self.turns);
        match self.task.await {
            Ok(result) => result,
            Err(_) => Err(Error::Closed),
        }
    }
}

/// Cancels the turn of a conversation that is being prompted, from another task.
#[derive(Clone)]
pub struct CancelHandle(Arc<Notify>);

impl CancelHandle {
    pub fn cancel(&self) {
        self.0.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_harnesses_have_commands() {
        let opencode = command_for("opencode").unwrap();
        assert_eq!(opencode.arguments(), ["acp"]);
        assert!(command_for("unknown").is_none());
    }
}

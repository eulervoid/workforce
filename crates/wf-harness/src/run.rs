use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock, InitializeRequest, LoadSessionRequest, NewSessionRequest,
    PromptRequest, RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    ResumeSessionRequest, SessionConfigId, SessionConfigKind, SessionConfigOption,
    SessionConfigOptionCategory, SessionConfigSelectOption, SessionConfigSelectOptions,
    SessionConfigValueId, SessionId, SessionNotification, SessionUpdate,
    SetSessionConfigOptionRequest, TextContent,
};
use agent_client_protocol::{AcpAgent, AcpAgentConfig, Agent, Client, ConnectionTo};
use jiff::Timestamp;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};
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
    #[error("the agent can neither resume nor load a session it opened before")]
    CannotResume,
    #[error("the agent does not offer the model {model:?}; it offers: {}", .available.join(", "))]
    UnknownModel {
        model: String,
        available: Vec<String>,
    },
}

/// A model an agent offers, as its model option lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Model {
    /// What `--model` takes.
    pub value: String,
    pub name: String,
    /// The agent uses it when no model is chosen.
    pub is_default: bool,
}

/// The choices of the agent's model options, each with the option it belongs to and whether it
/// is the option's current value.
fn model_choices(
    options: &[SessionConfigOption],
) -> Vec<(&SessionConfigId, bool, &SessionConfigSelectOption)> {
    let mut found = Vec::new();
    for option in options {
        if option.category != Some(SessionConfigOptionCategory::Model) {
            continue;
        }
        let SessionConfigKind::Select(select) = &option.kind else {
            continue;
        };
        let choices: Vec<_> = match &select.options {
            SessionConfigSelectOptions::Ungrouped(choices) => choices.iter().collect(),
            SessionConfigSelectOptions::Grouped(groups) => {
                groups.iter().flat_map(|group| &group.options).collect()
            }
            _ => Vec::new(),
        };
        for choice in choices {
            found.push((&option.id, choice.value == select.current_value, choice));
        }
    }
    found
}

/// The current value of the agent's model option; `None` when it has none.
fn current_model(options: &[SessionConfigOption]) -> Option<String> {
    model_choices(options)
        .into_iter()
        .find(|(_, is_current, _)| *is_current)
        .map(|(_, _, choice)| choice.value.0.to_string())
}

/// The model option of the agent and the value of it that `wanted` names, by value or by name.
fn choose_model(
    options: &[SessionConfigOption],
    wanted: &str,
) -> Result<(SessionConfigId, SessionConfigValueId), Error> {
    let choices = model_choices(options);
    if let Some((option, _, choice)) = choices.iter().find(|(_, _, choice)| {
        &*choice.value.0 == wanted || choice.name.eq_ignore_ascii_case(wanted)
    }) {
        return Ok(((*option).clone(), choice.value.clone()));
    }
    Err(Error::UnknownModel {
        model: wanted.to_string(),
        available: choices
            .iter()
            .map(|(_, _, choice)| format!("{} ({})", choice.value.0, choice.name))
            .collect(),
    })
}

/// Starts the agent, opens a session in `directory` to read the models it offers, and stops it
/// again without prompting it.
pub async fn models(agent: AcpAgentConfig, directory: PathBuf) -> Result<Vec<Model>, Error> {
    let models = Client
        .connect_with(
            AcpAgent::new(agent),
            async move |connection: ConnectionTo<Agent>| {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let session = connection
                    .send_request(NewSessionRequest::new(directory))
                    .block_task()
                    .await?;
                let options = session.config_options.as_deref().unwrap_or_default();
                Ok(model_choices(options)
                    .into_iter()
                    .map(|(_, is_default, choice)| Model {
                        value: choice.value.0.to_string(),
                        name: choice.name.clone(),
                        is_default,
                    })
                    .collect())
            },
        )
        .await?;
    Ok(models)
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

/// The id an agent gives a session it opens, which it can resume or load later by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentSessionId(pub String);

/// Which session of the agent a conversation opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Opening {
    New,
    /// A session an earlier conversation opened, with what was said in it. The agent resumes it
    /// if it can, or else loads it; the history a load replays is not recorded again.
    Resume(AgentSessionId),
}

/// What it takes to start an agent: the command, where it works and how it is answered. Every
/// line that crosses the connection is appended to `transcript`.
pub struct Launch<W> {
    pub agent: AcpAgentConfig,
    pub opening: Opening,
    /// The model to prompt, as the agent names it: the value or the name of an option it offers.
    /// Without one the agent keeps its own default.
    pub model: Option<String>,
    pub directory: PathBuf,
    pub permissions: Arc<dyn Permissions>,
    pub transcript: W,
}

struct Turn {
    prompt: String,
    reply: oneshot::Sender<Result<(StopReason, String), Error>>,
}

/// How long [`Conversation::close`] waits for the agent to exit before it kills it.
const CLOSE_GRACE: Duration = Duration::from_secs(10);

/// The task that talks to the agent. Aborting it drops the agent's process, which kills its
/// process group, so a conversation that is dropped, or a start that is given up, leaves nothing
/// running.
struct AgentTask(Option<JoinHandle<Result<(), Error>>>);

impl Drop for AgentTask {
    fn drop(&mut self) {
        if let Some(task) = &self.0 {
            task.abort();
        }
    }
}

/// A running agent with one ACP session, which is prompted turn after turn and keeps its context
/// between them. Dropping it stops the agent.
pub struct Conversation {
    session: AgentSessionId,
    /// The current value of the agent's model option once the session opened and the model
    /// was set; `None` when the agent has no model option.
    model: Option<String>,
    turns: mpsc::Sender<Turn>,
    /// Counts the cancels asked for. A watch keeps the latest value, so no cancel is lost, and
    /// both the turn and a permission request it waits on see it.
    cancel: Arc<watch::Sender<u64>>,
    task: AgentTask,
}

impl Conversation {
    /// Starts the agent and opens the session, ready for the first prompt.
    pub async fn start<W: Write + Send + 'static>(launch: Launch<W>) -> Result<Self, Error> {
        let writer = Arc::new(TranscriptWriter::new(launch.transcript));
        let recorder = Arc::clone(&writer);
        let decided = Arc::clone(&writer);
        // While a loaded session replays its history, the updates are neither recorded nor heard.
        let replaying = Arc::new(AtomicBool::new(false));
        let muted = Arc::clone(&replaying);
        let agent = AcpAgent::new(launch.agent).with_debug(move |line, direction| {
            let entry =
                TranscriptEntry::from_line(Timestamp::now(), Direction::from(direction), line);
            if muted.load(Ordering::SeqCst) && entry.message["method"] == "session/update" {
                return;
            }
            recorder.record(&entry);
        });
        let opening = launch.opening;
        let unheard = Arc::clone(&replaying);
        let permissions = launch.permissions;
        let directory = launch.directory;
        let model = launch.model;
        let (refused, mut refusal) = oneshot::channel();
        let (turns, mut incoming) = mpsc::channel::<Turn>(1);
        let (ready, opened) = oneshot::channel::<(SessionId, Option<String>)>();
        let (cancel, cancelled) = watch::channel(0u64);
        let cancel = Arc::new(cancel);
        let mut cancelled_turn = cancelled.clone();
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
                            && !unheard.load(Ordering::SeqCst)
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
                        // A request that is still undecided when its turn is cancelled is
                        // answered `cancelled`, as ACP asks of a client.
                        let mut cancelled = cancelled.clone();
                        cancelled.mark_unchanged();
                        let outcome = tokio::select! {
                            decision = permissions
                                .decide(PermissionRequest::of(&request.tool_call.fields)) => {
                                decided.record(&TranscriptEntry::decision(
                                    Timestamp::now(),
                                    &decision,
                                ));
                                decision.permission.answer(&request.options)
                            }
                            _ = cancelled.changed() => RequestPermissionOutcome::Cancelled,
                        };
                        responder.respond(RequestPermissionResponse::new(outcome))
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .connect_with(agent, |connection: ConnectionTo<Agent>| async move {
                    let initialized = connection
                        .send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;
                    let capabilities = initialized.agent_capabilities;
                    let (session_id, options) = match opening {
                        Opening::New => {
                            let session = connection
                                .send_request(NewSessionRequest::new(directory))
                                .block_task()
                                .await?;
                            (session.session_id, session.config_options)
                        }
                        Opening::Resume(AgentSessionId(id))
                            if capabilities.session_capabilities.resume.is_some() =>
                        {
                            let session_id = SessionId::new(id);
                            let resumed = connection
                                .send_request(ResumeSessionRequest::new(
                                    session_id.clone(),
                                    directory,
                                ))
                                .block_task()
                                .await?;
                            (session_id, resumed.config_options)
                        }
                        Opening::Resume(AgentSessionId(id)) if capabilities.load_session => {
                            let session_id = SessionId::new(id);
                            replaying.store(true, Ordering::SeqCst);
                            let loaded = connection
                                .send_request(LoadSessionRequest::new(
                                    session_id.clone(),
                                    directory,
                                ))
                                .block_task()
                                .await;
                            replaying.store(false, Ordering::SeqCst);
                            (session_id, loaded?.config_options)
                        }
                        Opening::Resume(_) => {
                            let _ = refused.send(Error::CannotResume);
                            return Err(agent_client_protocol::util::internal_error(
                                "cannot resume",
                            ));
                        }
                    };
                    let options = options.unwrap_or_default();
                    let mut current = current_model(&options);
                    if let Some(wanted) = &model {
                        match choose_model(&options, wanted) {
                            Ok((option, value)) => {
                                let set = connection
                                    .send_request(SetSessionConfigOptionRequest::new(
                                        session_id.clone(),
                                        option,
                                        &*value.0,
                                    ))
                                    .block_task()
                                    .await?;
                                // An agent that leaves its options out of the answer runs the
                                // model it was set to.
                                current = current_model(&set.config_options)
                                    .or_else(|| Some(value.0.to_string()));
                            }
                            Err(error) => {
                                let _ = refused.send(error);
                                return Err(agent_client_protocol::util::internal_error(
                                    "unknown model",
                                ));
                            }
                        }
                    }
                    let _ = ready.send((session_id.clone(), current));
                    while let Some(turn) = incoming.recv().await {
                        said.lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .clear();
                        let request = connection
                            .send_request(PromptRequest::new(
                                session_id.clone(),
                                vec![ContentBlock::Text(TextContent::new(turn.prompt))],
                            ))
                            .block_task();
                        tokio::pin!(request);
                        let response = loop {
                            tokio::select! {
                                response = &mut request => break response,
                                Ok(()) = cancelled_turn.changed() => {
                                    connection.send_notification(
                                        CancelNotification::new(session_id.clone()),
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

        // Given up before it opened, the start drops the task and with it the agent.
        let mut task = AgentTask(Some(task));
        match opened.await {
            Ok((session, model)) => Ok(Self {
                session: AgentSessionId(session.0.to_string()),
                model,
                turns,
                cancel,
                task,
            }),
            Err(_) => {
                let finished = task.0.take().expect("the task is there until here").await;
                Err(match finished {
                    Ok(Err(error)) => refusal.try_recv().unwrap_or(error),
                    Ok(Ok(())) | Err(_) => Error::Closed,
                })
            }
        }
    }

    /// The id the agent gave the session, to resume it by in a later conversation.
    pub fn session(&self) -> &AgentSessionId {
        &self.session
    }

    /// The model the session runs with, as the value of the agent's model option.
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
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

    /// Asks the agent to stop the turn that is running, and answers a permission request it
    /// waits on with `cancelled`. The turn then ends with [`StopReason::Cancelled`]. A cancel
    /// that comes before the turn has reached the agent still cancels it.
    pub fn cancel(&self) -> CancelHandle {
        CancelHandle(Arc::clone(&self.cancel))
    }

    /// Stops the agent and waits until it is gone, at most [`CLOSE_GRACE`]; an agent that takes
    /// longer is killed.
    pub async fn close(mut self) -> Result<(), Error> {
        drop(self.turns);
        let task = self.task.0.take().expect("the task is there until closed");
        let abort = task.abort_handle();
        match tokio::time::timeout(CLOSE_GRACE, task).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(Error::Closed),
            Err(_) => {
                abort.abort();
                Err(Error::Closed)
            }
        }
    }
}

/// Cancels the turn of a conversation that is being prompted, from another task.
#[derive(Clone)]
pub struct CancelHandle(Arc<watch::Sender<u64>>);

impl CancelHandle {
    pub fn cancel(&self) {
        self.0.send_modify(|cancels| *cancels += 1);
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

#[cfg(test)]
mod model_tests {
    use agent_client_protocol::schema::v1::SessionConfigSelect;

    use super::*;

    fn models() -> Vec<SessionConfigOption> {
        fn choice(value: &str, name: &str) -> SessionConfigSelectOption {
            SessionConfigSelectOption::new(value.to_string(), name.to_string())
        }
        fn select(current: &str, choices: Vec<SessionConfigSelectOption>) -> SessionConfigKind {
            SessionConfigKind::Select(SessionConfigSelect::new(current.to_string(), choices))
        }
        vec![
            SessionConfigOption::new(
                "effort",
                "Effort",
                select("low", vec![choice("haiku", "not a model")]),
            ),
            SessionConfigOption::new(
                "model",
                "Model",
                select(
                    "default",
                    vec![
                        choice("default", "Default"),
                        choice("sonnet", "Sonnet 5.5"),
                        choice("haiku", "Haiku 5.5"),
                    ],
                ),
            )
            .category(SessionConfigOptionCategory::Model),
        ]
    }

    #[test]
    fn a_model_is_found_by_value_or_by_name() {
        for wanted in ["haiku", "Haiku 5.5", "haiku 5.5"] {
            let (option, value) = choose_model(&models(), wanted).unwrap();
            assert_eq!(&*option.0, "model");
            assert_eq!(&*value.0, "haiku");
        }
    }

    #[test]
    fn an_unknown_model_lists_what_the_agent_offers() {
        let Err(Error::UnknownModel { model, available }) =
            choose_model(&models(), "claude-haiku-5-5")
        else {
            panic!("expected the model to be refused");
        };
        assert_eq!(model, "claude-haiku-5-5");
        assert_eq!(
            available,
            [
                "default (Default)",
                "sonnet (Sonnet 5.5)",
                "haiku (Haiku 5.5)"
            ]
        );
    }

    #[test]
    fn an_agent_without_a_model_option_offers_nothing() {
        assert!(matches!(
            choose_model(&[], "haiku"),
            Err(Error::UnknownModel { available, .. }) if available.is_empty()
        ));
    }
}

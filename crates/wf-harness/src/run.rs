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
    #[error("the task that runs the agent panicked")]
    Panicked,
    #[error("the agent exited before it opened a session")]
    ExitedBeforeOpen,
    #[error(
        "the agent did not stop within {}; it was killed",
        humantime::format_duration(CLOSE_GRACE)
    )]
    Killed,
    #[error("the conversation is over")]
    Ended,
    #[error("the agent said nothing for {}", humantime::format_duration(*.0))]
    Silent(Duration),
    #[error("the agent can neither resume nor load a session it opened before")]
    CannotResume,
    #[error("the agent does not offer the model {model:?}; it offers: {}", .available.join(", "))]
    UnknownModel {
        model: String,
        available: Vec<String>,
    },
}

impl Error {
    /// Whether the agent says that it needs a login first, as one whose login lapsed does.
    pub fn needs_login(&self) -> bool {
        match self {
            Error::Protocol(error) => error.code == agent_client_protocol::ErrorCode::AuthRequired,
            Error::Transcript(_)
            | Error::Panicked
            | Error::ExitedBeforeOpen
            | Error::Killed
            | Error::Ended
            | Error::Silent(_)
            | Error::CannotResume
            | Error::UnknownModel { .. } => false,
        }
    }
}

/// The agent says its usage limit is reached, as its harness reports `errorKind: "rate_limit"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UsageLimit {
    /// When the limit resets, if the agent said.
    pub resets_at: Option<Timestamp>,
}

impl Error {
    /// Whether the agent says its usage limit is reached; `now` places a reset time of day, such
    /// as `resets 7:50pm (Europe/Berlin)`, on the next time it comes.
    pub fn usage_limit(&self, now: Timestamp) -> Option<UsageLimit> {
        let Error::Protocol(error) = self else {
            return None;
        };
        let kind = error.data.as_ref()?.get("errorKind")?.as_str()?;
        (kind == "rate_limit").then(|| UsageLimit {
            resets_at: parse_reset(&error.message, now),
        })
    }
}

/// The time `message` says a limit resets, as `resets 7:50pm (Europe/Berlin)` or `resets 7pm`:
/// the next time of day after `now`, in the time zone named or else the machine's.
fn parse_reset(message: &str, now: Timestamp) -> Option<Timestamp> {
    let (_, after) = message.split_once("resets ")?;
    let (clock, rest) = after.split_once(char::is_whitespace).unwrap_or((after, ""));
    let (digits, is_pm) = match clock.to_ascii_lowercase().as_str() {
        text if text.ends_with("pm") => (text.trim_end_matches("pm").to_string(), true),
        text if text.ends_with("am") => (text.trim_end_matches("am").to_string(), false),
        _ => return None,
    };
    let (hour, minute) = digits.split_once(':').unwrap_or((&digits, "0"));
    let hour: i8 = hour.parse().ok().filter(|hour| (1..=12).contains(hour))?;
    let minute: i8 = minute
        .parse()
        .ok()
        .filter(|minute| (0..60).contains(minute))?;
    let hour = hour % 12 + if is_pm { 12 } else { 0 };
    let zone = rest
        .trim()
        .strip_prefix('(')
        .and_then(|zone| zone.split_once(')'))
        .and_then(|(name, _)| jiff::tz::TimeZone::get(name).ok())
        .unwrap_or_else(jiff::tz::TimeZone::system);
    let now = now.to_zoned(zone);
    let today = now
        .date()
        .at(hour, minute, 0, 0)
        .to_zoned(now.time_zone().clone())
        .ok()?;
    let reset = if today > now {
        today
    } else {
        today.checked_add(jiff::Span::new().days(1)).ok()?
    };
    Some(reset.timestamp())
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
            spawned(agent),
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

/// `agent`, started so that it inherits no descriptor of wf beyond stdin, stdout and stderr.
///
/// On macOS std creates a pipe and sets close-on-exec on it in two steps, as there is no
/// `pipe2`, and sockets the same way. A process spawned on another thread in between inherits
/// that end: an agent that holds the write end of the pipe a `git` call reads keeps the call
/// waiting until the agent exits. `AcpAgent` builds its `Command` itself, so neither a `pre_exec`
/// nor `POSIX_SPAWN_CLOEXEC_DEFAULT` can be set on it, and a lock around spawns would not cover
/// pipes and sockets opened by other code. So the agent starts through `sh`, which closes every
/// descriptor above stderr and then becomes the agent. Elsewhere std creates them close-on-exec
/// at once and the agent starts as it is.
fn spawned(agent: AcpAgentConfig) -> AcpAgent {
    if cfg!(target_os = "macos") {
        AcpAgent::new(
            AcpAgentConfig::new("/bin/sh")
                .arg("-c")
                .arg(CLOSE_INHERITED)
                .arg("sh")
                .arg(agent.command().to_string_lossy())
                .args(agent.arguments())
                .envs(agent.environment()),
        )
    } else {
        AcpAgent::new(agent)
    }
}

/// Closes every descriptor above stderr, then runs its arguments in its place.
const CLOSE_INHERITED: &str = r#"for path in /dev/fd/*; do
    descriptor=${path##*/}
    [ "$descriptor" -gt 2 ] && eval "exec $descriptor>&-"
done
exec "$@""#;

/// A harness wf knows how to start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KnownHarness {
    ClaudeCode,
    Opencode,
    Codex,
    GeminiCli,
}

impl KnownHarness {
    /// Every known harness, in the order they are offered.
    pub const ALL: [KnownHarness; 4] = [
        KnownHarness::ClaudeCode,
        KnownHarness::Opencode,
        KnownHarness::Codex,
        KnownHarness::GeminiCli,
    ];

    /// The harness called `name`, if wf knows it.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|harness| harness.name() == name)
    }

    pub fn name(self) -> &'static str {
        match self {
            KnownHarness::ClaudeCode => "claude-code",
            KnownHarness::Opencode => "opencode",
            KnownHarness::Codex => "codex",
            KnownHarness::GeminiCli => "gemini-cli",
        }
    }

    /// The command that starts it in ACP mode.
    pub fn command(self) -> AcpAgentConfig {
        match self {
            KnownHarness::ClaudeCode => AcpAgentConfig::new("claude-agent-acp"),
            KnownHarness::Opencode => AcpAgentConfig::new("opencode").arg("acp"),
            KnownHarness::Codex => AcpAgentConfig::new("codex-acp"),
            KnownHarness::GeminiCli => AcpAgentConfig::new("gemini").arg("--experimental-acp"),
        }
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
    /// How long a turn may go without a line from the agent while no permission request of it
    /// waits; the turn then fails with [`Error::Silent`].
    pub silence_limit: Duration,
}

struct Turn {
    prompt: String,
    reply: oneshot::Sender<Result<(StopReason, String), Error>>,
    /// How many cancels were asked for when the turn was: a later one cancels it, also one that
    /// came before the task picked the turn up.
    cancels_before: u64,
}

/// How long [`Conversation::close`] waits for the agent to exit before it kills it.
const CLOSE_GRACE: Duration = Duration::from_secs(10);

/// How often a turn checks how long the agent has been silent.
const SILENCE_CHECK: Duration = Duration::from_secs(1);

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
    /// Counts the turns the agent accepted, as [`Conversation::accepted`] tells them.
    accepted: Arc<watch::Sender<u64>>,
    task: AgentTask,
}

impl Conversation {
    /// Starts the agent and opens the session, ready for the first prompt.
    pub async fn start<W: Write + Send + 'static>(launch: Launch<W>) -> Result<Self, Error> {
        let writer = Arc::new(TranscriptWriter::new(launch.transcript));
        let recorder = Arc::clone(&writer);
        let decided = Arc::clone(&writer);
        let writer_of_turns = Arc::clone(&writer);
        let mut transcript_failed = writer.failed();
        // When the agent was last heard from, and how many of its permission requests wait: a
        // turn that waits on the principal is not silent.
        let heard_at = Arc::new(Mutex::new(tokio::time::Instant::now()));
        let heard = Arc::clone(&heard_at);
        let answered = Arc::clone(&heard_at);
        let asking = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let waiting = Arc::clone(&asking);
        let silence_limit = launch.silence_limit;
        // While a loaded session replays its history, the updates are neither recorded nor heard.
        let replaying = Arc::new(AtomicBool::new(false));
        let muted = Arc::clone(&replaying);
        let agent = spawned(launch.agent).with_debug(move |line, direction| {
            let entry =
                TranscriptEntry::from_line(Timestamp::now(), Direction::from(direction), line);
            match entry.direction {
                Direction::FromAgent | Direction::AgentLog => {
                    *heard
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                        tokio::time::Instant::now();
                }
                Direction::ToAgent | Direction::Wf => {}
            }
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
        let chunks = Arc::clone(&said);
        // Whether the prompt of the turn that runs was sent and not accepted yet: the agent
        // accepts it with its first update in the turn, or with its answer.
        let unaccepted = Arc::new(AtomicBool::new(false));
        let accepting = Arc::clone(&unaccepted);
        let (accepted, _) = watch::channel(0u64);
        let accepted = Arc::new(accepted);
        let acceptor = Arc::clone(&accepted);
        let answer_acceptor = Arc::clone(&accepted);

        let task = tokio::spawn(async move {
            let outcome = Client
                .builder()
                .on_receive_notification(
                    async move |notification: SessionNotification, _connection| {
                        if !unheard.load(Ordering::SeqCst)
                            && accepting.swap(false, Ordering::SeqCst)
                        {
                            acceptor.send_modify(|turns| *turns += 1);
                        }
                        if let SessionUpdate::AgentMessageChunk(chunk) = notification.update
                            && let ContentBlock::Text(text) = chunk.content
                            && !unheard.load(Ordering::SeqCst)
                        {
                            chunks
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
                        waiting.fetch_add(1, Ordering::SeqCst);
                        let outcome = tokio::select! {
                            decision = permissions
                                .decide(PermissionRequest::of(&request.tool_call.fields)) => {
                                decided.record(&TranscriptEntry::decision(
                                    Timestamp::now(),
                                    &request.tool_call.tool_call_id.0,
                                    &decision,
                                ));
                                decision.permission.answer(&request.options)
                            }
                            _ = cancelled.changed() => Ok(RequestPermissionOutcome::Cancelled),
                        };
                        waiting.fetch_sub(1, Ordering::SeqCst);
                        *answered
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                            tokio::time::Instant::now();
                        match outcome {
                            Ok(outcome) => {
                                responder.respond(RequestPermissionResponse::new(outcome))
                            }
                            Err(missing) => responder.respond_with_error(
                                agent_client_protocol::Error::invalid_params()
                                    .data(missing.to_string()),
                            ),
                        }
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
                        // A cancel asked for while no turn ran, or too late for the last one,
                        // is not meant for this turn; one since it was asked for is.
                        let cancelled_early =
                            *cancelled_turn.borrow_and_update() != turn.cancels_before;
                        said.lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .clear();
                        *heard_at
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                            tokio::time::Instant::now();
                        unaccepted.store(true, Ordering::SeqCst);
                        let request = connection
                            .send_request(PromptRequest::new(
                                session_id.clone(),
                                vec![ContentBlock::Text(TextContent::new(turn.prompt))],
                            ))
                            .block_task();
                        tokio::pin!(request);
                        // Sent after the prompt, which `send_request` has sent already.
                        if cancelled_early {
                            connection
                                .send_notification(CancelNotification::new(session_id.clone()))?;
                        }
                        // A turn whose transcript cannot be written, or whose agent went silent,
                        // fails: nobody could follow it, or it hangs.
                        let response = loop {
                            tokio::select! {
                                response = &mut request => break response.map_err(Error::from),
                                Ok(()) = cancelled_turn.changed() => {
                                    connection.send_notification(
                                        CancelNotification::new(session_id.clone()),
                                    )?;
                                }
                                Ok(_) = transcript_failed.wait_for(|failed| *failed) => {
                                    break Err(match writer_of_turns.finish() {
                                        Err(error) => Error::Transcript(error),
                                        Ok(()) => Error::Ended,
                                    });
                                }
                                () = tokio::time::sleep(SILENCE_CHECK) => {
                                    let heard = *heard_at
                                        .lock()
                                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                                    if asking.load(Ordering::SeqCst) == 0
                                        && heard.elapsed() >= silence_limit
                                    {
                                        break Err(Error::Silent(silence_limit));
                                    }
                                }
                            }
                        };
                        let text = std::mem::take(
                            &mut *said.lock().unwrap_or_else(|poisoned| poisoned.into_inner()),
                        );
                        let failed = response.is_err();
                        if unaccepted.swap(false, Ordering::SeqCst) && !failed {
                            answer_acceptor.send_modify(|turns| *turns += 1);
                        }
                        let _ = turn
                            .reply
                            .send(response.map(|response| (response.stop_reason, text)));
                        // The agent is left mid-turn: it is not prompted again.
                        if failed {
                            break;
                        }
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
                accepted,
                task,
            }),
            Err(_) => {
                let finished = task.0.take().expect("the task is there until here").await;
                Err(match finished {
                    Ok(Err(error)) => refusal.try_recv().unwrap_or(error),
                    Ok(Ok(())) => Error::ExitedBeforeOpen,
                    Err(_) => Error::Panicked,
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
            cancels_before: *self.cancel.borrow(),
        };
        self.turns.send(turn).await.map_err(|_| Error::Ended)?;
        answer.await.map_err(|_| Error::Ended)?
    }

    /// Asks the agent to stop the turn that is running, and answers a permission request it
    /// waits on with `cancelled`. The turn then ends with [`StopReason::Cancelled`]. A cancel
    /// that comes while no turn runs cancels nothing.
    pub fn cancel(&self) -> CancelHandle {
        CancelHandle(Arc::clone(&self.cancel))
    }

    /// Counts the turns whose prompt the agent accepted: it took it up with its first update in
    /// the turn, or answered it. A prompt that is sent is not yet accepted, as the agent may never
    /// read it.
    pub fn accepted(&self) -> watch::Receiver<u64> {
        self.accepted.subscribe()
    }

    /// Stops the agent and waits until it is gone, at most [`CLOSE_GRACE`]; an agent that takes
    /// longer is killed.
    pub async fn close(mut self) -> Result<(), Error> {
        drop(self.turns);
        let task = self.task.0.take().expect("the task is there until closed");
        let abort = task.abort_handle();
        match tokio::time::timeout(CLOSE_GRACE, task).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(Error::Panicked),
            Err(_) => {
                abort.abort();
                Err(Error::Killed)
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
        let opencode = KnownHarness::from_name("opencode").unwrap().command();
        assert_eq!(opencode.arguments(), ["acp"]);
        assert!(KnownHarness::from_name("unknown").is_none());
        for harness in KnownHarness::ALL {
            assert_eq!(KnownHarness::from_name(harness.name()), Some(harness));
        }
    }

    fn rate_limit(message: &str) -> Error {
        let mut error = agent_client_protocol::Error::internal_error();
        error.message = message.to_string();
        error.data = Some(serde_json::json!({"errorKind": "rate_limit"}));
        Error::Protocol(error)
    }

    #[test]
    fn a_rate_limit_says_when_it_resets() {
        let now: Timestamp = "2026-10-19T17:32:00Z".parse().unwrap();
        let limit = rate_limit("You've hit your session limit · resets 7:50pm (Europe/Berlin)")
            .usage_limit(now)
            .expect("a rate limit");
        assert_eq!(
            limit.resets_at,
            Some("2026-10-19T17:50:00Z".parse().unwrap())
        );
        let tomorrow = rate_limit("limit · resets 7:50am (Europe/Berlin)")
            .usage_limit(now)
            .unwrap();
        assert_eq!(
            tomorrow.resets_at,
            Some("2026-10-20T05:50:00Z".parse().unwrap())
        );
        let unsaid = rate_limit("slow down").usage_limit(now).unwrap();
        assert_eq!(unsaid.resets_at, None);
    }

    #[test]
    fn midnight_and_noon_are_12am_and_12pm() {
        let now: Timestamp = "2026-10-19T17:32:00Z".parse().unwrap();
        assert_eq!(
            parse_reset("resets 12am (UTC)", now),
            Some("2026-10-20T00:00:00Z".parse().unwrap())
        );
        assert_eq!(
            parse_reset("resets 12:30pm (UTC)", now),
            Some("2026-10-20T12:30:00Z".parse().unwrap())
        );
        assert_eq!(
            parse_reset(
                "resets 12:30pm (UTC)",
                "2026-10-19T11:00:00Z".parse().unwrap()
            ),
            Some("2026-10-19T12:30:00Z".parse().unwrap())
        );
    }

    #[test]
    fn a_reset_that_cannot_be_read_is_none() {
        let now: Timestamp = "2026-10-19T17:32:00Z".parse().unwrap();
        for message in [
            "limit reached",
            "resets soon",
            "resets 19:50",
            "resets 13pm (UTC)",
            "resets 0am (UTC)",
            "resets 7:75pm (UTC)",
            "resets x:50pm (UTC)",
        ] {
            assert_eq!(parse_reset(message, now), None, "{message}");
        }
    }

    #[test]
    fn a_reset_without_zone_is_in_the_machine_zone() {
        let now: Timestamp = "2026-10-19T17:32:00Z".parse().unwrap();
        let system = jiff::tz::TimeZone::system();
        let reset = parse_reset("resets 7:50pm", now).expect("a reset");
        let local = reset.to_zoned(system);
        assert_eq!((local.hour(), local.minute()), (19, 50));
        assert!(reset > now && reset <= now + std::time::Duration::from_secs(25 * 3600));
        assert_eq!(
            parse_reset("resets 7:50pm (Nowhere/Land)", now),
            Some(reset)
        );
    }

    #[test]
    fn other_errors_are_no_rate_limit() {
        let now: Timestamp = "2026-10-19T17:32:00Z".parse().unwrap();
        let mut error = agent_client_protocol::Error::internal_error();
        error.message = "resets 7:50pm".to_string();
        assert_eq!(Error::Protocol(error).usage_limit(now), None);
        assert_eq!(Error::Ended.usage_limit(now), None);
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

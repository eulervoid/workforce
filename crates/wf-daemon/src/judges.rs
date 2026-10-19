//! Agents that decide permission requests. The daemon keeps one harness session per agent and
//! workforce, prompts it once for each request, and replaces it after a number of prompts so its
//! context stays small. These are not wf sessions: nothing about them goes into the log.

use std::collections::BTreeMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use tokio::sync::Mutex;
use wf_core::id::WorkforceId;
use wf_core::name::AgentName;
use wf_harness::{Conversation, Launch, Opening, Permission};

use crate::Daemon;
use crate::decide::{AgentDeciders, DeciderRequest, Outcome, read_answer};

/// How long an agent has to answer one request.
const ANSWER_WITHIN: Duration = Duration::from_secs(120);

/// How long a judge has to start.
const START_WITHIN: Duration = Duration::from_secs(180);

struct Judge {
    conversation: Conversation,
    prompts: u32,
}

/// The session of one judge, if it runs, held while it answers a request.
type Slot = Arc<Mutex<Option<Judge>>>;

/// The live judge sessions of this daemon.
#[derive(Default)]
pub(crate) struct Judges {
    sessions: std::sync::Mutex<BTreeMap<(WorkforceId, AgentName), Slot>>,
}

impl Judges {
    fn slot(&self, workforce: WorkforceId, agent: &AgentName) -> Slot {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(sessions.entry((workforce, agent.clone())).or_default())
    }

    /// Closes every judge, waiting for the ones that answer a request to finish; a request that
    /// asks a closed judge starts a new one.
    pub(crate) async fn close(&self) {
        let slots: Vec<Slot> = std::mem::take(
            &mut *self
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
        .into_values()
        .collect();
        for slot in slots {
            if let Some(judge) = slot.lock().await.take() {
                let _ = judge.conversation.close().await;
            }
        }
    }
}

/// The agent deciders of one workforce, as the chain of a session in it asks them.
pub(crate) struct WorkforceJudges {
    pub(crate) daemon: Weak<Daemon>,
    pub(crate) workforce: WorkforceId,
}

impl AgentDeciders for WorkforceJudges {
    fn ask<'a>(
        &'a self,
        agent: &'a AgentName,
        turns: u32,
        request: &'a DeciderRequest,
    ) -> wf_harness::BoxFuture<'a, Outcome> {
        Box::pin(async move {
            match self.try_ask(agent, turns, request).await {
                Ok(outcome) => outcome,
                Err(why) => Outcome::Passed { why },
            }
        })
    }
}

impl WorkforceJudges {
    async fn try_ask(
        &self,
        agent: &AgentName,
        turns: u32,
        request: &DeciderRequest,
    ) -> Result<Outcome, String> {
        let daemon = self.daemon.upgrade().ok_or("the daemon is stopping")?;
        let slot = daemon.judges.slot(self.workforce, agent);
        let mut judge = slot.lock().await;
        if judge.as_ref().is_some_and(|judge| judge.prompts >= turns)
            && let Some(old) = judge.take()
        {
            let _ = old.conversation.close().await;
        }
        if judge.is_none() {
            let started = tokio::select! {
                started = self.start(&daemon, agent) => started?,
                () = daemon.until_halted() => return Err("the daemon is stopping".to_string()),
            };
            *judge = Some(started);
        }
        let live = judge.as_mut().expect("started above");
        live.prompts += 1;
        let prompt = prompt(request);
        let reply = tokio::select! {
            reply = tokio::time::timeout(ANSWER_WITHIN, live.conversation.reply(prompt)) => reply,
            () = daemon.until_halted() => {
                *judge = None;
                return Err("the daemon is stopping".to_string());
            }
        };
        match reply {
            Ok(Ok((_, said))) => Ok(answer_in(&said)),
            Ok(Err(error)) => {
                *judge = None;
                Err(format!("its session failed: {error}"))
            }
            Err(_) => {
                *judge = None;
                Err(format!(
                    "it did not answer within {}",
                    humantime::format_duration(ANSWER_WITHIN)
                ))
            }
        }
    }

    async fn start(&self, daemon: &Arc<Daemon>, agent: &AgentName) -> Result<Judge, String> {
        let loaded = daemon
            .loaded_workforce(&self.workforce)
            .ok_or("the workforce is not loaded")?;
        let state = daemon
            .snapshot(&loaded)
            .await
            .map_err(|error| error.to_string())?;
        let principal = daemon
            .principal_of_device(&state)
            .map_err(|error| error.to_string())?;
        let definition = state
            .agent(agent, principal)
            .ok_or_else(|| format!("there is no agent `{agent}`"))?;
        let command = daemon
            .harness_command(&definition.harness)
            .ok_or_else(|| format!("wf does not know the harness `{}`", definition.harness))?;
        let machine_config = daemon.machine_config().map_err(|error| error.to_string())?;
        let environment = machine_config.environment_of(agent);
        let workforce = daemon.workforce_directory(self.workforce);
        let directory = workforce.join("judges").join(agent.as_str());
        let transcripts = daemon.config.data_dir.transcripts(&self.workforce);
        for path in [&directory, &transcripts] {
            std::fs::create_dir_all(path)
                .map_err(|error| format!("{}: {error}", path.display()))?;
        }
        // Judges read text the requesting agent controls, so they run in the sandbox of their agent,
        // working in their own directory.
        let settings = workforce
            .join("sandbox")
            .join(format!("judge-{agent}.json"));
        let command = crate::runner::sandboxed_in(
            daemon,
            &machine_config.sandbox_of(agent),
            &directory,
            &directory,
            None,
            &settings,
            command,
        )
        .map_err(|problem| format!("the sandbox could not be set up: {problem}"))?;
        let command = crate::environment::confine(
            command,
            &environment,
            crate::environment::daemon_variables(),
        );
        let transcript_path = transcripts.join(format!("judge-{agent}.jsonl"));
        let transcript = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&transcript_path)
            .map_err(|error| format!("{}: {error}", transcript_path.display()))?;
        let conversation = start_within(
            START_WITHIN,
            Launch {
                agent: command,
                opening: Opening::New,
                model: definition.model.clone(),
                directory,
                // A judge only reads what it is given; whatever it asks to do is refused.
                permissions: Arc::new(Permission::Deny),
                transcript,
                silence_limit: crate::runner::SILENCE_LIMIT,
            },
        )
        .await?;
        Ok(Judge {
            conversation,
            prompts: 0,
        })
    }
}

/// The conversation of `launch`, or why it did not start within `limit`. A harness that hangs at
/// start is dropped, which stops it.
async fn start_within<W: std::io::Write + Send + 'static>(
    limit: Duration,
    launch: Launch<W>,
) -> Result<Conversation, String> {
    match tokio::time::timeout(limit, Conversation::start(launch)).await {
        Ok(Ok(conversation)) => Ok(conversation),
        Ok(Err(error)) => Err(format!("its session does not start: {error}")),
        Err(_) => Err(format!(
            "its session did not start within {}",
            humantime::format_duration(limit)
        )),
    }
}

fn prompt(request: &DeciderRequest) -> String {
    let request = serde_json::to_string_pretty(request).unwrap_or_default();
    format!(
        "You decide whether a coding agent may make a tool call. Do not use any tools yourself: \
         read the request below and answer.\n\n\
         The agent works on the item in the request, in its own directory. Follow the guidance \
         in the request when there is one. Allow what is safe and serves the item; deny what is \
         clearly harmful or against the guidance; say ask when the principal should decide.\n\n\
         ```json\n{request}\n```\n\n\
         Answer with exactly one line of JSON and nothing else:\n\
         {{\"permission\": \"allow\" | \"deny\" | \"ask\", \"reason\": \"<one sentence>\"}}\n"
    )
}

/// The answer in what the agent said: the last line that is one, or the JSON object in it.
fn answer_in(said: &str) -> Outcome {
    let line = said
        .lines()
        .rev()
        .map(|line| line.trim().trim_matches('`').trim())
        .find(|line| line.starts_with('{') && line.ends_with('}'));
    let object = match (said.find('{'), said.rfind('}')) {
        (Some(start), Some(end)) if start < end => Some(&said[start..=end]),
        _ => None,
    };
    match line.or(object) {
        Some(answer) => read_answer(answer),
        None => Outcome::Passed {
            why: "it did not answer with JSON".to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_judge_whose_harness_never_answers_the_start_does_not_hold_the_request() {
        let launch = Launch {
            agent: wf_harness::AcpAgentConfig::new("sleep").arg("60"),
            opening: Opening::New,
            model: None,
            directory: std::env::temp_dir(),
            permissions: Arc::new(Permission::Deny),
            transcript: std::io::sink(),
            silence_limit: Duration::from_secs(60),
        };
        let started = std::time::Instant::now();
        let outcome = start_within(Duration::from_millis(200), launch).await;
        assert!(outcome.is_err_and(|why| why.contains("did not start within")));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn the_answer_is_found_in_what_the_agent_says() {
        let said = "Looking at the request, this only runs the tests.\n\n\
                    ```json\n{\"permission\": \"allow\", \"reason\": \"runs tests\"}\n```";
        assert_eq!(
            answer_in(said),
            Outcome::Decided {
                permission: Permission::Allow,
                reason: Some("runs tests".into())
            }
        );
        assert!(matches!(
            answer_in("I am not sure."),
            Outcome::Passed { .. }
        ));
    }
}

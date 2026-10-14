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
            *judge = Some(self.start(&daemon, agent).await?);
        }
        let live = judge.as_mut().expect("started above");
        live.prompts += 1;
        let prompt = prompt(request);
        match tokio::time::timeout(ANSWER_WITHIN, live.conversation.reply(prompt)).await {
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
        let environment = daemon
            .machine_config()
            .map_err(|error| error.to_string())?
            .environment_of(agent);
        let command = crate::environment::confine(
            command,
            &environment,
            crate::environment::daemon_variables(),
        );
        let workforce = daemon.workforce_directory(self.workforce);
        let directory = workforce.join("judges").join(agent.as_str());
        let transcripts = workforce.join("transcripts");
        for path in [&directory, &transcripts] {
            std::fs::create_dir_all(path)
                .map_err(|error| format!("{}: {error}", path.display()))?;
        }
        let transcript_path = transcripts.join(format!("judge-{agent}.jsonl"));
        let transcript = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&transcript_path)
            .map_err(|error| format!("{}: {error}", transcript_path.display()))?;
        let conversation = Conversation::start(Launch {
            agent: command,
            opening: Opening::New,
            model: definition.model.clone(),
            directory,
            // A judge only reads what it is given; whatever it asks to do is refused.
            permissions: Arc::new(Permission::Deny),
            transcript,
            silence_limit: crate::runner::SILENCE_LIMIT,
        })
        .await
        .map_err(|error| format!("its session does not start: {error}"))?;
        Ok(Judge {
            conversation,
            prompts: 0,
        })
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

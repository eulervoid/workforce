//! The chain of deciders a permission request passes through before the principal is asked.

mod hook;
mod model;
pub mod rules;

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use wf_core::id::{HashId, ItemId, SessionId, WorkforceId};
use wf_core::inbox::{PERMISSION_KIND, permission_waits};
use wf_core::name::AgentName;
use wf_harness::{Decision, Permission, PermissionRequest, Permissions};

use crate::machine_config::{DeciderConfig, MachineConfig, Notice};
use crate::permissions::Held;

/// What every decider is given: the call and what it is part of. Hooks get it as JSON on stdin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeciderRequest {
    /// Always `permission`, so a program that serves several hooks can tell them apart.
    pub hook: String,
    pub version: u32,
    pub workforce: String,
    pub item: ItemContext,
    pub session: String,
    pub agent: AgentContext,
    /// Where the session works.
    pub directory: PathBuf,
    pub tool: PermissionRequest,
    pub guidance: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemContext {
    pub id: String,
    pub title: String,
    #[serde(rename = "type")]
    pub item_type: String,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentContext {
    pub name: String,
    pub capabilities: Vec<String>,
}

/// What one decider made of a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Decided {
        permission: Permission,
        reason: Option<String>,
    },
    /// Left to the next decider, and why: it said `ask`, failed or is not sure.
    Passed { why: String },
}

/// How a request went through the chain: the decision, if a decider made one, and why each
/// decider before it passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainResult {
    pub decision: Option<Decision>,
    /// `(decider, why)` for every decider that passed.
    pub passed: Vec<(String, String)>,
}

/// Asks the agents that decide, which only the daemon can run.
pub trait AgentDeciders: Send + Sync {
    fn ask<'a>(
        &'a self,
        agent: &'a AgentName,
        turns: u32,
        request: &'a DeciderRequest,
    ) -> wf_harness::BoxFuture<'a, Outcome>;
}

/// Runs the deciders in order until one decides. Agent deciders pass on without `agents`.
pub async fn run_chain(
    deciders: &[DeciderConfig],
    request: &DeciderRequest,
    agents: Option<&dyn AgentDeciders>,
) -> ChainResult {
    let mut passed = Vec::new();
    for decider in deciders {
        let outcome = ask(decider, request, agents).await;
        match outcome {
            Outcome::Decided { permission, reason } => {
                return ChainResult {
                    decision: Some(Decision {
                        permission,
                        by: decider.name(),
                        reason,
                    }),
                    passed,
                };
            }
            Outcome::Passed { why } => passed.push((decider.name(), why)),
        }
    }
    ChainResult {
        decision: None,
        passed,
    }
}

async fn ask(
    decider: &DeciderConfig,
    request: &DeciderRequest,
    agents: Option<&dyn AgentDeciders>,
) -> Outcome {
    match decider {
        DeciderConfig::Rules {} => match rules::allows(&request.tool) {
            Some(reason) => Outcome::Decided {
                permission: Permission::Allow,
                reason: Some(reason),
            },
            None => Outcome::Passed {
                why: "no rule covers it".to_string(),
            },
        },
        DeciderConfig::Hook { command, timeout } => hook::ask(command, *timeout, request).await,
        DeciderConfig::Model {
            url,
            model,
            api_key_env,
            question,
            timeout,
        } => {
            model::ModelDecider {
                url,
                model,
                api_key_env,
                question: *question,
                timeout: *timeout,
            }
            .ask(request)
            .await
        }
        DeciderConfig::Agent { agent, turns } => match agents {
            Some(agents) => agents.ask(agent, *turns, request).await,
            None => Outcome::Passed {
                why: "an agent decides only in the daemon".to_string(),
            },
        },
    }
}

/// The answer of a hook, an agent or a model, as JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Answer {
    Allow,
    Deny,
    Ask,
}

#[derive(Debug, Deserialize)]
struct AnswerLine {
    permission: Answer,
    reason: Option<String>,
}

/// Reads `{"permission": "allow" | "deny" | "ask", "reason": ...}`.
pub(crate) fn read_answer(text: &str) -> Outcome {
    match serde_json::from_str::<AnswerLine>(text.trim()) {
        Ok(AnswerLine {
            permission: Answer::Allow,
            reason,
        }) => Outcome::Decided {
            permission: Permission::Allow,
            reason,
        },
        Ok(AnswerLine {
            permission: Answer::Deny,
            reason,
        }) => Outcome::Decided {
            permission: Permission::Deny,
            reason,
        },
        Ok(AnswerLine {
            permission: Answer::Ask,
            reason,
        }) => Outcome::Passed {
            why: reason.unwrap_or_else(|| "it asks".to_string()),
        },
        Err(error) => Outcome::Passed {
            why: format!("its answer does not parse: {error}"),
        },
    }
}

/// Answers the requests of a session whose agent asks: the deciders of the machine config in
/// their order, then the principal, through `wf inbox` and `wf session allow|deny`.
pub(crate) struct Deciding {
    pub(crate) held: Arc<Held>,
    pub(crate) workforce: WorkforceId,
    pub(crate) item: ItemId,
    pub(crate) session: SessionId,
    /// The config is read for each request, so changes apply without restarting the daemon.
    pub(crate) machine_config: Option<PathBuf>,
    /// Everything of the request but the tool call, which is filled in for each.
    pub(crate) context: DeciderRequest,
    pub(crate) agents: crate::judges::WorkforceJudges,
}

impl Permissions for Deciding {
    fn decide(&self, tool: PermissionRequest) -> wf_harness::BoxFuture<'_, Decision> {
        Box::pin(async move {
            let config = match &self.machine_config {
                Some(path) => MachineConfig::load(path),
                None => Ok(MachineConfig::default()),
            };
            let title = tool.title.clone();
            let notify = config
                .as_ref()
                .map(|config| config.notify.clone())
                .unwrap_or_default();
            let (result, broken) = match config {
                Ok(config) => {
                    let request = DeciderRequest {
                        tool,
                        guidance: config.permissions.guidance.clone(),
                        ..self.context.clone()
                    };
                    (
                        run_chain(&config.permissions.deciders, &request, Some(&self.agents)).await,
                        None,
                    )
                }
                Err(error) => (
                    ChainResult {
                        decision: None,
                        passed: Vec::new(),
                    },
                    Some(error.to_string()),
                ),
            };
            if let Some(decision) = result.decision {
                return decision;
            }
            let mut passed: Vec<String> = result
                .passed
                .into_iter()
                .map(|(decider, why)| format!("{decider}: {why}"))
                .collect();
            if let Some(broken) = broken {
                passed.push(format!("the machine config is broken: {broken}"));
            }
            let answer = self.held.hold(self.workforce, self.session, &title, passed);
            crate::notify::run_command(
                &notify,
                &Notice {
                    kind: PERMISSION_KIND,
                    item: self.item.short(),
                    title: self.context.item.title.clone(),
                    what: permission_waits(Some(&self.context.agent.name), &title, self.session),
                },
            );
            Decision {
                permission: answer.await.unwrap_or(Permission::Deny),
                by: "the principal".to_string(),
                reason: None,
            }
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn request() -> DeciderRequest {
        DeciderRequest {
            hook: "permission".into(),
            version: 1,
            workforce: "w".into(),
            item: ItemContext {
                id: "k3f7".into(),
                title: "t".into(),
                item_type: "task".into(),
                state: "in_progress".into(),
            },
            session: "s".into(),
            agent: AgentContext {
                name: "coder".into(),
                capabilities: vec![],
            },
            directory: std::env::temp_dir(),
            tool: PermissionRequest {
                kind: Some("execute".into()),
                title: "cargo test".into(),
                input: Some(serde_json::json!({"command": "cargo test"})),
            },
            guidance: None,
        }
    }

    #[test]
    fn answers_are_json() {
        assert_eq!(
            read_answer(r#"{"permission": "allow", "reason": "reads only"}"#),
            Outcome::Decided {
                permission: Permission::Allow,
                reason: Some("reads only".into())
            }
        );
        assert_eq!(
            read_answer(r#"{"permission": "deny", "later": "fields are ignored"}"#),
            Outcome::Decided {
                permission: Permission::Deny,
                reason: None
            }
        );
        assert!(matches!(
            read_answer(r#"{"permission": "ask"}"#),
            Outcome::Passed { .. }
        ));
        assert!(matches!(read_answer("allow"), Outcome::Passed { .. }));
    }
}

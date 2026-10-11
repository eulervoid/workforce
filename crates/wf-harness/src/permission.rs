//! What an agent asks permission for, and how the answer is given.

use agent_client_protocol::schema::v1::{
    PermissionOption, PermissionOptionKind, RequestPermissionOutcome, SelectedPermissionOutcome,
    ToolCallUpdateFields,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How a permission request is answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    Allow,
    Deny,
}

impl PermissionMode {
    pub(crate) fn answer(self, options: &[PermissionOption]) -> RequestPermissionOutcome {
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

/// The tool call an agent wants to make, as the harness describes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionRequest {
    /// What sort of call it is, such as `execute`, `edit` or `fetch`, when the harness says.
    pub kind: Option<String>,
    pub title: String,
    /// The arguments of the call as the harness passes them, such as `{"command": "cargo test"}`.
    pub input: Option<Value>,
}

impl PermissionRequest {
    pub(crate) fn of(fields: &ToolCallUpdateFields) -> Self {
        PermissionRequest {
            kind: fields
                .kind
                .as_ref()
                .and_then(|kind| serde_json::to_value(kind).ok())
                .and_then(|kind| kind.as_str().map(str::to_string)),
            title: fields
                .title
                .clone()
                .unwrap_or_else(|| "a tool call".to_string()),
            input: fields.raw_input.clone(),
        }
    }

    /// The shell command the call runs, when it runs one.
    pub fn command(&self) -> Option<&str> {
        self.input.as_ref()?.get("command")?.as_str()
    }
}

/// The answer to a permission request, who gave it and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub permission: PermissionMode,
    /// Which decider answered: `rules`, a hook, a model, an agent, a person, or the agent's
    /// fixed permissions.
    pub by: String,
    pub reason: Option<String>,
}

/// Decides what a request of the agent for permission is answered with. A person may be asked,
/// so it may take a long time; the agent waits meanwhile.
pub trait Permissions: Send + Sync + 'static {
    fn decide(&self, request: PermissionRequest) -> agent_client_protocol::BoxFuture<'_, Decision>;
}

impl Permissions for PermissionMode {
    fn decide(&self, _request: PermissionRequest) -> agent_client_protocol::BoxFuture<'_, Decision> {
        let decision = Decision {
            permission: *self,
            by: "the agent's permissions".to_string(),
            reason: None,
        };
        Box::pin(async move { decision })
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
    fn a_request_carries_the_kind_title_and_input_of_the_call() {
        let fields = ToolCallUpdateFields::new()
            .kind(agent_client_protocol::schema::v1::ToolKind::Execute)
            .title("Terminal".to_string())
            .raw_input(serde_json::json!({"command": "wf item show k3f7"}));
        let request = PermissionRequest::of(&fields);
        assert_eq!(request.kind.as_deref(), Some("execute"));
        assert_eq!(request.title, "Terminal");
        assert_eq!(request.command(), Some("wf item show k3f7"));
    }
}

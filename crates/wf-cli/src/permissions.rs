//! `wf permissions test`: runs the deciders of this machine on a request, without a session.

use std::io::{IsTerminal, Read};
use std::path::Path;

use anyhow::Context;
use serde::Serialize;
use wf_daemon::decide::{AgentContext, DeciderRequest, ItemContext, run_chain};
use wf_daemon::machine_config::MachineConfig;
use wf_harness::{Decider, Permission, PermissionRequest};

use crate::cli::PermissionsCommand;
use crate::commands::emit;

#[derive(Serialize)]
struct ChainView {
    config: String,
    /// `decider: why` for each decider that passed it on.
    passed: Vec<String>,
    /// `allow` or `deny`, or `None` when the principal would be asked.
    permission: Option<Permission>,
    by: Option<Decider>,
    reason: Option<String>,
}

pub fn permissions(json: bool, command: PermissionsCommand, cwd: &Path) -> anyhow::Result<()> {
    match command {
        PermissionsCommand::Test { command } => {
            let request = match command {
                Some(command) => request_for(&command, cwd),
                None => read_request()?,
            };
            let path = MachineConfig::path_from_env().context("HOME is not set")?;
            let config = MachineConfig::load(&path)?;
            let request = DeciderRequest {
                guidance: config.permissions.guidance.clone(),
                ..request
            };
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let result = runtime.block_on(run_chain(&config.permissions.deciders, &request, None));
            let view = ChainView {
                config: path.display().to_string(),
                passed: result
                    .passed
                    .into_iter()
                    .map(|(decider, why)| format!("{decider}: {why}"))
                    .collect(),
                permission: result.decision.as_ref().map(|decision| decision.permission),
                by: result.decision.as_ref().map(|decision| decision.by.clone()),
                reason: result.decision.and_then(|decision| decision.reason),
            };
            emit(json, &view, render)
        }
    }
}

fn render(view: &ChainView) -> String {
    let mut out = String::new();
    for why in &view.passed {
        out.push_str(&format!("passed by {why}\n"));
    }
    let verdict = match (view.permission, &view.by) {
        (Some(Permission::Allow), Some(by)) => format!("allowed by {by}"),
        (Some(Permission::Deny), Some(by)) => format!("denied by {by}"),
        (Some(_), None) | (None, _) => "the principal would be asked".to_string(),
    };
    match &view.reason {
        Some(reason) => out.push_str(&format!("{verdict}: {reason}\n")),
        None => out.push_str(&format!("{verdict}\n")),
    }
    out
}

/// A request to run `command`, made up around it as if a session in `cwd` asked.
fn request_for(command: &str, cwd: &Path) -> DeciderRequest {
    DeciderRequest {
        header: Default::default(),
        workforce: String::new(),
        item: ItemContext {
            id: String::new(),
            title: "a test of the permission chain".to_string(),
            item_type: "task".to_string(),
            state: "active".to_string(),
        },
        session: String::new(),
        agent: AgentContext {
            name: "tester".to_string(),
            capabilities: Vec::new(),
        },
        directory: cwd.to_path_buf(),
        tool: PermissionRequest {
            kind: Some(wf_harness::ToolKind::Execute),
            title: command.to_string(),
            input: Some(serde_json::json!({ "command": command })),
        },
        guidance: None,
    }
}

fn read_request() -> anyhow::Result<DeciderRequest> {
    let mut stdin = std::io::stdin();
    if stdin.is_terminal() {
        anyhow::bail!("give a request as JSON on stdin, or a command with --command");
    }
    let mut text = String::new();
    stdin.read_to_string(&mut text)?;
    serde_json::from_str(&text).context("the request on stdin is not a permission request")
}

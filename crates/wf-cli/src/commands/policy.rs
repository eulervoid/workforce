//! `wf policy`: showing and changing the policy of the workforce.

use anyhow::Context;
use wf_core::event::{EventBody, PolicySet};
use wf_core::identity::Signed;
use wf_core::policy::Policy;

use super::{done, edit_in_editor, emit};
use crate::cli::*;
use crate::workspace::Workspace;

pub(super) fn policy(
    json: bool,
    command: PolicyCommand,
    workspace: &mut Workspace,
) -> anyhow::Result<()> {
    let current = workspace
        .state()
        .policy()
        .expect("a workforce has a policy")
        .source
        .clone();
    if let Some(error) = workspace.state().unreadable_policy() {
        eprintln!(
            "this version of wf cannot read the policy set last ({}); this is the last one it could read",
            wf_core::error_chain(&error)
        );
    }
    match command {
        PolicyCommand::Show => emit(json, &current, |source| source.clone()),
        PolicyCommand::Apply { file } => {
            let source = std::fs::read_to_string(&file)
                .with_context(|| format!("reading {}", file.display()))?;
            apply_policy(json, workspace, source)
        }
        PolicyCommand::Edit => {
            let edited = edit_in_editor(&current, ".toml")?;
            if edited == current {
                return emit(json, &"policy unchanged", |message| format!("{message}\n"));
            }
            apply_policy(json, workspace, edited)
        }
    }
}

fn apply_policy(json: bool, workspace: &mut Workspace, source: String) -> anyhow::Result<()> {
    Policy::parse(&source)?;
    let principal = workspace.principal_signer()?;
    let workforce = workspace
        .state()
        .workforce_id()
        .expect("a workforce has an id");
    let statement = Signed::sign(&principal, Some(workforce), PolicySet { policy: source })?;
    let event = workspace.author(EventBody::PolicySet(statement))?;
    done(json, "policy replaced".to_string(), &event, None)
}

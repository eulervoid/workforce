//! `wf policy`: showing and changing the policy of the workforce.

use anyhow::Context;
use wf_core::event::{BoundPolicySet, EventBody};
use wf_core::identity::Signed;
use wf_core::policy::Policy;

use super::{done, edit_until_valid, emit};
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
            let edited = edit_until_valid(&current, ".toml", |source| {
                Policy::parse_new(source)?;
                Ok(source.to_string())
            })?;
            if edited == current {
                return emit(json, &"policy unchanged", |message| format!("{message}\n"));
            }
            apply_policy(json, workspace, edited)
        }
    }
}

fn apply_policy(json: bool, workspace: &mut Workspace, source: String) -> anyhow::Result<()> {
    Policy::parse_new(&source)?;
    let principal = workspace.principal_signer()?;
    let workforce = workspace
        .state()
        .workforce_id()
        .expect("a workforce has an id");
    // The signature binds the policy to the event that carries it.
    let hlc = workspace.state().next_hlc(wf_store::now_ms())?;
    let statement = Signed::sign(
        &principal,
        Some(workforce),
        BoundPolicySet {
            policy: source,
            hlc,
        },
    )?;
    let event = workspace.author(EventBody::BoundPolicySet(statement))?;
    done(json, "policy replaced".to_string(), &event, None)
}

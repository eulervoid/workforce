//! `wf agent`: the agents of the principal and where they run.

use anyhow::{Context, anyhow, bail};
use wf_core::autonomy::Autonomy;
use wf_core::event::{AgentDefined, AgentRemoved, EventBody};
use wf_core::name::AgentName;
use wf_core::resolve::MentionTarget;
use wf_core::validate::OneOf;

use super::{done, emit};
use crate::cli::*;
use crate::render;
use crate::view::{self};
use crate::workspace::Workspace;
use wf_core::autonomy::StartMode;

pub(super) fn agent(
    json: bool,
    command: AgentCommand,
    workspace: &mut Workspace,
) -> anyhow::Result<()> {
    let principal = workspace.principal();
    match command {
        AgentCommand::List => {
            let agents = view::agent_views(workspace.state());
            emit(json, &agents, |agents| render::agents(agents))
        }
        AgentCommand::Show { agent } => {
            let state = workspace.state();
            let id = state
                .agents()
                .map(|(id, _)| id)
                .find(|id| {
                    (id.principal == principal && id.name.as_str() == agent)
                        || view::mention_label(state, &MentionTarget::Agent((*id).clone())) == agent
                })
                .cloned()
                .ok_or_else(|| {
                    let all = state.agents().map(|(id, _)| {
                        view::mention_label(state, &MentionTarget::Agent(id.clone()))
                    });
                    anyhow!("there is no agent `{agent}`; {}", OneOf::new(all))
                })?;
            let events = workspace.events()?;
            let detail = view::agent_detail(
                workspace.state(),
                &id,
                &events,
                Some(workspace.device_key()),
            )
            .context("a listed agent is defined")?;
            emit(json, &detail, render::agent_detail)
        }
        AgentCommand::Add(args) => {
            let name = match args.name {
                Some(name) => name,
                None => {
                    let taken: Vec<AgentName> = workspace
                        .state()
                        .agents()
                        .map(|(id, _)| id.name.clone())
                        .collect();
                    crate::agent_names::free(&taken)
                }
            };
            if workspace.state().agent(&name, principal).is_some() {
                bail!("agent `{name}` already exists; use `wf agent edit`");
            }
            let warning =
                crate::harness::check_startable(&args.harness, args.harness_unknown_here)?;
            let autonomy = args.autonomy.applied_to(Autonomy::default());
            let start = autonomy.start;
            let event = workspace.author(EventBody::AgentDefined(AgentDefined {
                name: name.clone(),
                harness: args.harness,
                model: args.model,
                capabilities: args.capabilities,
                lanes: args.lanes,
                autonomy,
                instructions: args.instructions.and_then(instructions),
            }))?;
            let message = format!("defined agent {name}");
            let message = warned(message, warning);
            let message = hosting_note(workspace, &name, start, message)?;
            done(json, message, &event, None)
        }
        AgentCommand::Remove { name } => {
            let state = workspace.state();
            if state.agent(&name, principal).is_none() {
                let yours = state
                    .agents()
                    .filter(|(id, _)| id.principal == principal)
                    .map(|(id, _)| id.name.clone());
                bail!("there is no agent `{name}`; {}", OneOf::new(yours));
            }
            let live = state
                .sessions()
                .filter(|session| {
                    session.is_live()
                        && session.agent().principal == principal
                        && session.agent().name == name
                })
                .count();
            if live > 0 {
                bail!("{name} has {live} live session(s); stop them first with `wf session stop`");
            }
            let event =
                workspace.author(EventBody::AgentRemoved(AgentRemoved { name: name.clone() }))?;
            done(json, format!("removed agent {name}"), &event, None)
        }
        AgentCommand::Edit(args) => {
            let current = workspace
                .state()
                .agent(&args.name, principal)
                .ok_or_else(|| {
                    let yours = workspace
                        .state()
                        .agents()
                        .filter(|(id, _)| id.principal == principal)
                        .map(|(id, _)| id.name.clone());
                    anyhow!("there is no agent `{}`; {}", args.name, OneOf::new(yours))
                })?
                .clone();
            let warning = match &args.harness {
                Some(harness) => {
                    crate::harness::check_startable(harness, args.harness_unknown_here)?
                }
                None => None,
            };
            let autonomy = args.autonomy.applied_to(current.autonomy);
            let start = autonomy.start;
            let event = workspace.author(EventBody::AgentDefined(AgentDefined {
                name: args.name.clone(),
                harness: args.harness.unwrap_or(current.harness),
                model: args.model.or(current.model),
                capabilities: if args.capabilities.is_empty() {
                    current.capabilities
                } else {
                    args.capabilities
                },
                lanes: if args.lanes.is_empty() {
                    current.lanes
                } else {
                    args.lanes
                },
                autonomy,
                instructions: match args.instructions {
                    Some(text) => instructions(text),
                    None => current.instructions,
                },
            }))?;
            let message = format!("updated agent {}", args.name);
            let message = warned(message, warning);
            let message = hosting_note(workspace, &args.name, start, message)?;
            done(json, message, &event, None)
        }
    }
}

/// `message`, followed by the warning about the harness when there is one.
fn warned(message: String, warning: Option<String>) -> String {
    match warning {
        Some(warning) => format!("{message}\nwarning: {warning}"),
        None => message,
    }
}

/// The instructions of an agent as given, or none for an empty text.
fn instructions(text: String) -> Option<String> {
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// An agent that starts automatically is of no use until a device runs it, so this device does,
/// and `message` says so.
fn hosting_note(
    workspace: &mut Workspace,
    agent: &AgentName,
    start: StartMode,
    message: String,
) -> anyhow::Result<String> {
    match start {
        StartMode::Auto if crate::devices::host_here(workspace, agent)? => Ok(format!(
            "{message}; this device runs it, `wf device edit --agent` changes that"
        )),
        StartMode::Auto | StartMode::Manual => Ok(message),
    }
}

//! `wf device` and `wf members`.

use std::collections::BTreeMap;

use anyhow::{Context, bail};
use jiff::Timestamp;
use serde::Serialize;
use wf_core::event::{DeviceProfile, EventBody};
use wf_core::identity::{DeviceKey, Role};
use wf_core::name::AgentName;
use wf_core::state::State;

use crate::cli::{DeviceCommand, DeviceEditArgs};
use crate::commands::emit;
use crate::network::parse_agents;
use crate::render;
use crate::workspace::Workspace;

/// When each device was last heard from, in milliseconds since the epoch. Local to this machine
/// and not part of the workforce state.
pub type LastSeen = BTreeMap<DeviceKey, u64>;

#[derive(Serialize)]
pub struct DeviceView {
    pub device: String,
    pub name: Option<String>,
    pub principal: String,
    pub hardware: Option<String>,
    pub max_sessions: Option<u32>,
    pub agents: BTreeMap<String, u32>,
    pub last_seen: Option<Timestamp>,
    pub this_device: bool,
    pub flags: Vec<String>,
}

pub fn device_views(state: &State, own: DeviceKey, last_seen: &LastSeen) -> Vec<DeviceView> {
    let mut views: Vec<DeviceView> = state
        .devices()
        .map(|info| {
            let mut flags = Vec::new();
            if let Some(seq) = info.equivocated {
                flags.push(format!("equivocated at seq {seq}"));
            }
            if info.invite_redeemed_twice {
                flags.push("invite redeemed twice".to_string());
            }
            DeviceView {
                device: info.device.to_string(),
                name: info.profile.map(|profile| profile.name.clone()),
                principal: state.actor_label(&wf_core::identity::Actor::Principal(info.principal)),
                hardware: info.profile.map(|profile| profile.hardware.clone()),
                max_sessions: info.profile.map(|profile| profile.max_sessions),
                agents: info
                    .profile
                    .map(|profile| {
                        profile
                            .agents
                            .iter()
                            .map(|(agent, sessions)| (agent.to_string(), *sessions))
                            .collect()
                    })
                    .unwrap_or_default(),
                last_seen: last_seen
                    .get(&info.device)
                    .map(|millis| crate::view::timestamp_ms(*millis)),
                this_device: info.device == own,
                flags,
            }
        })
        .collect();
    views.sort_by(|a, b| {
        (&a.principal, &a.name, &a.device).cmp(&(&b.principal, &b.name, &b.device))
    });
    views
}

#[derive(Serialize)]
pub struct MemberView {
    pub handle: String,
    pub principal: String,
    pub role: Role,
    pub devices: Vec<String>,
}

pub fn member_views(state: &State) -> Vec<MemberView> {
    state
        .members()
        .map(|(principal, member)| MemberView {
            handle: format!("@{}", member.handle),
            principal: principal.to_string(),
            role: member.role,
            devices: member
                .devices
                .iter()
                .map(|device| {
                    state
                        .devices()
                        .find(|info| info.device == *device)
                        .and_then(|info| info.profile)
                        .map_or_else(|| device.fmt_short(), |profile| profile.name.clone())
                })
                .collect(),
        })
        .collect()
}

pub fn members(json: bool, workspace: &Workspace) -> anyhow::Result<()> {
    emit(json, &member_views(workspace.state()), |members| {
        render::members(members)
    })
}

pub fn device(json: bool, command: DeviceCommand, workspace: &mut Workspace) -> anyhow::Result<()> {
    match command {
        DeviceCommand::List => {
            let views = device_views(
                workspace.state(),
                workspace.device_key(),
                &workspace.last_seen()?,
            );
            emit(json, &views, |views| render::devices(views))
        }
        DeviceCommand::Edit(args) => edit(json, workspace, args),
        DeviceCommand::AllowedSigners => {
            let lines = allowed_signers(workspace.state())?;
            emit(json, &lines, |lines| {
                lines.iter().map(|line| format!("{line}\n")).collect()
            })
        }
    }
}

/// One allowed-signers line per bound device, for `<handle>@<device name>`.
fn allowed_signers(state: &State) -> anyhow::Result<Vec<String>> {
    state
        .devices()
        .map(|info| {
            let handle = state
                .member(&info.principal)
                .map_or_else(|| "unknown".to_string(), |member| member.handle.to_string());
            let name: String = info
                .profile
                .map_or_else(|| info.device.fmt_short(), |profile| profile.name.clone())
                .chars()
                .map(|character| {
                    if character.is_alphanumeric() || matches!(character, '-' | '_' | '.') {
                        character
                    } else {
                        '-'
                    }
                })
                .collect();
            Ok(wf_vcs::signing::allowed_signer(
                &format!("{handle}@{name}"),
                info.device,
            )?)
        })
        .collect()
}

/// Lets this device run the agent (one session at a time) unless it already says how many. Returns
/// whether the profile changed. A device without a profile is left alone.
pub fn host_here(workspace: &mut Workspace, agent: &AgentName) -> anyhow::Result<bool> {
    let own = workspace.device_key();
    let Some(mut profile) = workspace
        .state()
        .devices()
        .find(|info| info.device == own)
        .and_then(|info| info.profile.cloned())
    else {
        return Ok(false);
    };
    if profile.agents.contains_key(agent) {
        return Ok(false);
    }
    profile.agents.insert(agent.clone(), 1);
    workspace.author(EventBody::DeviceProfile(profile))?;
    Ok(true)
}

fn edit(json: bool, workspace: &mut Workspace, args: DeviceEditArgs) -> anyhow::Result<()> {
    let own = workspace.device_key();
    let current = workspace
        .state()
        .devices()
        .find(|info| info.device == own)
        .context("this device is not bound")?
        .profile
        .cloned();
    let mut profile = current.unwrap_or_else(|| DeviceProfile {
        name: own.fmt_short(),
        hardware: String::new(),
        agents: BTreeMap::new(),
        max_sessions: 1,
    });
    let unchanged = args.name.is_none()
        && args.hardware.is_none()
        && args.max_sessions.is_none()
        && args.agents.is_empty();
    if unchanged {
        bail!("nothing to change; pass --name, --hardware, --max-sessions or --agent");
    }
    if let Some(name) = args.name {
        if name.trim().is_empty() {
            bail!("the device name must not be empty");
        }
        profile.name = name;
    }
    if let Some(hardware) = args.hardware {
        profile.hardware = crate::network::hardware(&hardware)?;
    }
    if let Some(max_sessions) = args.max_sessions {
        profile.max_sessions = max_sessions;
    }
    if !args.agents.is_empty() {
        profile.agents = parse_agents(&args.agents)?;
    }
    let message = format!("updated device {}", profile.name);
    let event = workspace.author(EventBody::DeviceProfile(profile))?;
    emit(
        json,
        &crate::commands::Done {
            message,
            event: Some(wf_core::id::HashId::to_base32(&event.id())),
            session: None,
            item: None,
        },
        |done| format!("{}\n", done.message),
    )
}

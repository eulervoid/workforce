//! `wf fork`: a new workforce that starts where this one is, without what was lost.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, anyhow, bail};
use serde::Serialize;
use wf_core::event::{EventBody, ForkAnnounced, ForkPoint, Genesis};
use wf_core::id::WorkforceId;
use wf_core::identity::{
    Actor, DeviceKey, MemberSeed, PrincipalKey, PrincipalSigner, Role, Signed,
};
use wf_core::state::State;
use wf_net::{Node, StorageHandle};
use wf_store::{RedbStore, Replica};

use crate::cli::ForkArgs;
use crate::commands::emit;
use crate::data_dir::DataDir;
use crate::network::{network_for, sync_devices};
use crate::view::device_label;
use crate::workspace::{self, Replicated, WORKFORCE_FILE, WorkforceFile, Workspace};

/// The directory of a fork that is being made, removed again unless the fork is completed.
struct Unfinished(Option<PathBuf>);

impl Unfinished {
    fn new(directory: PathBuf) -> Self {
        Self(Some(directory))
    }

    fn keep(&mut self) {
        self.0 = None;
    }
}

impl Drop for Unfinished {
    fn drop(&mut self) {
        if let Some(directory) = &self.0 {
            let _ = std::fs::remove_dir_all(directory);
        }
    }
}

/// What `--exclude` names.
#[derive(Debug, PartialEq, Eq)]
enum Exclusion {
    Device(DeviceKey),
    Member(PrincipalKey),
}

fn resolve_exclusion(state: &State, text: &str) -> anyhow::Result<Exclusion> {
    if let Some(handle) = text.strip_prefix('@') {
        let matches: Vec<PrincipalKey> = state
            .members()
            .filter(|(_, member)| member.handle.as_str() == handle)
            .map(|(principal, _)| *principal)
            .collect();
        return match matches.as_slice() {
            [principal] => Ok(Exclusion::Member(*principal)),
            _ => bail!("there is no member @{handle}"),
        };
    }
    let wanted = text.to_lowercase();
    let matches: Vec<DeviceKey> = state
        .devices()
        .filter(|info| {
            info.profile
                .is_some_and(|profile| profile.name.to_lowercase() == wanted)
                || (wanted.len() >= wf_core::id::MIN_PREFIX_LEN
                    && info.device.to_string().starts_with(&wanted))
        })
        .map(|info| info.device)
        .collect();
    match matches.as_slice() {
        [device] => Ok(Exclusion::Device(*device)),
        [] => bail!("there is no device `{text}`; see `wf device list`"),
        several => bail!(
            "`{text}` matches {} devices; use more of its key",
            several.len()
        ),
    }
}

#[derive(Serialize)]
pub struct ForkView {
    pub workforce: String,
    pub parent: String,
    pub name: String,
    pub excluded_devices: Vec<String>,
    pub excluded_members: Vec<String>,
    pub members: Vec<String>,
    pub announced: bool,
    pub new_principal: bool,
    pub history_events: usize,
}

pub fn fork(
    json: bool,
    workspace: &mut Workspace,
    args: ForkArgs,
    cwd: &Path,
) -> anyhow::Result<()> {
    let state = workspace.state().clone();
    let parent = state
        .workforce_id()
        .context("the workforce has no genesis")?;
    let own_device = workspace.device_key();
    let own_principal = state
        .principal_of(&own_device)
        .context("this device is not bound")?;
    let signer = workspace.principal_signer()?;
    if signer.key() != own_principal {
        bail!("the principal key on this machine belongs to someone else");
    }

    let mut excluded_devices = BTreeSet::new();
    let mut excluded_members = BTreeSet::new();
    for text in &args.exclude {
        match resolve_exclusion(&state, text)? {
            Exclusion::Device(device) => excluded_devices.insert(device),
            Exclusion::Member(principal) => excluded_members.insert(principal),
        };
    }
    if excluded_members.contains(&own_principal) || excluded_devices.contains(&own_device) {
        bail!("a fork cannot leave out the device or member that makes it");
    }

    let founder_key = if args.new_principal {
        PrincipalSigner::generate(rand::random())
    } else {
        signer.clone()
    };
    let handle = state
        .member(&own_principal)
        .context("the founder is not a member")?
        .handle
        .clone();
    let seed_devices = |principal: &PrincipalKey| -> Vec<DeviceKey> {
        state
            .member(principal)
            .map(|member| {
                member
                    .devices
                    .iter()
                    .filter(|device| !excluded_devices.contains(device))
                    .copied()
                    .collect()
            })
            .unwrap_or_default()
    };
    let founder = MemberSeed {
        principal: founder_key.key(),
        handle,
        devices: seed_devices(&own_principal),
        previous: if args.new_principal {
            vec![own_principal]
        } else {
            Vec::new()
        },
    };
    let members: Vec<MemberSeed> = state
        .members()
        .filter(|(principal, _)| {
            **principal != own_principal && !excluded_members.contains(principal)
        })
        .map(|(principal, member)| MemberSeed {
            principal: *principal,
            handle: member.handle.clone(),
            devices: seed_devices(principal),
            previous: Vec::new(),
        })
        .collect();
    let kept: Vec<PrincipalKey> = std::iter::once(founder.principal)
        .chain(members.iter().map(|seed| seed.principal))
        .collect();
    let name = args
        .name
        .unwrap_or_else(|| format!("{} (fork)", state.name().unwrap_or_default()));
    let genesis = Genesis {
        name: name.clone(),
        founder,
        members,
        policy: state
            .policy()
            .context("the workforce has no policy")?
            .source
            .clone(),
        forked_from: Some(ForkPoint {
            workforce: parent,
            heads: state.heads().iter().copied().collect(),
        }),
    };
    let device = workspace
        .data
        .device(false)?
        .context("no device key on this machine")?;
    let genesis_event = state
        .fork_genesis_builder(
            own_device,
            Actor::Principal(founder_key.key()),
            workspace::now_ms(),
        )
        .context("the workforce has no genesis")?
        .sign(
            &device,
            EventBody::Genesis(Signed::sign(&founder_key, None, genesis)?),
        )?;
    let child: WorkforceId = genesis_event.id().into();
    let mut unfinished = Unfinished::new(workspace.data.workforce(&child));
    workspace.data.store_principal(&child, &founder_key)?;

    let history_events = state.event_count();
    workspace.create_fork(genesis_event)?;

    let announce = !args.new_principal
        && state
            .member(&own_principal)
            .is_some_and(|member| member.role == Role::Owner);
    if announce {
        workspace.author(EventBody::ForkAnnounced(Signed::sign(
            &signer,
            Some(parent),
            ForkAnnounced {
                to: child,
                members: kept.clone(),
            },
        )?))?;
    }
    let file = toml::to_string(&WorkforceFile { id: child })?;
    std::fs::write(find_file(cwd)?, file)?;
    unfinished.keep();

    let describe = |device: &DeviceKey| device_label(&state, *device);
    emit(
        json,
        &ForkView {
            workforce: child.to_string(),
            parent: parent.to_string(),
            name,
            excluded_devices: excluded_devices.iter().map(describe).collect(),
            excluded_members: excluded_members
                .iter()
                .map(|principal| state.actor_label(&Actor::Principal(*principal)))
                .collect(),
            members: kept
                .iter()
                .map(|principal| {
                    state.actor_label(&Actor::Principal(
                        if args.new_principal && *principal == founder_key.key() {
                            own_principal
                        } else {
                            *principal
                        },
                    ))
                })
                .collect(),
            announced: announce,
            new_principal: args.new_principal,
            history_events,
        },
        |view| {
            let mut out = format!(
                "forked \"{}\" ({}) from {} with {} events of history\n",
                view.name, view.workforce, view.parent, view.history_events
            );
            for device in &view.excluded_devices {
                out.push_str(&format!("left out device {device}\n"));
            }
            for member in &view.excluded_members {
                out.push_str(&format!("left out member {member}\n"));
            }
            if view.new_principal {
                out.push_str(
                    "founded with a new principal key; the old workforce keeps the old one\n",
                );
            }
            out.push_str(&format!(
                "this directory now uses the fork; on each other device run `wf fork accept {}`\n",
                view.workforce
            ));
            if !view.announced {
                out.push_str("the fork is not announced in the old workforce\n");
            }
            out
        },
    )
}

fn find_file(cwd: &Path) -> anyhow::Result<std::path::PathBuf> {
    Ok(workspace::find_workforce(cwd)?.0)
}

#[derive(Serialize)]
pub struct AcceptView {
    pub workforce: String,
    pub events: usize,
}

pub fn accept(
    json: bool,
    data: DataDir,
    cwd: &Path,
    id: &str,
    relay: Option<&str>,
) -> anyhow::Result<()> {
    let child: WorkforceId = id
        .parse()
        .map_err(|_| anyhow!("`{id}` is not a workforce id"))?;
    let network = network_for(relay)?;
    let workspace = Workspace::open(data.clone(), cwd, false)?;
    let known = workspace
        .state()
        .announcements()
        .any(|announcement| announcement.to == child);
    if !known {
        eprintln!("note: this workforce has no announcement of a fork to {child}");
    }
    let own = workspace.device_key();
    let bootstrap = bootstrap_devices(workspace.state(), child, own);
    let (replicated, _) = workspace.into_replicated();

    // Whoever keeps the log of this machine (the daemon, or this process) takes the fork in. The
    // history comes from the workforce it was forked from when this machine has it.
    let events = match replicated {
        Replicated::Daemon(mut client) => {
            let results = client.follow(child, bootstrap)?;
            if results.iter().all(|result| !result.reached()) {
                bail!("could not reach a device that has the fork");
            }
            let state = client.state(child)?;
            check_member(&state, own, child)?;
            state.event_count()
        }
        Replicated::Local(parent, device) => {
            let store = workspace::store_path(&data, &child)?;
            let mut replica = Replica::open(RedbStore::open(&store)?, child)?;
            replica.set_history_source(parent);
            let storage = StorageHandle::spawn(replica, move || {
                Replica::open(RedbStore::open(&store)?, child)
            });
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async {
                let node: Arc<Node<_>> = Arc::new(Node::start(&device, network).await?);
                node.serve(child, storage.clone());
                let results = sync_devices(&node, child, bootstrap, None).await;
                if results.iter().all(|result| !result.reached()) {
                    bail!("could not reach a device that has the fork");
                }
                anyhow::Ok(())
            })?;
            let state = storage.with_blocking(|replica| replica.state().clone())?;
            check_member(&state, own, child)?;
            state.event_count()
        }
    };
    let file = toml::to_string(&WorkforceFile { id: child })?;
    std::fs::write(workspace::find_workforce(cwd)?.0, file)?;
    emit(
        json,
        &AcceptView {
            workforce: child.to_string(),
            events,
        },
        |view| {
            format!(
                "following workforce {} ({} events); {WORKFORCE_FILE} updated\n",
                view.workforce, view.events
            )
        },
    )
}

/// Where to look for a fork: the devices of the members the announcement says it keeps, so a
/// device that was left out is not asked. Without an announcement, every other device.
fn bootstrap_devices(state: &State, child: WorkforceId, own: DeviceKey) -> Vec<DeviceKey> {
    let kept = state
        .announcements()
        .find(|announcement| announcement.to == child)
        .map(|announcement| &announcement.members);
    state
        .devices()
        .filter(|info| kept.is_none_or(|members| members.contains(&info.principal)))
        .map(|info| info.device)
        .filter(|device| *device != own)
        .collect()
}

fn check_member(state: &State, device: DeviceKey, child: WorkforceId) -> anyhow::Result<()> {
    if state.workforce_id() != Some(child) {
        bail!("the devices that answered do not have the fork yet; try again later");
    }
    if state.principal_of(&device).is_none() {
        bail!("this device is not part of the fork");
    }
    if state.history_incomplete() {
        bail!(
            "the history of the fork is still arriving; run `wf sync` and then `wf fork accept` again"
        );
    }
    Ok(())
}

#[derive(Serialize)]
pub struct ForkListView {
    pub workforce: String,
    pub parent: Option<ParentView>,
    pub forks: Vec<ForkedView>,
}

#[derive(Serialize)]
pub struct ParentView {
    pub workforce: String,
    pub heads: usize,
}

#[derive(Serialize)]
pub struct ForkedView {
    pub workforce: String,
    pub announced_by: String,
    pub members: Vec<String>,
}

pub fn list(json: bool, workspace: &Workspace) -> anyhow::Result<()> {
    let state = workspace.state();
    let view = ForkListView {
        workforce: state
            .workforce_id()
            .map(|id| id.to_string())
            .unwrap_or_default(),
        parent: state.fork_point().map(|(parent, heads)| ParentView {
            workforce: parent.to_string(),
            heads: heads.len(),
        }),
        forks: state
            .announcements()
            .map(|announcement| ForkedView {
                workforce: announcement.to.to_string(),
                announced_by: state.actor_label(&Actor::Principal(announcement.by)),
                members: announcement
                    .members
                    .iter()
                    .map(|principal| state.actor_label(&Actor::Principal(*principal)))
                    .collect(),
            })
            .collect(),
    };
    emit(json, &view, |view| {
        let mut out = String::new();
        match &view.parent {
            Some(parent) => out.push_str(&format!(
                "forked from {} at {} heads\n",
                parent.workforce, parent.heads
            )),
            None => out.push_str("not a fork\n"),
        }
        if view.forks.is_empty() {
            out.push_str("no forks announced\n");
        }
        for fork in &view.forks {
            out.push_str(&format!(
                "fork {} announced by {}, keeping {}\n",
                fork.workforce,
                fork.announced_by,
                fork.members.join(", ")
            ));
        }
        out
    })
}

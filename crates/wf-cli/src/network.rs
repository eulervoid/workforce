//! Commands that talk to other devices.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use serde::Serialize;
use wf_core::event::{DeviceBound, DeviceProfile, EventBody, InviteCreated, InviteKind};
use wf_core::identity::{Actor, DeviceKey, DeviceSigner, InviteSigner, Signed};
use wf_core::invite::InviteCode;
use wf_core::name::AgentName;
use wf_daemon::{PeerOutcome, PeerSync, peers_of, sync_all};

pub(crate) async fn sync_devices(
    node: &Arc<Node<SharedReplica<RedbStore>>>,
    workforce: wf_core::id::WorkforceId,
    devices: Vec<DeviceKey>,
    presence: Option<&wf_daemon::Presence>,
) -> Vec<PeerSync> {
    let peers = devices
        .into_iter()
        .map(|device| (device, device.fmt_short()))
        .collect();
    sync_all(node, workforce, peers, presence).await
}
use wf_net::{NetError, Network, Node, SharedReplica};
use wf_store::{RedbStore, Replica};

use crate::cli::{DaemonArgs, InviteDeviceArgs, JoinArgs, SyncArgs};
use crate::commands::emit;
use crate::interactive::Mode;
use crate::keys::DataDir;
use crate::view::{device_label, timestamp_now};
use crate::workspace::{self, Replicated, WORKFORCE_FILE, WorkforceFile, Workspace};

#[derive(Serialize)]
pub struct PeerView {
    pub device: String,
    pub reached: bool,
    pub sent: usize,
    pub received: usize,
    pub rejected: usize,
    pub error: Option<String>,
}

impl From<&PeerSync> for PeerView {
    fn from(peer: &PeerSync) -> Self {
        match &peer.outcome {
            PeerOutcome::Synced {
                sent,
                received,
                rejected,
            } => Self {
                device: peer.label.clone(),
                reached: true,
                sent: *sent,
                received: *received,
                rejected: *rejected,
                error: None,
            },
            PeerOutcome::Unreachable(error) => Self {
                device: peer.label.clone(),
                reached: false,
                sent: 0,
                received: 0,
                rejected: 0,
                error: Some(error.clone()),
            },
        }
    }
}

#[derive(Serialize)]
pub struct SyncView {
    pub device: String,
    pub peers: Vec<PeerView>,
    pub events: usize,
    pub serving: bool,
}

fn render(view: &SyncView) -> String {
    let mut out = String::new();
    if view.peers.is_empty() {
        out.push_str("no other devices to sync with\n");
    }
    for peer in &view.peers {
        match &peer.error {
            None => out.push_str(&format!(
                "{}: sent {}, received {} events{}\n",
                peer.device,
                peer.sent,
                peer.received,
                if peer.rejected > 0 {
                    format!(" ({} rejected)", peer.rejected)
                } else {
                    String::new()
                }
            )),
            Some(error) => out.push_str(&format!("{}: unreachable ({error})\n", peer.device)),
        }
    }
    out.push_str(&format!("{} events known\n", view.events));
    out
}

pub fn network_for(relay: Option<&str>) -> anyhow::Result<Network> {
    match relay {
        None => Ok(Network::Internet),
        Some(url) => Ok(Network::InternetWithRelay(
            url.parse()
                .with_context(|| format!("`{url}` is not a relay url"))?,
        )),
    }
}

pub fn sync(json: bool, workspace: Workspace, args: SyncArgs) -> anyhow::Result<()> {
    let network = network_for(args.relay.as_deref())?;
    let (replicated, device, workforce) = workspace.into_replicated();
    let replica = match replicated {
        Replicated::Local(replica) => *replica,
        Replicated::Daemon(mut client) => {
            if args.serve {
                bail!("the daemon of this machine is already serving; no need for --serve");
            }
            let peers = client.sync_now(workforce)?;
            let state = client.state(workforce)?;
            return finish(json, &device, &peers, state.event_count(), false);
        }
    };
    let peers: Vec<(DeviceKey, String)> = peers_of(replica.state(), device.key())
        .into_iter()
        .map(|peer| (peer, device_label(replica.state(), peer)))
        .collect();
    let shared = SharedReplica::new(replica);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let node = Arc::new(
            Node::start(&device, network)
                .await
                .context("starting the network endpoint")?,
        );
        node.serve(workforce, shared.clone());
        if args.serve {
            eprintln!(
                "serving as device {} since {}; interrupt to stop",
                device.key(),
                timestamp_now()
            );
        }
        let results = sync_all(&node, workforce, peers, None).await;
        let events = shared.lock().state().event_count();
        finish(json, &device, &results, events, args.serve)?;
        if args.serve {
            tokio::signal::ctrl_c().await?;
        }
        Ok(())
    })
}

fn finish(
    json: bool,
    device: &DeviceSigner,
    peers: &[PeerSync],
    events: usize,
    serving: bool,
) -> anyhow::Result<()> {
    let view = SyncView {
        device: device.key().to_string(),
        peers: peers.iter().map(PeerView::from).collect(),
        events,
        serving,
    };
    emit(json, &view, render)?;
    if !serving && !peers.is_empty() && peers.iter().all(|peer| !peer.reached()) {
        bail!("no other device could be reached");
    }
    Ok(())
}

#[derive(Serialize)]
pub struct InviteView {
    pub code: String,
    pub expires: String,
}

pub fn invite_device(
    json: bool,
    workspace: &mut Workspace,
    args: InviteDeviceArgs,
) -> anyhow::Result<()> {
    let principal = workspace.principal_signer()?;
    let device = workspace.device_key();
    if workspace.state().principal_of(&device) != Some(principal.key()) {
        bail!("the principal key on this machine belongs to someone else");
    }
    let workforce = workspace
        .state()
        .workforce_id()
        .context("the workforce has no genesis")?;
    let now = crate::workspace::now_ms();
    let lifetime = u64::try_from(Duration::from(args.expires).as_millis()).unwrap_or(u64::MAX);
    let expires_ms = now.saturating_add(lifetime);

    let secret = InviteSigner::generate();
    let statement = InviteCreated {
        kind: InviteKind::Device,
        invite: secret.key(),
        expires_ms,
    };
    let event = workspace.author(EventBody::InviteCreated(Signed::sign(
        &principal,
        Some(workforce),
        statement,
    )?))?;
    let mut bootstrap = vec![device];
    bootstrap.extend(
        peers_of(workspace.state(), device)
            .into_iter()
            .filter(|peer| workspace.state().principal_of(peer) == Some(principal.key())),
    );
    let code = InviteCode::new(workforce, event.id(), &secret, bootstrap);
    let expires = i64::try_from(expires_ms)
        .ok()
        .and_then(|millis| jiff::Timestamp::from_millisecond(millis).ok())
        .map_or_else(|| "unknown".to_string(), |time| time.to_string());
    emit(
        json,
        &InviteView {
            code: code.to_string(),
            expires,
        },
        |view| {
            format!(
                "{}\nvalid until {}; on the new machine, inside a clone of this repository, run:\n  wf join <code> --name <machine>\nthis machine must be online: run `wf daemon` or `wf sync --serve`\n",
                view.code, view.expires
            )
        },
    )
}

#[derive(Serialize)]
pub struct JoinView {
    pub workforce: String,
    pub device: String,
    pub name: String,
    pub principal: String,
    pub events: usize,
}

pub(crate) fn detect_hardware() -> String {
    let cpus = std::thread::available_parallelism().map_or(1, |count| count.get());
    format!(
        "{} {}, {cpus} cpus",
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

pub(crate) fn parse_agents(agents: &[String]) -> anyhow::Result<BTreeMap<AgentName, u32>> {
    agents
        .iter()
        .map(|text| {
            let (name, sessions) = text
                .split_once('=')
                .with_context(|| format!("`{text}` is not NAME=SESSIONS"))?;
            Ok((
                AgentName::new(name)?,
                sessions
                    .parse()
                    .with_context(|| format!("`{sessions}` is not a number"))?,
            ))
        })
        .collect()
}

fn ask_device_name() -> anyhow::Result<String> {
    use std::io::{BufRead, Write};
    eprint!("name for this machine: ");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

pub fn join(
    json: bool,
    mode: Mode,
    args: JoinArgs,
    data: DataDir,
    cwd: &Path,
) -> anyhow::Result<()> {
    let code: InviteCode = args.code.parse().context("reading the invite code")?;
    let name = match args.name {
        Some(name) => name,
        None if mode == Mode::Interactive => ask_device_name()?,
        None => bail!("pass --name to say what this machine is called"),
    };
    if name.trim().is_empty() {
        bail!("the device name must not be empty");
    }
    let profile = DeviceProfile {
        name: name.clone(),
        hardware: args.hardware.unwrap_or_else(detect_hardware),
        agents: parse_agents(&args.agents)?,
        max_sessions: args.max_sessions,
    };
    let network = network_for(args.relay.as_deref())?;

    match workspace::find_workforce(cwd) {
        Ok((_, id)) if id != code.workforce => bail!(
            "this directory belongs to workforce {id}, but the invite is for {}",
            code.workforce
        ),
        Ok(_) => {}
        Err(_) => {
            let file = toml::to_string(&WorkforceFile { id: code.workforce })?;
            std::fs::write(cwd.join(WORKFORCE_FILE), file)?;
        }
    }
    let device = data.device(true)?.expect("created on demand");
    let store = RedbStore::open(workspace::store_path(&data, &code.workforce)?)?;
    let replica = Replica::open(store)?;
    if replica.state().principal_of(&device.key()).is_some() {
        bail!("this machine has already joined the workforce");
    }
    let shared = SharedReplica::new(replica);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let node = Node::start(&device, network)
            .await
            .context("starting the network endpoint")?;
        let mut failures = Vec::new();
        let mut joined = false;
        for bootstrap in &code.bootstrap {
            let authoring = shared.clone();
            let signer = device.clone();
            let invite = code.invite;
            let proof = code.prove(device.key())?;
            let profile = profile.clone();
            let result = node
                .join(&code, wf_net::endpoint_id(*bootstrap), &shared, move || {
                    let mut replica = authoring.lock();
                    let principal = replica
                        .state()
                        .invite(&invite)
                        .map(|invite| invite.principal)
                        .ok_or("the invite is not in the log the member sent")?;
                    let actor = Actor::Human(principal);
                    replica.author(
                        &signer,
                        actor.clone(),
                        workspace::now_ms(),
                        EventBody::DeviceBound(DeviceBound { invite, proof }),
                    )?;
                    replica.author(
                        &signer,
                        actor,
                        workspace::now_ms(),
                        EventBody::DeviceProfile(profile),
                    )?;
                    Ok(())
                })
                .await;
            match result {
                Ok(()) => {
                    joined = true;
                    break;
                }
                Err(error @ (NetError::JoinRefused { .. } | NetError::JoinAuthoring(_))) => {
                    node.shutdown().await;
                    return Err(error.into());
                }
                Err(error) => failures.push(error.to_string()),
            }
        }
        node.shutdown().await;
        if !joined {
            bail!(
                "could not reach a device to join through:\n  {}",
                failures.join("\n  ")
            );
        }
        let replica = shared.lock();
        let state = replica.state();
        let principal = state
            .principal_of(&device.key())
            .context("the join did not bind this device")?;
        emit(
            json,
            &JoinView {
                workforce: code.workforce.to_string(),
                device: device.key().to_string(),
                name,
                principal: crate::view::actor_label(state, &Actor::Human(principal)),
                events: state.event_count(),
            },
            |view| {
                format!(
                    "joined workforce {} as {} ({}) with {} events\n",
                    view.workforce, view.name, view.principal, view.events
                )
            },
        )
    })
}

/// The name of this machine, for the device profile.
pub(crate) fn host_name() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|name| !name.trim().is_empty())
        .or_else(|| {
            let output = std::process::Command::new("hostname").output().ok()?;
            let name = String::from_utf8(output.stdout).ok()?.trim().to_string();
            (!name.is_empty()).then_some(name)
        })
        .unwrap_or_else(|| "this-machine".to_string())
}

#[derive(Serialize)]
struct DaemonView {
    device: String,
    socket: String,
}

pub fn daemon(json: bool, args: DaemonArgs, data: DataDir) -> anyhow::Result<()> {
    let device = data
        .device(false)?
        .context("no device key; run `wf init` or `wf join` first")?;
    let socket = data.daemon_socket();
    let config = wf_daemon::Config {
        data_dir: data.root().to_path_buf(),
        socket: socket.clone(),
        network: if args.no_network {
            Network::Direct
        } else {
            network_for(args.relay.as_deref())?
        },
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let daemon = wf_daemon::Daemon::start(config, device.clone()).await?;
        emit(
            json,
            &DaemonView {
                device: device.key().to_string(),
                socket: socket.display().to_string(),
            },
            |view| {
                format!(
                    "daemon of device {} listening on {}; interrupt to stop\n",
                    view.device, view.socket
                )
            },
        )?;
        daemon
            .serve(async {
                let _ = tokio::signal::ctrl_c().await;
            })
            .await?;
        Ok(())
    })
}

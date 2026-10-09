//! Commands that talk to other devices.

use std::sync::Arc;

use anyhow::{Context, bail};
use serde::Serialize;
use tokio::task::JoinSet;
use wf_core::identity::DeviceKey;
use wf_core::state::State;
use wf_net::{NetError, Network, Node, SharedReplica, SyncReport};
use wf_store::RedbStore;

use crate::cli::SyncArgs;
use crate::commands::emit;
use crate::view::{device_label, timestamp_now};
use crate::workspace::Workspace;

type ReplicaNode = Node<SharedReplica<RedbStore>>;

#[derive(Serialize)]
pub struct PeerSync {
    pub device: String,
    pub reached: bool,
    pub sent: usize,
    pub received: usize,
    pub rejected: usize,
    pub error: Option<String>,
}

#[derive(Serialize)]
pub struct SyncView {
    pub device: String,
    pub peers: Vec<PeerSync>,
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

/// The other bound devices of the workforce.
pub fn peers_of(state: &State, own: DeviceKey) -> Vec<DeviceKey> {
    state
        .members()
        .flat_map(|(_, member)| member.devices.iter().copied())
        .filter(|device| *device != own)
        .collect()
}

pub async fn sync_all(
    node: &Arc<ReplicaNode>,
    workforce: wf_core::id::WorkforceId,
    peers: Vec<(DeviceKey, String)>,
) -> Vec<PeerSync> {
    let mut tasks = JoinSet::new();
    for (index, (device, label)) in peers.into_iter().enumerate() {
        let node = Arc::clone(node);
        tasks.spawn(async move {
            let result = node.sync_with(workforce, wf_net::endpoint_id(device)).await;
            (index, label, result)
        });
    }
    let mut results = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        let Ok((index, label, result)) = joined else {
            continue;
        };
        results.push((index, peer_result(label, result)));
    }
    results.sort_by_key(|(index, _)| *index);
    results.into_iter().map(|(_, result)| result).collect()
}

fn peer_result(device: String, result: Result<SyncReport, NetError>) -> PeerSync {
    match result {
        Ok(report) => PeerSync {
            device,
            reached: true,
            sent: report.sent,
            received: report.applied,
            rejected: report.rejected,
            error: None,
        },
        Err(error) => PeerSync {
            device,
            reached: false,
            sent: 0,
            received: 0,
            rejected: 0,
            error: Some(error.to_string()),
        },
    }
}

pub fn sync(json: bool, workspace: Workspace, args: SyncArgs) -> anyhow::Result<()> {
    let network = network_for(args.relay.as_deref())?;
    let (replica, device) = workspace.into_parts();
    let workforce = replica
        .state()
        .workforce_id()
        .context("the workforce has no genesis")?;
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
        let results = sync_all(&node, workforce, peers).await;
        let events = shared.lock().state().event_count();
        let view = SyncView {
            device: device.key().to_string(),
            peers: results,
            events,
            serving: args.serve,
        };
        if !args.serve && !view.peers.is_empty() && view.peers.iter().all(|peer| !peer.reached) {
            emit(json, &view, render)?;
            bail!("no other device could be reached");
        }
        emit(json, &view, render)?;
        if args.serve {
            tokio::signal::ctrl_c().await?;
        }
        Ok(())
    })
}

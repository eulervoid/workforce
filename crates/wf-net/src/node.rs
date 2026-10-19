//! An iroh endpoint serving workforces: the device key is the endpoint key, and every protocol
//! but joining refuses endpoints that are not bound devices of a member.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use iroh::address_lookup::memory::MemoryLookup;
use iroh::endpoint::{Connection, presets};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, RelayUrl, SecretKey};
use wf_core::id::WorkforceId;
use wf_core::identity::{DeviceKey, DeviceSigner};
use wf_core::invite::{InviteCode, JoinRefusal};

use crate::gossip::{GOSSIP_ALPN, HeadsTopic, topic_id};
use crate::join::{JOIN_ALPN, JoinFailure, JoinHandler, JoinRequest, Reservations, run_join};
use crate::log::SyncLog;
use crate::sync::{SYNC_ALPN, SyncError, SyncReport, initiate, read_open, respond};

/// Hole punching and address lookup can take a while.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// Without those a connection works at once or not at all.
const DIRECT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const FORBIDDEN: u32 = 403;
const NOT_SERVED: u32 = 404;
pub(crate) const FAILED: u32 = 500;
const DONE: u32 = 0;

/// The error of some other layer, kept as the source of a [`NetError`].
pub type SourceError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("could not start the network endpoint")]
    Bind(#[source] SourceError),
    #[error("could not reach {peer}: {source}")]
    Connect {
        peer: String,
        #[source]
        source: SourceError,
    },
    #[error("sync with {peer} failed: {source}")]
    Sync {
        peer: String,
        #[source]
        source: SyncError,
    },
    #[error("gossip failed")]
    Gossip(#[source] SourceError),
    #[error("workforce {0} is not served here")]
    NotServed(WorkforceId),
    #[error("{peer} refused the join: {reason}")]
    JoinRefused { peer: String, reason: JoinRefusal },
    #[error("could not write the events of the new device")]
    JoinAuthoring(#[source] SourceError),
}

/// How the endpoint finds and reaches its peers.
#[derive(Clone, Debug, Default)]
pub enum Network {
    /// Relay servers and address lookup from n0, or the given relay instead of theirs.
    #[default]
    Internet,
    InternetWithRelay(RelayUrl),
    /// Direct connections to the addresses given only: no relay, no address lookup.
    Direct,
}

#[derive(Debug, thiserror::Error)]
#[error("timed out")]
struct TimedOut;

fn connect_error(peer: &str, source: impl Into<SourceError>) -> NetError {
    NetError::Connect {
        peer: peer.to_string(),
        source: source.into(),
    }
}

pub fn endpoint_id(device: DeviceKey) -> EndpointId {
    EndpointId::from_bytes(device.as_bytes()).expect("a device key is a valid endpoint id")
}

pub fn device_key(endpoint: EndpointId) -> DeviceKey {
    DeviceKey::from_bytes(endpoint.as_bytes()).expect("an endpoint id is a valid device key")
}

/// The workforces this endpoint serves, by id.
pub(crate) struct Registry<L>(Arc<RwLock<BTreeMap<WorkforceId, L>>>);

impl<L> Clone for Registry<L> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<L: SyncLog + Clone> Registry<L> {
    fn new() -> Self {
        Self(Arc::new(RwLock::new(BTreeMap::new())))
    }

    fn insert(&self, workforce: WorkforceId, log: L) {
        self.0
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(workforce, log);
    }

    pub(crate) fn get(&self, workforce: &WorkforceId) -> Option<L> {
        self.0
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(workforce)
            .cloned()
    }

    fn logs(&self) -> Vec<L> {
        self.0
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect()
    }

    /// Whether the device is bound in any workforce served here.
    pub(crate) async fn is_member(&self, device: DeviceKey) -> bool {
        for log in self.logs() {
            if log.is_bound(device).await.unwrap_or(false) {
                return true;
            }
        }
        false
    }
}

struct SyncHandler<L>(Registry<L>);

impl<L> std::fmt::Debug for SyncHandler<L> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SyncHandler")
    }
}

impl<L: SyncLog + Clone + 'static> ProtocolHandler for SyncHandler<L> {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let remote = device_key(connection.remote_id());
        if !self.0.is_member(remote).await {
            connection.close(FORBIDDEN.into(), b"not a member");
            return Ok(());
        }
        let (mut send, mut recv) = connection
            .accept_bi()
            .await
            .map_err(AcceptError::from_err)?;
        let workforce = read_open(&mut recv).await.map_err(AcceptError::from_err)?;
        let Some(log) = self.0.get(&workforce) else {
            connection.close(NOT_SERVED.into(), b"workforce not served");
            return Ok(());
        };
        if !log.is_bound(remote).await.unwrap_or(false) {
            connection.close(FORBIDDEN.into(), b"not a member");
            return Ok(());
        }
        let result = respond(tokio::io::join(&mut recv, &mut send), &log).await;
        match result {
            Ok(_) => {
                let _ = send.finish();
                connection.closed().await;
                Ok(())
            }
            Err(error) => {
                connection.close(FAILED.into(), b"sync failed");
                Err(AcceptError::from_err(error))
            }
        }
    }
}

/// Lets a protocol handler through only for devices that are bound in a workforce served here.
struct MembersOnly<H, L> {
    inner: H,
    registry: Registry<L>,
}

impl<H: std::fmt::Debug, L> std::fmt::Debug for MembersOnly<H, L> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "MembersOnly({:?})", self.inner)
    }
}

impl<H: ProtocolHandler, L: SyncLog + Clone + 'static> ProtocolHandler for MembersOnly<H, L> {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        if !self
            .registry
            .is_member(device_key(connection.remote_id()))
            .await
        {
            connection.close(FORBIDDEN.into(), b"not a member");
            return Ok(());
        }
        self.inner.accept(connection).await
    }

    async fn shutdown(&self) {
        self.inner.shutdown().await;
    }
}

pub struct Node<L: SyncLog + Clone + 'static> {
    router: Router,
    registry: Registry<L>,
    gossip: iroh_gossip::Gossip,
    lookup: MemoryLookup,
    connect_timeout: Duration,
}

impl<L: SyncLog + Clone + 'static> Node<L> {
    pub async fn start(device: &DeviceSigner, network: Network) -> Result<Self, NetError> {
        let lookup = MemoryLookup::new();
        let connect_timeout = match network {
            Network::Direct => DIRECT_CONNECT_TIMEOUT,
            Network::Internet | Network::InternetWithRelay(_) => CONNECT_TIMEOUT,
        };
        let builder = match network {
            Network::Internet => Endpoint::builder(presets::N0),
            Network::InternetWithRelay(url) => {
                Endpoint::builder(presets::N0).relay_mode(RelayMode::custom([url]))
            }
            Network::Direct => Endpoint::builder(presets::Minimal).relay_mode(RelayMode::Disabled),
        }
        .secret_key(SecretKey::from_bytes(&device.to_bytes()))
        .address_lookup(lookup.clone());
        let endpoint = builder
            .bind()
            .await
            .map_err(|error| NetError::Bind(error.into()))?;
        let registry = Registry::new();
        let gossip = iroh_gossip::Gossip::builder().spawn(endpoint.clone());
        let router = Router::builder(endpoint)
            .accept(SYNC_ALPN, SyncHandler(registry.clone()))
            .accept(
                GOSSIP_ALPN,
                MembersOnly {
                    inner: gossip.clone(),
                    registry: registry.clone(),
                },
            )
            .accept(
                JOIN_ALPN,
                JoinHandler {
                    registry: registry.clone(),
                    reservations: Reservations::default(),
                },
            )
            .spawn();
        Ok(Self {
            router,
            registry,
            gossip,
            lookup,
            connect_timeout,
        })
    }

    pub fn endpoint(&self) -> &Endpoint {
        self.router.endpoint()
    }

    pub fn device(&self) -> DeviceKey {
        device_key(self.endpoint().id())
    }

    /// Where this endpoint can be dialed on the local network, for peers that cannot look it up.
    pub fn local_addr(&self) -> EndpointAddr {
        let port_addrs = self.endpoint().bound_sockets();
        let loopback = std::net::Ipv4Addr::LOCALHOST;
        port_addrs
            .into_iter()
            .fold(EndpointAddr::new(self.endpoint().id()), |addr, socket| {
                addr.with_ip_addr(std::net::SocketAddr::new(loopback.into(), socket.port()))
            })
    }

    /// Remembers where a device can be reached, for peers address lookup does not know.
    pub fn remember(&self, addr: EndpointAddr) {
        self.lookup.add_endpoint_info(addr);
    }

    /// Joins the heads topic of a workforce, starting from `peers`.
    pub async fn heads_topic(
        &self,
        workforce: &WorkforceId,
        peers: &[DeviceKey],
    ) -> Result<HeadsTopic, NetError> {
        let bootstrap = peers.iter().map(|peer| endpoint_id(*peer)).collect();
        let (sender, receiver) = self
            .gossip
            .subscribe(topic_id(workforce), bootstrap)
            .await
            .map_err(|error| NetError::Gossip(error.into()))?
            .split();
        Ok(HeadsTopic::new(sender, receiver, self.device()))
    }

    pub fn serve(&self, workforce: WorkforceId, log: L) {
        self.registry.insert(workforce, log);
    }

    /// Syncs one workforce with a peer. `peer` may be a bare device key when address lookup is on.
    pub async fn sync_with(
        &self,
        workforce: WorkforceId,
        peer: impl Into<EndpointAddr>,
    ) -> Result<SyncReport, NetError> {
        let log = self
            .registry
            .get(&workforce)
            .ok_or(NetError::NotServed(workforce))?;
        let peer: EndpointAddr = peer.into();
        let name = device_key(peer.id).fmt_short();
        let connection = tokio::time::timeout(
            self.connect_timeout,
            self.endpoint().connect(peer.clone(), SYNC_ALPN),
        )
        .await
        .map_err(|_| connect_error(&name, TimedOut))?
        .map_err(|error| connect_error(&name, error))?;
        let (mut send, mut recv) = connection
            .open_bi()
            .await
            .map_err(|error| connect_error(&name, error))?;
        let report = initiate(tokio::io::join(&mut recv, &mut send), workforce, &log)
            .await
            .map_err(|source| NetError::Sync {
                peer: name.clone(),
                source,
            })?;
        let _ = send.finish();
        connection.close(DONE.into(), b"done");
        Ok(report)
    }

    /// Redeems an invite with the member at `peer`. The log of the workforce arrives in `log`,
    /// then `author` writes the events of this device into it, and they are sent back.
    pub async fn join<Fut>(
        &self,
        code: &InviteCode,
        peer: impl Into<EndpointAddr>,
        log: &L,
        author: impl FnOnce() -> Fut,
    ) -> Result<(), NetError>
    where
        Fut: Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>>,
    {
        let peer: EndpointAddr = peer.into();
        let name = device_key(peer.id).fmt_short();
        let connection = tokio::time::timeout(
            self.connect_timeout,
            self.endpoint().connect(peer.clone(), JOIN_ALPN),
        )
        .await
        .map_err(|_| connect_error(&name, TimedOut))?
        .map_err(|error| connect_error(&name, error))?;
        let (mut send, mut recv) = connection
            .open_bi()
            .await
            .map_err(|error| connect_error(&name, error))?;
        let proof = code
            .prove(self.device())
            .map_err(|error| NetError::JoinAuthoring(error.into()))?;
        let request = JoinRequest {
            workforce: code.workforce,
            invite: code.invite,
            proof,
        };
        let result = run_join(
            &mut tokio::io::join(&mut recv, &mut send),
            request,
            log,
            author,
        )
        .await;
        let _ = send.finish();
        connection.close(DONE.into(), b"done");
        result.map_err(|failure| match failure {
            JoinFailure::Net(source) => connect_error(&name, source),
            JoinFailure::Refused(reason) => NetError::JoinRefused {
                peer: name.clone(),
                reason,
            },
            JoinFailure::Author(error) => NetError::JoinAuthoring(error),
        })
    }

    pub async fn shutdown(self) {
        let _ = self.router.shutdown().await;
    }
}

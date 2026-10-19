//! Gossip on `heads`: after a write and every minute each device announces a digest of its
//! heads on a topic of the workforce. Messages carry hashes only, never events: a device whose
//! digest differs syncs with the announcer over `wf/sync/3`. Hearing from a device is also
//! what "last seen" is made of.

use std::collections::BTreeSet;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use iroh::EndpointId;
use iroh_gossip::TopicId;
use iroh_gossip::api::{Event, GossipReceiver, GossipSender};
use n0_future::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use wf_core::encoding;
use wf_core::id::{EventId, HashId, WorkforceId};
use wf_core::identity::{DeviceKey, DeviceSigner, Signature};

use crate::log::SyncLog;
use crate::node::{NetError, endpoint_id};

pub const GOSSIP_ALPN: &[u8] = iroh_gossip::ALPN;

const HEARD_CAPACITY: usize = 64;

pub fn topic_id(workforce: &WorkforceId) -> TopicId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"wf/gossip/1");
    hasher.update(workforce.as_bytes());
    TopicId::from_bytes(*hasher.finalize().as_bytes())
}

/// What two replicas compare to tell whether they hold the same events.
pub fn heads_digest(heads: &BTreeSet<EventId>) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    for head in heads {
        hasher.update(head.as_bytes());
    }
    *hasher.finalize().as_bytes()
}

/// How far an announcement's time may be from ours before it is taken for a replay.
const MAX_CLOCK_SKEW: Duration = Duration::from_secs(5 * 60);

#[derive(Serialize, Deserialize)]
enum Message {
    /// Signed by `device` over the topic, the digest and the time, so only that device can say it
    /// is online and nobody can replay an old announcement for long or on another topic.
    Heads {
        device: DeviceKey,
        digest: [u8; 32],
        time: u64,
        signature: Signature,
    },
}

/// An announcement of someone's heads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Heard {
    pub device: DeviceKey,
    pub digest: [u8; 32],
}

fn unix_seconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// The announcement a message makes, if its signature holds for this topic and its time is near
/// `now`. Whether the signer belongs to the workforce is for the caller to check.
fn authenticate(topic: TopicId, content: &[u8], now: SystemTime) -> Option<Heard> {
    let Message::Heads {
        device,
        digest,
        time,
        signature,
    } = encoding::decode(content).ok()?;
    if unix_seconds(now).abs_diff(time) > MAX_CLOCK_SKEW.as_secs() {
        return None;
    }
    device
        .verify_heads(topic.as_bytes(), &digest, time, &signature)
        .ok()?;
    Some(Heard { device, digest })
}

/// One workforce's gossip topic: announce our heads, hear theirs.
pub struct HeadsTopic {
    sender: GossipSender,
    topic: TopicId,
    signer: DeviceSigner,
    pub heard: mpsc::Receiver<Heard>,
}

impl HeadsTopic {
    /// Only devices bound in `log` are heard.
    pub(crate) fn new<L: SyncLog + 'static>(
        topic: TopicId,
        sender: GossipSender,
        receiver: GossipReceiver,
        signer: DeviceSigner,
        log: L,
    ) -> Self {
        let (relay, heard) = mpsc::channel(HEARD_CAPACITY);
        tokio::spawn(relay_messages(topic, receiver, relay, log));
        Self {
            sender,
            topic,
            signer,
            heard,
        }
    }

    pub async fn announce(&self, heads: &BTreeSet<EventId>) -> Result<(), NetError> {
        let digest = heads_digest(heads);
        let time = unix_seconds(SystemTime::now());
        let signature = self
            .signer
            .sign_heads(self.topic.as_bytes(), &digest, time)
            .map_err(|error| NetError::Gossip(error.into()))?;
        let message = encoding::encode(&Message::Heads {
            device: self.signer.key(),
            digest,
            time,
            signature,
        })
        .map_err(|error| NetError::Gossip(error.into()))?;
        self.sender
            .broadcast(message.into())
            .await
            .map_err(|error| NetError::Gossip(error.into()))
    }

    /// Reaches out to devices that may not be neighbors yet.
    pub async fn join(&self, peers: &[DeviceKey]) -> Result<(), NetError> {
        let peers: Vec<EndpointId> = peers.iter().map(|peer| endpoint_id(*peer)).collect();
        self.sender
            .join_peers(peers)
            .await
            .map_err(|error| NetError::Gossip(error.into()))
    }
}

/// Passes on what the topic hears from members until the topic is dropped, which leaves the
/// gossip topic too. The gossip connection is shared by all topics, so which workforce a sender
/// belongs to can only be checked here.
async fn relay_messages<L: SyncLog>(
    topic: TopicId,
    mut receiver: GossipReceiver,
    relay: mpsc::Sender<Heard>,
    log: L,
) {
    loop {
        let event = tokio::select! {
            _ = relay.closed() => return,
            event = receiver.next() => event,
        };
        let Some(event) = event else { return };
        let Ok(Event::Received(message)) = event else {
            continue;
        };
        let Some(heard) = authenticate(topic, &message.content, SystemTime::now()) else {
            continue;
        };
        if !log.is_bound(heard.device).await.unwrap_or(false) {
            continue;
        }
        if relay.send(heard).await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digests_follow_the_heads_and_topics_the_workforce() {
        let one = BTreeSet::from([EventId::of(b"a")]);
        let two = BTreeSet::from([EventId::of(b"a"), EventId::of(b"b")]);
        assert_eq!(heads_digest(&one), heads_digest(&one.clone()));
        assert_ne!(heads_digest(&one), heads_digest(&two));
        let first = WorkforceId::from(EventId::of(b"one"));
        let second = WorkforceId::from(EventId::of(b"two"));
        assert_ne!(topic_id(&first), topic_id(&second));
        assert_eq!(topic_id(&first), topic_id(&first));
    }

    fn announcement(
        signer: &DeviceSigner,
        claimed: DeviceKey,
        topic: TopicId,
        digest: [u8; 32],
        time: SystemTime,
    ) -> Vec<u8> {
        let time = unix_seconds(time);
        let signature = signer.sign_heads(topic.as_bytes(), &digest, time).unwrap();
        encoding::encode(&Message::Heads {
            device: claimed,
            digest,
            time,
            signature,
        })
        .unwrap()
    }

    #[test]
    fn a_spoofed_sender_is_dropped() {
        let topic = topic_id(&WorkforceId::from(EventId::of(b"one")));
        let other_topic = topic_id(&WorkforceId::from(EventId::of(b"two")));
        let now = SystemTime::now();
        let device = DeviceSigner::generate([1; 32]);
        let impostor = DeviceSigner::generate([2; 32]);
        let digest = [7; 32];

        let genuine = announcement(&device, device.key(), topic, digest, now);
        assert_eq!(
            authenticate(topic, &genuine, now),
            Some(Heard {
                device: device.key(),
                digest
            })
        );

        let spoofed = announcement(&impostor, device.key(), topic, digest, now);
        assert_eq!(authenticate(topic, &spoofed, now), None, "wrong signer");
        assert_eq!(
            authenticate(other_topic, &genuine, now),
            None,
            "other topic"
        );

        let stale = announcement(
            &device,
            device.key(),
            topic,
            digest,
            now - 2 * MAX_CLOCK_SKEW,
        );
        assert_eq!(authenticate(topic, &stale, now), None, "replayed late");
    }
}

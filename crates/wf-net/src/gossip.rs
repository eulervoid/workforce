//! Gossip on `heads`: after a write and every minute each device announces a digest of its
//! heads on a topic of the workforce. Messages carry hashes only, never events: a device whose
//! digest differs syncs with the announcer over `wf/sync/1`. Hearing from a device is also
//! what "last seen" is made of.

use std::collections::BTreeSet;

use iroh::EndpointId;
use iroh_gossip::TopicId;
use iroh_gossip::api::{Event, GossipReceiver, GossipSender};
use n0_future::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use wf_core::encoding;
use wf_core::id::{EventId, HashId, WorkforceId};
use wf_core::identity::DeviceKey;

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

#[derive(Serialize, Deserialize)]
enum Message {
    /// The sender is a claim made by the message, not something gossip proves: a wrong one only
    /// makes a receiver sync with a device it would sync with anyway.
    Heads { device: DeviceKey, digest: [u8; 32] },
}

/// An announcement of someone's heads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Heard {
    pub device: DeviceKey,
    pub digest: [u8; 32],
}

/// One workforce's gossip topic: announce our heads, hear theirs.
pub struct HeadsTopic {
    sender: GossipSender,
    device: DeviceKey,
    pub heard: mpsc::Receiver<Heard>,
}

impl HeadsTopic {
    pub(crate) fn new(sender: GossipSender, receiver: GossipReceiver, device: DeviceKey) -> Self {
        let (relay, heard) = mpsc::channel(HEARD_CAPACITY);
        tokio::spawn(relay_messages(receiver, relay));
        Self {
            sender,
            device,
            heard,
        }
    }

    pub async fn announce(&self, heads: &BTreeSet<EventId>) -> Result<(), NetError> {
        let message = encoding::encode(&Message::Heads {
            device: self.device,
            digest: heads_digest(heads),
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

async fn relay_messages(mut receiver: GossipReceiver, relay: mpsc::Sender<Heard>) {
    while let Some(event) = receiver.next().await {
        let Ok(Event::Received(message)) = event else {
            continue;
        };
        let Ok(Message::Heads { device, digest }) = encoding::decode(&message.content) else {
            continue;
        };
        if relay.send(Heard { device, digest }).await.is_err() {
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
}

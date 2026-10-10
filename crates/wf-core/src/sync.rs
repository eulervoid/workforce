//! The pure half of reconciliation: what to send a peer and what to ask it for. The wire
//! protocol lives in `wf-net`.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::event::Event;
use crate::id::EventId;
use crate::identity::DeviceKey;
use crate::state::{FeedHead, State, Waiting};

/// What a replica knows of one author's feed: enough for a peer to tell what it lacks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedVersion {
    pub max_seq: u64,
    /// The ends of the feed's branches: one unless the feed has equivocated.
    pub tips: Vec<FeedHead>,
}

pub type Versions = BTreeMap<DeviceKey, FeedVersion>;

/// What to send of one author's feed: every event above `after_seq`, and these specific events,
/// which can sit at or below it when the feed has equivocated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outgoing {
    pub author: DeviceKey,
    pub after_seq: u64,
    pub tips: Vec<EventId>,
}

/// What `mine` has that `theirs` may lack.
///
/// Events above the peer's highest `seq` are always missing. Below it only an equivocated feed can hide
/// something, because then two different events share a `seq`: the tips of such a feed are sent
/// unless the peer already has them as its own tips. The events behind a tip reach the peer as
/// missing parents, by id.
pub fn ahead_of(mine: &Versions, theirs: &Versions) -> Vec<Outgoing> {
    mine.iter()
        .filter_map(|(author, version)| {
            let peer = theirs.get(author);
            let after_seq = peer.map_or(0, |peer| peer.max_seq);
            let forked = version.tips.len() > 1;
            let tips: Vec<EventId> = version
                .tips
                .iter()
                .filter(|tip| {
                    tip.seq <= after_seq
                        && (forked || tip.seq == after_seq)
                        && !peer.is_some_and(|peer| peer.tips.contains(tip))
                })
                .map(|tip| tip.id)
                .collect();
            (version.max_seq > after_seq || !tips.is_empty()).then_some(Outgoing {
                author: *author,
                after_seq,
                tips,
            })
        })
        .collect()
}

/// Orders events so every event follows the ones it depends on. A receiver that gets them in
/// this order never has to park one.
pub fn causal_order(events: &mut [Event]) {
    events.sort_by_cached_key(|event| (event.content.hlc, event.id()));
}

impl State {
    /// Parents of parked events that this state has not seen and no parked event can supply, and
    /// the history of a fork that is still to be taken in. Ask a peer for these by id.
    pub fn missing_parents(&self) -> BTreeSet<EventId> {
        self.pending
            .waiting
            .keys()
            .filter_map(|waiting| match waiting {
                Waiting::Event(id) if !self.pending.events.contains_key(id) => Some(*id),
                Waiting::Event(_) | Waiting::Genesis | Waiting::History => None,
            })
            .chain(self.wanted_history())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Trust;
    use crate::testing::*;

    fn version(max_seq: u64, tips: &[(u64, &[u8])]) -> FeedVersion {
        FeedVersion {
            max_seq,
            tips: tips
                .iter()
                .map(|(seq, name)| FeedHead {
                    seq: *seq,
                    id: EventId::of(name),
                })
                .collect(),
        }
    }

    #[test]
    fn ahead_of_lists_authors_the_peer_lacks() {
        let fixture = Fixture::new(2);
        let first = fixture.devices[0].key();
        let second = fixture.devices[1].key();
        let mine = Versions::from([
            (first, version(5, &[(5, b"a")])),
            (second, version(2, &[(2, b"b")])),
        ]);
        let theirs = Versions::from([(first, version(5, &[(5, b"a")]))]);
        let outgoing = ahead_of(&mine, &theirs);
        assert_eq!(
            outgoing,
            vec![Outgoing {
                author: second,
                after_seq: 0,
                tips: vec![]
            }]
        );
        let theirs = Versions::from([
            (first, version(3, &[(3, b"a")])),
            (second, version(2, &[(2, b"b")])),
        ]);
        assert_eq!(ahead_of(&mine, &theirs).len(), 1);
        assert!(ahead_of(&theirs, &mine).is_empty());
    }

    #[test]
    fn different_tips_at_the_same_seq_are_sent() {
        let author = Fixture::new(1).devices[0].key();
        let mine = Versions::from([(author, version(5, &[(5, b"a")]))]);
        let theirs = Versions::from([(author, version(5, &[(5, b"b")]))]);
        let outgoing = ahead_of(&mine, &theirs);
        assert_eq!(outgoing[0].after_seq, 5);
        assert_eq!(outgoing[0].tips, vec![EventId::of(b"a")]);
    }

    #[test]
    fn a_forked_feed_offers_every_tip_the_peer_lacks() {
        let author = Fixture::new(1).devices[0].key();
        let mine = Versions::from([(author, version(5, &[(3, b"a"), (5, b"b")]))]);
        let theirs = Versions::from([(author, version(5, &[(5, b"b")]))]);
        let outgoing = ahead_of(&mine, &theirs);
        assert_eq!(outgoing[0].tips, vec![EventId::of(b"a")]);
        assert!(ahead_of(&theirs, &mine).is_empty());
    }

    #[test]
    fn missing_parents_names_only_what_nothing_parked_supplies() {
        let fixture = Fixture::new(1);
        let mut author = fixture.state();
        let first = fixture.apply(&mut author, 0, 2_000, create("task", "a"));
        let second = fixture.apply(&mut author, 0, 2_001, create("task", "b"));
        let third = fixture.apply(&mut author, 0, 2_002, create("task", "c"));

        let mut state = fixture.state();
        assert!(state.missing_parents().is_empty());
        state.receive(third, Trust::Untrusted).unwrap();
        assert_eq!(state.missing_parents(), BTreeSet::from([second.id()]));
        state.receive(second, Trust::Untrusted).unwrap();
        assert_eq!(state.missing_parents(), BTreeSet::from([first.id()]));
        state.receive(first, Trust::Untrusted).unwrap();
        assert!(state.missing_parents().is_empty());
    }

    #[test]
    fn causal_order_puts_parents_first() {
        let fixture = Fixture::new(2);
        let mut state = fixture.state();
        let a = fixture.apply(&mut state, 0, 2_000, create("task", "a"));
        let b = fixture.apply(&mut state, 1, 2_001, create("task", "b"));
        let c = fixture.apply(&mut state, 0, 2_002, create("task", "c"));
        let mut events = vec![c.clone(), fixture.genesis.clone(), b.clone(), a.clone()];
        causal_order(&mut events);
        let mut replica = State::new();
        for event in events {
            let received = replica.receive(event, Trust::Untrusted).unwrap();
            assert_ne!(received.disposition, crate::state::Disposition::Pending);
        }
        assert_eq!(replica, state);
    }
}

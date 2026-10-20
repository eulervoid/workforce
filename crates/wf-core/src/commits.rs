//! The commits the log names and where the branch of each item belongs, which a device needs to
//! fetch the commits it lacks and to keep the item refs of its repository. Pure: the daemon asks
//! git what is present and carries out the rest.

use crate::id::{ItemId, SessionId};
use crate::identity::DeviceKey;
use crate::item::ItemState;
use crate::session::{CommitId, SessionRole};
use crate::state::State;

/// Why the log names a commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Naming {
    /// The head of a work session, named by one of its comments or by its end.
    Head { session: SessionId },
    /// The commit a verdict is about.
    Reviewed,
    /// The merge commit, or the commit that was merged.
    Merged,
}

/// A commit the log names, and the device that wrote the event, which has it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NamedCommit {
    pub commit: CommitId,
    pub item: ItemId,
    pub author: DeviceKey,
    pub naming: Naming,
}

/// The head a work session had when the log last named one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeadOf {
    pub session: SessionId,
    pub device: DeviceKey,
    pub commit: CommitId,
}

/// The heads the log names for the work on an item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemHeads {
    pub item: ItemId,
    /// The item is done or cancelled: its refs and its agent repository are no longer needed.
    pub finished: bool,
    /// The head `wf/<item>` is at: that of the session holding the claim, or of the latest work
    /// session that named one when none holds it.
    pub holder: Option<HeadOf>,
    /// The heads of the other work sessions that differ from the holder's. Whether one is an
    /// ancestor of the holder's, and so nothing is lost, only git knows.
    pub others: Vec<HeadOf>,
}

impl State {
    /// Every commit the log names for the items that are not cancelled or archived: session
    /// heads, the commits of verdicts, and merges.
    pub fn named_commits(&self) -> Vec<NamedCommit> {
        let Some(policy) = self.policy().map(|document| &document.policy) else {
            return Vec::new();
        };
        let mut named = Vec::new();
        for item in self.live_items() {
            if item.state(policy) == ItemState::Cancelled {
                continue;
            }
            let id = item.id();
            for session in self.sessions_of(id) {
                if let (SessionRole::Work, Some(commit)) = (session.role(), session.head()) {
                    named.push(NamedCommit {
                        commit,
                        item: id,
                        author: session.device(),
                        naming: Naming::Head {
                            session: session.id(),
                        },
                    });
                }
            }
            for review in item.verdicts().values() {
                if let Some(commit) = review.commit {
                    named.push(NamedCommit {
                        commit,
                        item: id,
                        author: review.device,
                        naming: Naming::Reviewed,
                    });
                }
            }
            for merge in item.merges().values() {
                for commit in [merge.reviewed, merge.commit] {
                    named.push(NamedCommit {
                        commit,
                        item: id,
                        author: merge.device,
                        naming: Naming::Merged,
                    });
                }
            }
        }
        named
    }

    /// Where the work on each item that is not archived stands, as the log names the heads.
    pub fn item_heads(&self) -> Vec<ItemHeads> {
        let Some(policy) = self.policy().map(|document| &document.policy) else {
            return Vec::new();
        };
        self.live_items()
            .map(|item| {
                let head_of = |session: &crate::session::Session| {
                    session.head().map(|commit| HeadOf {
                        session: session.id(),
                        device: session.device(),
                        commit,
                    })
                };
                let id = item.id();
                let latest = self
                    .sessions_of(id)
                    .filter(|session| session.role() == SessionRole::Work)
                    .filter(|session| session.head().is_some())
                    .max_by_key(|session| session.started());
                let holder = self
                    .claim(id)
                    .filter(|session| session.head().is_some())
                    .or(latest)
                    .and_then(head_of);
                let others = self
                    .sessions_of(id)
                    .filter(|session| session.role() == SessionRole::Work)
                    .filter_map(head_of)
                    .filter(|head| {
                        holder.is_some_and(|holder| {
                            head.session != holder.session && head.commit != holder.commit
                        })
                    })
                    .collect();
                ItemHeads {
                    item: id,
                    finished: item.state(policy).is_terminal(),
                    holder,
                    others,
                }
            })
            .collect()
    }
}

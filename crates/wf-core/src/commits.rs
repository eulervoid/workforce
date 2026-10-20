//! The commits the log names and where the branch of each item belongs, which a device needs to
//! fetch the commits it lacks and to keep the item refs of its repository. Pure: the daemon asks
//! git what is present and carries out the rest.

use serde::{Deserialize, Serialize};

use crate::id::{ItemId, SessionId};
use crate::identity::DeviceKey;
use crate::item::ItemState;
use crate::session::{CommitId, SessionRole};
use crate::state::State;

/// Why a device needs a commit the log names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Naming {
    /// The head of a work session of an item that is not finished, named by one of its comments or
    /// by its end: the branch of the item, or work set apart from it.
    Head { session: SessionId },
    /// The commit an item in review was submitted at, which a review and the merge start on.
    Submitted,
}

/// A commit the log names, and the device that wrote the event, which has it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamedCommit {
    pub commit: CommitId,
    pub item: ItemId,
    pub author: DeviceKey,
    pub naming: Naming,
}

/// A commit the log names that this device lacks, and how fetching it goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AwaitedCommit {
    pub named: NamedCommit,
    pub fetching: Fetching,
}

/// How fetching a commit this device lacks goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Fetching {
    /// Not asked for yet, or still being asked for.
    Underway,
    /// No device it was asked of had it or could be reached: it is asked for again now and then,
    /// and what starts on it waits.
    Unavailable {
        /// When it was found unavailable, in milliseconds since the Unix epoch.
        since_ms: u64,
    },
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
    /// The item is done or cancelled, or the holder's head is merged: its refs and its agent
    /// repository are no longer needed. Merged counts before the item is moved to done, or a
    /// device would bring back the branch the merge removed in between.
    pub finished: bool,
    /// Every work session of the item, which may have a ref of work set apart.
    pub sessions: Vec<SessionId>,
    /// The head `wf/<item>` is at: that of the session holding the claim, or of the work session
    /// that named a head last when none holds it.
    pub holder: Option<HeadOf>,
    /// The heads of the other work sessions that differ from the holder's. Whether one is an
    /// ancestor of the holder's, and so nothing is lost, only git knows.
    pub others: Vec<HeadOf>,
}

impl State {
    /// The commits that something on a device still needs: the heads of items that are not
    /// finished (the holder's, and the others' to keep apart), and the commit an item in review
    /// was submitted at, which its reviews and its merge start on. What finished items or heads
    /// rewritten away once named, or verdicts on them, is not here: no device is asked for it.
    pub fn named_commits(&self) -> Vec<NamedCommit> {
        let Some(policy) = self.policy().map(|document| &document.policy) else {
            return Vec::new();
        };
        let mut named = Vec::new();
        for heads in self.item_heads().iter().filter(|heads| !heads.finished) {
            for head in heads.holder.iter().chain(&heads.others) {
                named.push(NamedCommit {
                    commit: head.commit,
                    item: heads.item,
                    author: head.device,
                    naming: Naming::Head {
                        session: head.session,
                    },
                });
            }
        }
        for item in self.live_items() {
            let id = item.id();
            let state = item.state(policy);
            if state.is_terminal() {
                continue;
            }
            if state == ItemState::Review
                && let Some((session, _)) = self.submitting_session(id)
                && let Some(commit) = session.head()
            {
                named.push(NamedCommit {
                    commit,
                    item: id,
                    author: session.device(),
                    naming: Naming::Submitted,
                });
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
                let work = || {
                    self.sessions_of(id)
                        .filter(|session| session.role() == SessionRole::Work)
                };
                let holder = self
                    .holder(id, &SessionRole::Work)
                    .filter(|session| session.head().is_some())
                    .or_else(|| self.latest_head_session(id))
                    .and_then(head_of);
                let others = work()
                    .filter_map(head_of)
                    .filter(|head| {
                        holder.is_some_and(|holder| {
                            head.session != holder.session && head.commit != holder.commit
                        })
                    })
                    .collect();
                let merged = holder.is_some_and(|holder| {
                    item.merges()
                        .values()
                        .any(|merge| merge.reviewed == holder.commit)
                });
                ItemHeads {
                    item: id,
                    finished: merged || item.state(policy).is_terminal(),
                    sessions: work().map(|session| session.id()).collect(),
                    holder,
                    others,
                }
            })
            .collect()
    }
}

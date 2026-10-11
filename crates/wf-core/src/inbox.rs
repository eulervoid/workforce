//! What waits on a person, worked out from the state.

use crate::id::{CommentId, ItemId};
use crate::policy::{ReviewStep, StateCategory};
use crate::review::ReviewStatus;
use crate::session::CommitId;
use crate::state::State;
use crate::supervision::WaitsOn;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Waiting {
    /// A blocker on the item waits on this person: resolving it is the answer.
    Blocker { comment: CommentId },
    /// The item is in a blocked state that is not a review, such as a proposed decision. Moving
    /// it on is the answer.
    Blocked,
    /// The review of the work has reached a step for a person: `wf review`.
    HumanReview { commit: Option<CommitId> },
    /// The work is ready to merge, but the target branch is checked out with changes.
    Merge { commit: CommitId },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboxEntry {
    pub item: ItemId,
    pub waiting: Waiting,
}

impl State {
    /// What waits on a person, the item changed last first. Permission requests are held by the
    /// daemon and are not part of the state.
    pub fn inbox(&self) -> Vec<InboxEntry> {
        let Some(policy) = self.policy().map(|document| &document.policy) else {
            return Vec::new();
        };
        let merges = self.pending_merges();
        let mut entries: Vec<_> = self
            .live_items()
            .filter(|item| item.standing(policy).state_category() == Some(StateCategory::Blocked))
            .filter_map(|item| {
                let waiting = match self.review_status(item.id()) {
                    Some(ReviewStatus::Awaiting {
                        step: ReviewStep::Human,
                    }) => {
                        let (commit, _) = self.submission(item.id())?;
                        Waiting::HumanReview { commit }
                    }
                    Some(ReviewStatus::Awaiting {
                        step: ReviewStep::Agent { .. },
                    }) => return None,
                    Some(ReviewStatus::Approved { .. })
                    | Some(ReviewStatus::ChangesRequested)
                    | Some(ReviewStatus::Merged { .. })
                    | None => match merges.iter().find(|(merging, _)| *merging == item.id()) {
                        Some((_, commit)) => Waiting::Merge { commit: *commit },
                        None => Waiting::Blocked,
                    },
                };
                Some((
                    item.updated(),
                    InboxEntry {
                        item: item.id(),
                        waiting,
                    },
                ))
            })
            .collect();
        for blocker in self.open_blockers() {
            if let WaitsOn::Person(_) = blocker.waits_on {
                let Some(crate::item::ItemView::Live(item)) = self.item(blocker.item) else {
                    continue;
                };
                entries.push((
                    item.updated(),
                    InboxEntry {
                        item: blocker.item,
                        waiting: Waiting::Blocker {
                            comment: blocker.comment,
                        },
                    },
                ));
            }
        }
        entries.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.item.cmp(&right.1.item)));
        entries.into_iter().map(|(_, entry)| entry).collect()
    }
}

//! Where the review of an item stands, worked out from its submitted work, its review level and
//! the verdicts on the submitted commit. Nothing here is stored: it is a function of the state.

use std::collections::BTreeSet;

use crate::crdt::Stamp;
use crate::id::ItemId;
use crate::identity::AgentId;
use crate::item::{Item, ItemView};
use crate::name::Requirement;
use crate::policy::{ReviewStep, StateCategory};
use crate::session::{CommitId, EndReason, SessionRole, Verdict};
use crate::state::State;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReviewStatus {
    /// The next step without a verdict.
    Awaiting {
        step: ReviewStep,
    },
    ChangesRequested,
    Approved {
        commit: Option<CommitId>,
    },
    /// The reviewed commit went into the target branch as `commit`.
    Merged {
        commit: CommitId,
    },
}

impl State {
    /// What the latest submitted work session of the item handed in, and when it ended: its
    /// commit, or `None` when the branch had no commits, as for a finding that nothing has to change.
    pub fn submission(&self, item: ItemId) -> Option<(Option<CommitId>, Stamp)> {
        self.sessions()
            .filter(|session| session.item() == item && session.role() == SessionRole::Work)
            .filter_map(|session| {
                let ended = session.ended()?;
                match ended.reason {
                    EndReason::Submitted { commit } => Some((commit, ended.stamp)),
                    EndReason::Reviewed
                    | EndReason::Released
                    | EndReason::Supervised
                    | EndReason::TimedOut
                    | EndReason::Failed
                    | EndReason::Cancelled
                    | EndReason::Interrupted => None,
                }
            })
            .max_by_key(|(_, stamp)| *stamp)
    }

    /// Where the review of the item stands. `None` when nothing was submitted, or when the item
    /// needs no review and has not been merged.
    pub fn review_status(&self, item: ItemId) -> Option<ReviewStatus> {
        let policy = &self.policy()?.policy;
        let ItemView::Live(live) = self.item(item)? else {
            return None;
        };
        let (commit, submitted) = self.submission(item)?;
        if let Some(merge) = commit.and_then(|commit| {
            live.merges()
                .values()
                .find(|merge| merge.reviewed == commit)
        }) {
            return Some(ReviewStatus::Merged {
                commit: merge.commit,
            });
        }
        let level = policy.review_level(live.effective_review(policy)?)?;
        let mut steps = level.steps.clone();
        let mut index = 0;
        let mut verdicts: Vec<_> = live
            .verdicts()
            .values()
            .filter(|verdict| verdict.commit == commit && verdict.stamp > submitted)
            .collect();
        verdicts.sort_by_key(|verdict| verdict.stamp);
        for verdict in verdicts {
            if index >= steps.len() {
                break;
            }
            match verdict.verdict {
                Verdict::Approve => index += 1,
                Verdict::RequestChanges => return Some(ReviewStatus::ChangesRequested),
                Verdict::Escalate => {
                    index += 1;
                    steps.insert(index, ReviewStep::Principal);
                }
            }
        }
        Some(match steps.get(index) {
            Some(step) => ReviewStatus::Awaiting { step: step.clone() },
            None => ReviewStatus::Approved { commit },
        })
    }

    /// Items whose work is ready to merge and not merged yet: approved, or needing no review,
    /// and waiting in a review state.
    pub fn pending_merges(&self) -> Vec<(ItemId, CommitId)> {
        let Some(policy) = self.policy().map(|document| &document.policy) else {
            return Vec::new();
        };
        self.live_items()
            .filter(|item| item.standing(policy).state_category() == Some(StateCategory::Review))
            .filter_map(|item| {
                let (Some(commit), _) = self.submission(item.id())? else {
                    return None;
                };
                let ready = match self.review_status(item.id()) {
                    Some(ReviewStatus::Approved { .. }) => true,
                    Some(
                        ReviewStatus::Awaiting { .. }
                        | ReviewStatus::ChangesRequested
                        | ReviewStatus::Merged { .. },
                    ) => false,
                    None => !item.needs_review(policy),
                };
                ready.then_some((item.id(), commit))
            })
            .collect()
    }

    /// Items with approved work that wait in a review state to be finished: the work was
    /// submitted without commits, so there is nothing to merge. An item that moved on after the
    /// submission was moved by someone, and is left where it is.
    pub fn approved_without_commits(&self) -> Vec<ItemId> {
        let Some(policy) = self.policy().map(|document| &document.policy) else {
            return Vec::new();
        };
        self.live_items()
            .filter(|item| item.standing(policy).state_category() == Some(StateCategory::Review))
            .filter(|item| {
                matches!(
                    self.submission(item.id()),
                    Some((None, submitted)) if item.moved() < submitted
                ) && matches!(
                    self.review_status(item.id()),
                    Some(ReviewStatus::Approved { .. })
                )
            })
            .map(Item::id)
            .collect()
    }

    /// Items that wait for a review step by an agent that no hosted agent the dispatcher starts has
    /// the capabilities for.
    pub fn unstaffed_reviews(&self) -> Vec<ItemId> {
        let Some(policy) = self.policy().map(|document| &document.policy) else {
            return Vec::new();
        };
        let staff: Vec<&crate::state::AgentDefinition> = self
            .agents()
            .filter(|(id, definition)| {
                definition.autonomy.start == crate::autonomy::StartMode::Auto
                    && self.devices().any(|device| {
                        device.principal == id.principal
                            && device
                                .profile
                                .is_some_and(|profile| profile.agents.contains_key(&id.name))
                    })
            })
            .map(|(_, definition)| definition)
            .collect();
        self.live_items()
            .filter(|item| item.standing(policy).state_category() == Some(StateCategory::Review))
            .filter(|item| {
                let Some(ReviewStatus::Awaiting {
                    step: ReviewStep::Agent { requires },
                }) = self.review_status(item.id())
                else {
                    return false;
                };
                !staff.iter().any(|definition| {
                    requires
                        .iter()
                        .all(|need| definition.capabilities.contains(need))
                })
            })
            .map(Item::id)
            .collect()
    }

    /// Items waiting for a review step that `agent` could take: the step names capabilities the
    /// agent has, the agent never did the work on the item, and no review session holds the step.
    pub fn reviewable(&self, agent: &AgentId) -> Vec<(ItemId, Option<CommitId>)> {
        let Some(definition) = self.agent(&agent.name, agent.principal) else {
            return Vec::new();
        };
        let offered: BTreeSet<&Requirement> = definition.capabilities.iter().collect();
        let Some(policy) = self.policy().map(|document| &document.policy) else {
            return Vec::new();
        };
        let mut found: Vec<(&Item, Option<CommitId>)> = self
            .live_items()
            .filter(|item| item.standing(policy).state_category() == Some(StateCategory::Review))
            .filter_map(|item| {
                let (commit, _) = self.submission(item.id())?;
                let Some(ReviewStatus::Awaiting {
                    step: ReviewStep::Agent { requires },
                }) = self.review_status(item.id())
                else {
                    return None;
                };
                let capable = requires.iter().all(|need| offered.contains(need));
                let did_the_work = self.sessions().any(|session| {
                    session.item() == item.id()
                        && session.agent() == agent
                        && session.role() == SessionRole::Work
                });
                let held = self.live_sessions().any(|session| {
                    session.item() == item.id()
                        && matches!(session.role(), SessionRole::Review { .. })
                });
                let waits = self
                    .recovery(item.id(), SessionRole::Review { commit })
                    .waits_for_principal();
                (capable && !did_the_work && !held && !waits).then_some((item, commit))
            })
            .collect();
        found.sort_by_key(|(item, _)| (item.created(), item.id()));
        found
            .into_iter()
            .map(|(item, commit)| (item.id(), commit))
            .collect()
    }
}

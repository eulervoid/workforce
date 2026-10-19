//! Where the review of an item stands, worked out from its submitted work, its review level and
//! the verdicts on the submitted commit. Nothing here is stored: it is a function of the state.

use std::collections::BTreeSet;

use crate::crdt::Stamp;
use crate::id::ItemId;
use crate::identity::{Actor, AgentId, DeviceKey};
use crate::item::{Item, ItemEntry, ItemState};
use crate::name::Requirement;
use crate::policy::ReviewStep;
use crate::session::{CommitId, Outcome, SessionRole, Verdict};
use crate::state::State;

/// Work the daemon merges into the target, then moves its item to `done`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PendingMerge {
    pub item: ItemId,
    pub commit: CommitId,
}

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
        self.sessions_of(item)
            .filter(|session| session.role() == SessionRole::Work)
            .filter_map(|session| {
                let ended = session.ended()?;
                match ended.reason.outcome() {
                    Outcome::Submitted { commit } => Some((commit, ended.stamp)),
                    Outcome::Finished | Outcome::Failed | Outcome::Neither => None,
                }
            })
            .max_by_key(|(_, stamp)| *stamp)
    }

    /// Where the review of the item stands. `None` when nothing was submitted, or when the item
    /// needs no review and has not been merged.
    pub fn review_status(&self, item: ItemId) -> Option<ReviewStatus> {
        let policy = &self.policy()?.policy;
        let ItemEntry::Live(live) = self.item(item)? else {
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
        let level = policy.review_level(live.effective_review_level(policy)?)?;
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
            let by_agent = matches!(verdict.by, Actor::Agent { .. });
            let by_principal = matches!(verdict.by, Actor::Principal(_));
            match (verdict.verdict, steps[index].clone()) {
                // A principal decides in place of any step, an agent only for an agent step.
                (Verdict::Approve, ReviewStep::Agent { .. }) if by_agent || by_principal => {
                    index += 1;
                }
                (Verdict::Approve, ReviewStep::Principal) if by_principal => index += 1,
                (Verdict::RequestChanges, _) => return Some(ReviewStatus::ChangesRequested),
                (Verdict::Escalate, ReviewStep::Agent { .. }) if by_agent => {
                    index += 1;
                    steps.insert(index, ReviewStep::Principal);
                }
                // Such as an agent verdict on a principal step: it counts for no step.
                (Verdict::Approve | Verdict::Escalate, _) => {}
            }
        }
        Some(match steps.get(index) {
            Some(step) => ReviewStatus::Awaiting { step: step.clone() },
            None => ReviewStatus::Approved { commit },
        })
    }

    /// Items whose work is ready to merge, approved or needing no review, and waiting in
    /// `review`, of a type that uses `done`. Work merged already is among them until its item is
    /// moved to `done`, so that only the merge moves it, unless it was moved since.
    pub fn pending_merges(&self) -> Vec<PendingMerge> {
        let Some(policy) = self.policy().map(|document| &document.policy) else {
            return Vec::new();
        };
        self.live_items()
            .filter(|item| item.state(policy) == ItemState::Review)
            // Such as a check that fails on the target too: it waits until that is resolved.
            .filter(|item| !item.has_open_blocker())
            .filter_map(|item| {
                let (Some(commit), submitted) = self.submission(item.id())? else {
                    return None;
                };
                let ready = match self.review_status(item.id()) {
                    Some(ReviewStatus::Approved { .. }) => true,
                    Some(ReviewStatus::Merged { .. }) => item.moved() < submitted,
                    Some(ReviewStatus::Awaiting { .. } | ReviewStatus::ChangesRequested) => false,
                    None => !item.needs_review(policy),
                };
                if !ready {
                    return None;
                }
                self.state_if_used(item.id(), ItemState::Done)?;
                Some(PendingMerge {
                    item: item.id(),
                    commit,
                })
            })
            .collect()
    }

    /// Items whose review is over and that wait in `review` only to be moved to `done`: approved
    /// work submitted without commits, so there is nothing to merge. Merged work is moved by its
    /// merge, see [`State::pending_merges`]. An item that moved on after the submission was moved
    /// by someone, and is left where it is. Only items of types that use `done`.
    pub fn finished_reviews(&self) -> Vec<ItemId> {
        let Some(policy) = self.policy().map(|document| &document.policy) else {
            return Vec::new();
        };
        self.live_items()
            .filter(|item| item.state(policy) == ItemState::Review)
            .filter(|item| {
                matches!(
                    self.submission(item.id()),
                    Some((_, submitted)) if item.moved() < submitted
                ) && matches!(
                    self.review_status(item.id()),
                    Some(ReviewStatus::Approved { commit: None })
                )
            })
            .filter_map(|item| {
                self.state_if_used(item.id(), ItemState::Done)
                    .map(|_| item.id())
            })
            .collect()
    }

    /// Items that wait for a review step by an agent that no hosted agent the dispatcher starts has
    /// the capabilities for.
    pub fn unstaffed_reviews(&self) -> Vec<ItemId> {
        let Some(policy) = self.policy().map(|document| &document.policy) else {
            return Vec::new();
        };
        // What each agent that starts by itself offers on each device that hosts it.
        let staff: Vec<BTreeSet<Requirement>> = self
            .agents()
            .filter(|(_, definition)| definition.autonomy.start == crate::autonomy::StartMode::Auto)
            .flat_map(|(id, definition)| {
                self.devices()
                    .filter(move |device| {
                        device.principal == id.principal
                            && device
                                .profile
                                .is_some_and(|profile| profile.agents.contains_key(&id.name))
                    })
                    .map(move |device| self.offered(device.device, definition))
            })
            .collect();
        self.live_items()
            .filter(|item| item.state(policy) == ItemState::Review)
            .filter(|item| {
                let Some(ReviewStatus::Awaiting {
                    step: ReviewStep::Agent { requires },
                }) = self.review_status(item.id())
                else {
                    return false;
                };
                !staff
                    .iter()
                    .any(|offered| requires.iter().all(|need| offered.contains(need)))
            })
            .map(Item::id)
            .collect()
    }

    /// Items waiting for a review step that `agent` could take on `device`: the agent's
    /// capabilities and the device's hardware meet what the step requires, the agent never did
    /// the work on the item, and no review session holds the step.
    pub fn reviewable(
        &self,
        device: DeviceKey,
        agent: &AgentId,
    ) -> Vec<(ItemId, Option<CommitId>)> {
        let Some(definition) = self.agent(&agent.name, agent.principal) else {
            return Vec::new();
        };
        let offered = self.offered(device, definition);
        let Some(policy) = self.policy().map(|document| &document.policy) else {
            return Vec::new();
        };
        let mut found: Vec<(&Item, Option<CommitId>)> = self
            .live_items()
            .filter(|item| item.state(policy) == ItemState::Review)
            .filter_map(|item| {
                let (commit, _) = self.submission(item.id())?;
                let Some(ReviewStatus::Awaiting {
                    step: ReviewStep::Agent { requires },
                }) = self.review_status(item.id())
                else {
                    return None;
                };
                let capable = requires.iter().all(|need| offered.contains(need));
                let did_the_work = self
                    .sessions_of(item.id())
                    .any(|session| session.agent() == agent && session.role() == SessionRole::Work);
                let held = self.sessions_of(item.id()).any(|session| {
                    session.is_live() && matches!(session.role(), SessionRole::Review { .. })
                });
                let allowed = self
                    .recovery(item.id(), SessionRole::Review { commit })
                    .allows_attempt();
                (capable && !did_the_work && !held && allowed).then_some((item, commit))
            })
            .collect();
        found.sort_by_key(|(item, _)| (item.created(), item.id()));
        found
            .into_iter()
            .map(|(item, commit)| (item.id(), commit))
            .collect()
    }
}

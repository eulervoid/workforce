//! Where the review of an item stands, worked out from its submitted work, its review level and
//! the verdicts on the submitted commit. Nothing here is stored: it is a function of the state.

use std::collections::BTreeSet;

use crate::crdt::Stamp;
use crate::event::CommentKind;
use crate::hlc::Hlc;
use crate::id::{ItemId, SessionId};
use crate::identity::{Actor, AgentId, DeviceKey};
use crate::inbox::Stale;
use crate::item::{Item, ItemEntry, ItemState};
use crate::name::{Requirement, ReviewStepName};
use crate::policy::ReviewStep;
use crate::session::{CommitId, Outcome, Session, SessionRole, Verdict};
use crate::state::State;

/// Work the daemon merges into the target, then moves its item to `done`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PendingMerge {
    pub item: ItemId,
    pub commit: CommitId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReviewStatus {
    /// The steps of the current entry that still have no approval; they run at once.
    Awaiting {
        steps: Vec<ReviewStep>,
        /// The names of the agent steps of that entry that approved already.
        approved: Vec<Option<ReviewStepName>>,
    },
    ChangesRequested,
    /// An agent's review requested changes after the item had `rounds` rounds of them, as many as
    /// its review level allows: the work stays in review, and the principal decides with a verdict
    /// of their own. The other steps' verdicts on the commit count for nothing.
    RoundLimitReached {
        rounds: u32,
    },
    Approved {
        commit: Option<CommitId>,
    },
    /// The reviewed commit went into the target branch as `commit`.
    Merged {
        commit: CommitId,
    },
}

/// Why no review session starts for an item that awaits a review.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ReviewStartError {
    #[error("every open review step is held by a session or waits for the principal")]
    NoOpenStep,
    #[error("several review steps are open ({}); name one with --step", step_names(.open))]
    Ambiguous { open: Vec<Option<ReviewStepName>> },
    #[error("no open review step is called `{step}`; open: {}", step_names(.open))]
    NoSuchStep {
        step: ReviewStepName,
        open: Vec<Option<ReviewStepName>>,
    },
}

/// The names of review steps for a message; the step without one is `unnamed`.
pub fn step_names(steps: &[Option<ReviewStepName>]) -> String {
    steps
        .iter()
        .map(|step| step.as_ref().map_or("unnamed", |name| name.as_str()))
        .collect::<Vec<_>>()
        .join(", ")
}

impl State {
    /// What the latest submitted work session of the item handed in, and when it ended: its
    /// commit, or `None` when the branch had no commits, as for a finding that nothing has to change.
    pub fn submission(&self, item: ItemId) -> Option<(Option<CommitId>, Stamp)> {
        let (session, stamp) = self.submitting_session(item)?;
        Some((session.head(), stamp))
    }

    /// The latest work session of the item that submitted, and when it ended.
    pub(crate) fn submitting_session(&self, item: ItemId) -> Option<(&Session, Stamp)> {
        self.sessions_of(item)
            .filter(|session| session.role() == SessionRole::Work)
            .filter_map(|session| {
                let ended = session.ended()?;
                match ended.reason.outcome() {
                    Outcome::Submitted => Some((session, ended.stamp)),
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
        if let Some(merge) =
            commit.and_then(|commit| live.merges().find(|merge| merge.reviewed == commit))
        {
            return Some(ReviewStatus::Merged {
                commit: merge.commit,
            });
        }
        let level = policy.review_level(live.effective_review_level(policy)?)?;
        let mut progress = Progress::new(level.steps.clone());
        let mut verdicts: Vec<_> = live
            .verdicts()
            .values()
            .filter(|verdict| verdict.commit == commit && verdict.stamp > submitted)
            .collect();
        verdicts.sort_by_key(|verdict| verdict.stamp);
        let mut round_limit_reached = None;
        for verdict in verdicts {
            if progress.is_done() {
                break;
            }
            if round_limit_reached.is_some() && !matches!(verdict.by, Actor::Principal(_)) {
                continue;
            }
            match (verdict.verdict, &verdict.by) {
                (Verdict::RequestChanges, _) => match verdict.round_limit_reached {
                    None => return Some(ReviewStatus::ChangesRequested),
                    Some(rounds) => round_limit_reached = Some(rounds),
                },
                // A principal decides for the whole entry, at the round limit too.
                (Verdict::Approve, Actor::Principal(_)) => {
                    round_limit_reached = None;
                    progress.decided_by_principal();
                }
                (Verdict::Approve, Actor::Agent { session, .. }) => {
                    progress.close(self.step_of(*session), false);
                }
                (Verdict::Escalate, Actor::Agent { session, .. }) => {
                    progress.close(self.step_of(*session), true);
                }
                // Such as an escalation by a principal, or a verdict of the daemon: it counts for
                // no step.
                (Verdict::Approve | Verdict::Escalate, _) => {}
            }
        }
        Some(if let Some(rounds) = round_limit_reached {
            ReviewStatus::RoundLimitReached { rounds }
        } else if progress.is_done() {
            ReviewStatus::Approved { commit }
        } else {
            ReviewStatus::Awaiting {
                steps: progress.open,
                approved: progress.approved,
            }
        })
    }

    /// The name of the step a review session was started for.
    fn step_of(&self, session: SessionId) -> Option<ReviewStepName> {
        match self.session(session)?.role() {
            SessionRole::Review { step, .. } => step,
            SessionRole::Work | SessionRole::Supervise | SessionRole::Diagnose => None,
        }
    }

    /// The open steps of the review of `commit` of `item` that no live review session holds, but
    /// for the `expired` ones ([`State::expired`]).
    fn unheld(
        &self,
        item: ItemId,
        commit: Option<CommitId>,
        open: Vec<ReviewStep>,
        expired: &BTreeSet<SessionId>,
    ) -> Vec<ReviewStep> {
        open.into_iter()
            .filter(|step| match step {
                ReviewStep::Agent { name, .. } => {
                    let role = SessionRole::Review {
                        commit,
                        step: name.clone(),
                    };
                    !self.held(item, &role, expired)
                }
                ReviewStep::Principal => true,
            })
            .collect()
    }

    /// The review steps on offer: items in `review` with submitted commits that have an open step
    /// no review session holds. Each item is offered once, with its first such step. These are the
    /// items whose branch the daemon checks against the target before anyone spends a review on it.
    pub fn offered_reviews(&self) -> Vec<(ItemId, CommitId, ReviewStep)> {
        self.live_items()
            .filter(|item| item.state() == ItemState::Review)
            .filter_map(|item| {
                let (Some(commit), _) = self.submission(item.id())? else {
                    return None;
                };
                match self.review_status(item.id())? {
                    ReviewStatus::Awaiting { steps, .. } => self
                        .unheld(item.id(), Some(commit), steps, &BTreeSet::new())
                        .into_iter()
                        .next()
                        .map(|step| (item.id(), commit, step)),
                    ReviewStatus::ChangesRequested
                    | ReviewStatus::RoundLimitReached { .. }
                    | ReviewStatus::Approved { .. }
                    | ReviewStatus::Merged { .. } => None,
                }
            })
            .collect()
    }

    /// The commit under review and the names of the agent steps a review session could be started
    /// for by hand: the open steps of the item that no live review session holds. `None` when the
    /// item is not in `review` with submitted work that awaits a verdict.
    pub fn startable_reviews(
        &self,
        item: ItemId,
    ) -> Option<(Option<CommitId>, Vec<Option<ReviewStepName>>)> {
        if self.live_state(item)? != ItemState::Review {
            return None;
        }
        let (commit, _) = self.submission(item)?;
        let ReviewStatus::Awaiting { steps, .. } = self.review_status(item)? else {
            return None;
        };
        let names = self
            .unheld(item, commit, steps, &BTreeSet::new())
            .into_iter()
            .filter_map(|step| match step {
                ReviewStep::Agent { name, .. } => Some(name),
                ReviewStep::Principal => None,
            })
            .collect();
        Some((commit, names))
    }

    /// The review session to start by hand on `item`, for `step` when the entry has several steps
    /// open; no `step` stands for the unnamed one. `None` when the item awaits no review, so that a work session starts.
    pub fn review_to_start(
        &self,
        item: ItemId,
        step: Option<&ReviewStepName>,
    ) -> Option<Result<SessionRole, ReviewStartError>> {
        let (commit, open) = self.startable_reviews(item)?;
        let chosen = match (step, open.as_slice()) {
            (_, []) => Err(ReviewStartError::NoOpenStep),
            (None, [only]) => Ok(only.clone()),
            // Without a name, the step without one.
            (None, _) if open.contains(&None) => Ok(None),
            (None, _) => Err(ReviewStartError::Ambiguous { open }),
            (Some(wanted), _) if open.contains(&Some(wanted.clone())) => Ok(Some(wanted.clone())),
            (Some(wanted), _) => Err(ReviewStartError::NoSuchStep {
                step: wanted.clone(),
                open,
            }),
        };
        Some(chosen.map(|step| SessionRole::Review { commit, step }))
    }

    /// When the daemon found, after `submitted`, that the work of `item` no longer merges into
    /// the target: its latest change request on the item that is still open.
    pub fn stale_since(&self, item: ItemId, submitted: Hlc) -> Option<Stale> {
        let ItemEntry::Live(live) = self.item(item)? else {
            return None;
        };
        live.comments()
            .iter()
            .filter(|(_, comment)| {
                comment.kind == CommentKind::ChangeRequest
                    && comment.is_open()
                    && matches!(comment.by, Actor::Daemon(_))
                    && comment.at > submitted
            })
            .max_by_key(|(_, comment)| comment.at)
            .map(|(id, comment)| Stale {
                comment: *id,
                since_ms: comment.at.wall_ms(),
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
            .filter(|item| item.state() == ItemState::Review)
            // Such as a check that fails on the target too: it waits until that is resolved.
            .filter(|item| !item.has_open_blocker())
            .filter_map(|item| {
                let (Some(commit), submitted) = self.submission(item.id())? else {
                    return None;
                };
                let ready = match self.review_status(item.id()) {
                    Some(ReviewStatus::Approved { .. }) => true,
                    Some(ReviewStatus::Merged { .. }) => item.moved() < submitted,
                    Some(
                        ReviewStatus::Awaiting { .. }
                        | ReviewStatus::ChangesRequested
                        | ReviewStatus::RoundLimitReached { .. },
                    ) => false,
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
        self.live_items()
            .filter(|item| item.state() == ItemState::Review)
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
    /// the capabilities for, with the names of those steps; steps without a name are not listed.
    pub fn unstaffed_reviews(&self) -> Vec<(ItemId, Vec<ReviewStepName>)> {
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
            .filter(|item| item.state() == ItemState::Review)
            .filter_map(|item| {
                let Some(ReviewStatus::Awaiting { steps, .. }) = self.review_status(item.id())
                else {
                    return None;
                };
                let unstaffed: Vec<_> = steps
                    .into_iter()
                    .filter_map(|step| match step {
                        ReviewStep::Agent { name, requires, .. } => (!staff
                            .iter()
                            .any(|offered| requires.iter().all(|need| offered.contains(need))))
                        .then_some(name),
                        ReviewStep::Principal => None,
                    })
                    .collect();
                (!unstaffed.is_empty())
                    .then(|| (item.id(), unstaffed.into_iter().flatten().collect()))
            })
            .collect()
    }

    /// The review steps `agent` could take on `device`, with the item and commit they are for: the
    /// agent's capabilities and the device's hardware meet what the step requires, and no review
    /// session holds the step, but for the `expired` ones ([`State::expired`]). Not offered an
    /// item are agents that did its work or gave a verdict on the submitted commit. Steps of the
    /// same name are one step, offered once.
    pub fn reviewable(
        &self,
        device: DeviceKey,
        agent: &AgentId,
        expired: &BTreeSet<SessionId>,
    ) -> Vec<(ItemId, Option<CommitId>, Option<ReviewStepName>)> {
        let Some(definition) = self.agent(&agent.name, agent.principal) else {
            return Vec::new();
        };
        let offered = self.offered(device, definition);
        let mut found: Vec<(&Item, Option<CommitId>, Option<ReviewStepName>)> = Vec::new();
        for item in self
            .live_items()
            .filter(|item| item.state() == ItemState::Review)
        {
            let Some((commit, submitted)) = self.submission(item.id()) else {
                continue;
            };
            let Some(ReviewStatus::Awaiting { steps, .. }) = self.review_status(item.id()) else {
                continue;
            };
            let did_the_work = self
                .sessions_of(item.id())
                .any(|session| session.agent() == agent && session.role() == SessionRole::Work);
            let gave_a_verdict = item.verdicts().values().any(|verdict| {
                verdict.commit == commit
                    && verdict.stamp > submitted
                    && matches!(&verdict.by, Actor::Agent { agent: by, .. } if by == agent)
            });
            if did_the_work || gave_a_verdict {
                continue;
            }
            let mut offered_names = BTreeSet::new();
            for step in self.unheld(item.id(), commit, steps, expired) {
                let ReviewStep::Agent { name, requires, .. } = step else {
                    continue;
                };
                let capable = requires.iter().all(|need| offered.contains(need));
                let allowed = self
                    .recovery(
                        item.id(),
                        &SessionRole::Review {
                            commit,
                            step: name.clone(),
                        },
                    )
                    .allows_attempt();
                if capable && allowed && offered_names.insert(name.clone()) {
                    found.push((item, commit, name));
                }
            }
        }
        found.sort_by_key(|(item, _, _)| (item.created(), item.id()));
        found
            .into_iter()
            .map(|(item, commit, step)| (item.id(), commit, step))
            .collect()
    }
}

/// Where the verdicts have got to in the entries of a review level.
struct Progress {
    entries: Vec<Vec<ReviewStep>>,
    /// The entry the verdicts are for.
    index: usize,
    /// The steps of that entry without an approval.
    open: Vec<ReviewStep>,
    /// The agent steps of that entry that approved.
    approved: Vec<Option<ReviewStepName>>,
    /// A step of the entry escalated: a principal step follows the entry.
    escalated: bool,
}

impl Progress {
    fn new(entries: Vec<Vec<ReviewStep>>) -> Self {
        let mut progress = Self {
            open: Vec::new(),
            approved: Vec::new(),
            entries,
            index: 0,
            escalated: false,
        };
        progress.enter();
        progress
    }

    fn is_done(&self) -> bool {
        self.index >= self.entries.len()
    }

    /// Opens the steps of the entry at `index`, skipping entries without any.
    fn enter(&mut self) {
        self.approved.clear();
        while let Some(entry) = self.entries.get(self.index) {
            if !entry.is_empty() {
                self.open = entry.clone();
                return;
            }
            self.index += 1;
        }
        self.open.clear();
    }

    fn next_entry(&mut self) {
        self.index += 1;
        self.enter();
    }

    /// Closes an open agent step of this name, if there is one, and notes that it escalated.
    fn close(&mut self, name: Option<ReviewStepName>, escalating: bool) {
        let position = self
            .open
            .iter()
            .position(|step| matches!(step, ReviewStep::Agent { name: open, .. } if *open == name));
        let Some(position) = position else {
            return;
        };
        self.open.remove(position);
        if !escalating {
            self.approved.push(name);
        }
        self.escalated |= escalating;
        self.advance_when_closed();
    }

    /// Moves on to the next entry once every step is closed; an escalation adds a principal entry.
    fn advance_when_closed(&mut self) {
        if !self.open.is_empty() {
            return;
        }
        self.index += 1;
        if std::mem::take(&mut self.escalated) {
            self.entries.insert(self.index, vec![ReviewStep::Principal]);
        }
        self.enter();
    }

    fn decided_by_principal(&mut self) {
        self.escalated = false;
        self.next_entry();
    }
}

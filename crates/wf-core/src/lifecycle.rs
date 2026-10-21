//! What follows from a session starting, ending and finishing a turn. Pure functions of the
//! state: the daemon asks and then writes the events.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::event::CommentKind;
use crate::graph::DependencyGraph;
use crate::hlc::Hlc;
use crate::id::{CommentId, ItemId, SessionId};
use crate::identity::Actor;
use crate::item::{Item, ItemEntry, ItemState};
use crate::policy::Policy;
use crate::review::ReviewStatus;
use crate::session::{CommitId, EndReason, MachineFault, Outcome, SessionRole, Verdict};
use crate::state::State;

/// How an agent ends its session. The session ends after the agent's turn, and the agent writes
/// the end.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Finish {
    /// The work is done.
    Submit { note: Option<String> },
    /// The agent gives the item back.
    Release { note: Option<String> },
    /// A review session has given its verdict.
    Reviewed,
    /// A supervise session has resolved or escalated what it was started for.
    Supervised,
    /// A diagnose session has said what went wrong.
    Diagnosed,
}

/// Why a session ends now, without waiting for the agent. The daemon writes the end.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Interruption {
    /// The principal stops it.
    Stop,
    /// Someone moved the item to a state where it waits, such as review, while the agent worked.
    MovedOn,
    /// It waited too long or ran too long.
    TimedOut,
    /// The harness failed or exited, or the daemon could not run it.
    Failed { because: String },
    /// The daemon that ran it stopped; a later daemon ends the sessions it left live.
    Interrupted,
    /// The agent used up its nudges without progress. `last_message` is what it said last.
    GaveUp { turns: u32, last_message: String },
    /// The machine cannot carry the session. Nothing is commented: the inbox says once what is
    /// wrong with the machine, for all the sessions it ends.
    MachineFault(MachineFault),
    /// Its device was gone for longer than the lease, and a daemon of its principal takes over
    /// what it claimed.
    LeaseExpired,
    /// An earlier claim on the same thing arrived.
    Outclaimed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionEnd {
    Finish(Finish),
    Interruption(Interruption),
}

/// What is written when a session ends: the comment `note` by `actor`, then the end.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Closing {
    pub reason: EndReason,
    pub actor: Actor,
    pub note: Option<String>,
    /// For a work session, where its branch is: what a submission hands in, `None` when the branch
    /// has no commits of its own.
    pub head: Option<CommitId>,
}

/// Why reviewed work could not be merged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeFailure {
    /// The work conflicts with the target: the item goes back to work, and the next submission
    /// rebases the branch onto the target.
    Conflict,
    /// It may merge later: the target's checkout has changes, the target kept moving, or git
    /// failed. The dispatcher tries again.
    Transient,
}

/// What follows when checks fail on the merge result of an item's work, by its own fault.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AfterFailedChecks {
    /// Each failed check becomes a change request and the item goes back to work, to `to`.
    BackToWork { to: ItemState },
    /// The checks failed `runs` times already since the principal last acted on the item: the
    /// principal decides instead.
    WaitsForPrincipal { runs: usize },
}

/// How many failed check runs send an item back to work before the next waits for the principal.
const FAILED_CHECK_RUNS_LIMIT: usize = 2;

/// Where a request for changes sends the submitted work of an item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AfterChangesRequested {
    /// Back to work, in the same event: what every request for changes does under a review level
    /// without `max_rounds`, and a principal's always.
    BackToWork,
    /// An agent's review requests changes after the item had `rounds` rounds of them since the
    /// principal last acted on it, as many as its review level's `max_rounds` or more: the item
    /// stays in review and waits for the principal.
    WaitsForPrincipal { rounds: u32 },
}

/// How much of the last words of an agent that gave up goes into the comment, in characters.
const LAST_MESSAGE_LIMIT: usize = 600;

/// What happens to the item of a work session that ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AfterEnd {
    /// The item stays where it is.
    Nothing,
    MoveTo(ItemState),
    /// The work needs no review: move the item to `wait_in` while the merge of `commit` waits
    /// for its turn, as a merge that may succeed later does, then merge it and move the item to
    /// `done`.
    MergeThenDone {
        commit: CommitId,
        wait_in: Option<ItemState>,
    },
}

/// What a session that continues after the daemon restarted is told first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resume {
    /// The session was working: the daemon restarted under it.
    AfterRestart,
    /// The session's last turn asked a blocker, so it was parked when the daemon stopped, even if
    /// the blocker is answered by now.
    AfterWait,
}

/// What to do for a request to merge the reviewed work of an item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergePlan {
    /// The item is done: another request finished the merge, or the item was finished otherwise.
    Nothing,
    /// The commit is in the target branch already, as when the daemon stopped before it moved
    /// the item: move the item to `done`.
    MoveToDone,
    /// Merge the commit into the target branch, clean up the branch and move the item to `done`.
    Merge,
}

impl MergePlan {
    /// What the daemon does for a request to merge `commit`, the commit that was reviewed, of
    /// `item`.
    pub fn of(item: &Item, policy: &Policy, commit: CommitId) -> MergePlan {
        if item.state(policy) == ItemState::Done {
            MergePlan::Nothing
        } else if item.merges().any(|merged| merged.reviewed == commit) {
            MergePlan::MoveToDone
        } else {
            MergePlan::Merge
        }
    }
}

/// Which review rounds [`State::review_rounds`] counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rounds {
    All,
    /// Not the latest submission: a verdict on it decides as the first one on it did.
    BeforeThisSubmission,
}

/// What to do when a turn of a session has ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnOutcome {
    /// The session already ended, or its item was moved on: to review, done, cancelled or back to
    /// draft, or archived.
    Close,
    /// The item waits on a dependency or an open blocker, such as the session's own question.
    Park,
    /// Prompt again: continue, and submit, ask or release when done or stuck.
    Continue,
    /// The nudges are used up without a new commit.
    GiveUp,
    /// The machine is at fault: the session ends without using up the nudges, and the item is
    /// tried again once the machine is fine.
    MachineFault(MachineFault),
}

impl State {
    /// What is written when `session` ends. `head` is the head of the session's branch when the
    /// branch has commits of its own; a work session records it, whatever the reason it ends for,
    /// except a takeover: another device writes `LeaseExpired` and cannot know the branch.
    /// `None` for a session that does not exist or a device that is not bound.
    pub fn closing(
        &self,
        session: SessionId,
        end: &SessionEnd,
        head: Option<CommitId>,
    ) -> Option<Closing> {
        let session = self.session(session)?;
        let head = match (session.role(), end) {
            (_, SessionEnd::Interruption(Interruption::LeaseExpired)) => None,
            (SessionRole::Work, _) => head,
            (SessionRole::Review { .. } | SessionRole::Supervise | SessionRole::Diagnose, _) => {
                None
            }
        };
        let closing = |reason, actor, note| Closing {
            reason,
            actor,
            note,
            head,
        };
        Some(match end {
            SessionEnd::Finish(finish) => {
                let by_agent = session.actor();
                match finish {
                    Finish::Submit { note } => {
                        closing(EndReason::Submitted, by_agent, note.clone())
                    }
                    Finish::Release { note } => {
                        closing(EndReason::Released, by_agent, note.clone())
                    }
                    Finish::Reviewed => closing(EndReason::Reviewed, by_agent, None),
                    Finish::Supervised => closing(EndReason::Supervised, by_agent, None),
                    Finish::Diagnosed => closing(EndReason::Diagnosed, by_agent, None),
                }
            }
            SessionEnd::Interruption(interruption) => {
                let by_daemon = Actor::Daemon(self.principal_of(&session.device())?);
                match interruption {
                    Interruption::Stop | Interruption::MovedOn => {
                        closing(EndReason::Cancelled, by_daemon, None)
                    }
                    Interruption::TimedOut => closing(EndReason::TimedOut, by_daemon, None),
                    Interruption::Interrupted => closing(EndReason::Interrupted, by_daemon, None),
                    Interruption::LeaseExpired => closing(EndReason::LeaseExpired, by_daemon, None),
                    Interruption::Outclaimed => closing(EndReason::Outclaimed, by_daemon, None),
                    Interruption::MachineFault(fault) => {
                        closing(EndReason::MachineFault(*fault), by_daemon, None)
                    }
                    Interruption::Failed { because } => closing(
                        EndReason::Failed,
                        by_daemon,
                        Some(format!("The session failed: {because}")),
                    ),
                    Interruption::GaveUp {
                        turns,
                        last_message,
                    } => {
                        let without = match session.role() {
                            SessionRole::Work => "a new commit",
                            SessionRole::Review { .. } => "a verdict",
                            SessionRole::Supervise => "an answer",
                            SessionRole::Diagnose => "a report",
                        };
                        let gave_up = format!("Gave up: {turns} turns in a row without {without}.");
                        let note = match last_message.trim() {
                            "" => gave_up,
                            said => format!(
                                "{gave_up} The agent last said:\n\n{}",
                                truncated(said, LAST_MESSAGE_LIMIT)
                            ),
                        };
                        closing(EndReason::Released, by_daemon, Some(note))
                    }
                }
            }
        })
    }

    /// Where `item` goes when its reviewed work cannot be merged: back to `open` for a conflict,
    /// which the next session resolves as it submits, and otherwise to `review`, where the
    /// dispatcher tries again. `None` when its type does not use the state.
    pub fn after_failed_merge(&self, item: ItemId, failure: MergeFailure) -> Option<ItemState> {
        match failure {
            MergeFailure::Conflict => self.state_if_used(item, ItemState::Open),
            MergeFailure::Transient => self.state_if_used(item, ItemState::Review),
        }
    }

    /// What follows when checks fail on the merge result of `item` by its own fault: back to work,
    /// unless [`State::failed_check_runs`] reached the limit or its type has no `open` to go back
    /// to.
    pub fn after_failed_checks(&self, item: ItemId) -> AfterFailedChecks {
        let runs = self.failed_check_runs(item);
        match self.state_if_used(item, ItemState::Open) {
            Some(open) if runs < FAILED_CHECK_RUNS_LIMIT => {
                AfterFailedChecks::BackToWork { to: open }
            }
            Some(_) | None => AfterFailedChecks::WaitsForPrincipal { runs },
        }
    }

    /// Where a request for changes by `by` on the submitted work of `item` sends it, by the
    /// review rounds the item had before this submission. Once a request waits for the principal,
    /// those of the other steps on the submission wait too: only the principal's verdict, a move or
    /// a cancel ends the hold, not a comment that starts the count over. The verdict records the
    /// answer, so the fold does not depend on the policy.
    pub fn after_changes_requested(&self, item: ItemId, by: &Actor) -> AfterChangesRequested {
        if let (Actor::Agent { .. }, Some(ReviewStatus::RoundLimitReached { rounds })) =
            (by, self.review_status(item))
        {
            return AfterChangesRequested::WaitsForPrincipal { rounds };
        }
        let max_rounds = self.policy().and_then(|document| {
            let policy = &document.policy;
            let Some(ItemEntry::Live(live)) = self.item(item) else {
                return None;
            };
            policy
                .review_level(live.effective_review_level(policy)?)?
                .max_rounds
        });
        match (by, max_rounds) {
            (Actor::Agent { .. }, Some(max_rounds)) => {
                let rounds = self.review_rounds(item, Rounds::BeforeThisSubmission);
                if rounds >= max_rounds.get() {
                    AfterChangesRequested::WaitsForPrincipal { rounds }
                } else {
                    AfterChangesRequested::BackToWork
                }
            }
            (Actor::Agent { .. }, None) | (Actor::Principal(_) | Actor::Daemon(_), _) => {
                AfterChangesRequested::BackToWork
            }
        }
    }

    /// What a verdict by `by` on the submitted work of `item` records as its
    /// [`Reviewed::round_limit_reached`](crate::event::Reviewed::round_limit_reached).
    pub fn round_limit_reached(&self, item: ItemId, verdict: Verdict, by: &Actor) -> Option<u32> {
        match verdict {
            Verdict::RequestChanges => match self.after_changes_requested(item, by) {
                AfterChangesRequested::BackToWork => None,
                AfterChangesRequested::WaitsForPrincipal { rounds } => Some(rounds),
            },
            Verdict::Approve | Verdict::Escalate => None,
        }
    }

    /// The change requests that a verdict by `by` on the submitted work of `item` overrules, for
    /// its writer to resolve: when the principal approves at the round limit, the agents' open
    /// ones, oldest first. Left open, they would keep the next work session from submitting if
    /// the merge sent the work back.
    pub fn overruled_change_requests(
        &self,
        item: ItemId,
        verdict: Verdict,
        by: &Actor,
    ) -> Vec<CommentId> {
        let at_round_limit = matches!(
            self.review_status(item),
            Some(ReviewStatus::RoundLimitReached { .. })
        );
        let Some(ItemEntry::Live(live)) = self.item(item) else {
            return Vec::new();
        };
        match (verdict, by) {
            (Verdict::Approve, Actor::Principal(_)) if at_round_limit => {
                let mut open: Vec<_> = live
                    .comments()
                    .iter()
                    .filter(|(_, comment)| {
                        comment.kind.is_change_request()
                            && comment.is_open()
                            && matches!(comment.by, Actor::Agent { .. })
                    })
                    .map(|(id, comment)| (comment.at, *id))
                    .collect();
                open.sort();
                open.into_iter().map(|(_, id)| id).collect()
            }
            (Verdict::Approve | Verdict::RequestChanges | Verdict::Escalate, _) => Vec::new(),
        }
    }

    /// The review rounds of `item` since the principal last acted on it: its submissions that an
    /// agent's review sent back with a request for changes. Several steps that request changes on
    /// one submission make one round; a request that waits for the principal makes none, nor do
    /// those of the principal and of the daemon.
    pub fn review_rounds(&self, item: ItemId, which: Rounds) -> u32 {
        let Some(ItemEntry::Live(live)) = self.item(item) else {
            return 0;
        };
        let mut submitted: Vec<Hlc> = self
            .sessions_of(item)
            .filter(|session| session.role() == SessionRole::Work)
            .filter_map(|session| {
                let ended = session.ended()?;
                match ended.reason.outcome() {
                    Outcome::Submitted => Some(ended.at()),
                    Outcome::Finished | Outcome::Failed | Outcome::Neither => None,
                }
            })
            .collect();
        submitted.sort();
        let since = live.principal_touched();
        let sent_back: BTreeSet<usize> = live
            .verdicts()
            .values()
            .filter(|review| {
                review.verdict == Verdict::RequestChanges
                    && review.round_limit_reached.is_none()
                    && matches!(review.by, Actor::Agent { .. })
                    && since.is_none_or(|since| review.stamp.hlc > since)
            })
            // The submissions before the verdict: the last of them is the one it reviewed.
            .map(|review| submitted.partition_point(|at| *at < review.stamp.hlc))
            .filter(|before| match which {
                Rounds::All => true,
                Rounds::BeforeThisSubmission => *before < submitted.len(),
            })
            .collect();
        u32::try_from(sent_back.len()).unwrap_or(u32::MAX)
    }

    /// How many check runs on the merge result of `item` failed since the principal last acted on
    /// it, at its submission or at its merge. The daemon tells a failed run as one check failure
    /// naming every check that failed.
    pub fn failed_check_runs(&self, item: ItemId) -> usize {
        let Some(ItemEntry::Live(live)) = self.item(item) else {
            return 0;
        };
        let since = live.principal_touched();
        live.comments()
            .values()
            .filter(|comment| {
                comment.kind == CommentKind::CheckFailure
                    && since.is_none_or(|since| comment.at > since)
            })
            .count()
    }

    /// The state the daemon moves the item of a work session to when the session starts:
    /// `active`, if its type uses it.
    pub fn start_move(&self, session: SessionId) -> Option<(ItemId, ItemState)> {
        let session = self.session(session)?;
        if session.role() != SessionRole::Work {
            return None;
        }
        let active = self.state_if_used(session.item(), ItemState::Active)?;
        Some((session.item(), active))
    }

    /// Whether the item's branch is rebased onto the target as `session` submits (by `wf work
    /// submit`, in the agent's repository), before anything is written: for work on an item that is being worked on. Nobody has approved its
    /// commits yet, so rewriting them costs nothing, and the reviewer sees them as they will land.
    /// Approved work, which waits in review for its merge, is never rewritten.
    pub fn rebases_at_submit(&self, session: SessionId) -> bool {
        let Some(session) = self.session(session) else {
            return false;
        };
        let working = match session.role() {
            SessionRole::Work => true,
            SessionRole::Review { .. } | SessionRole::Supervise | SessionRole::Diagnose => false,
        };
        let at_work = match self.live_state(session.item()) {
            Some(ItemState::Open | ItemState::Active) => true,
            Some(ItemState::Review | ItemState::Done | ItemState::Cancelled | ItemState::Draft)
            | None => false,
        };
        working && at_work
    }

    /// What happens to the item of `session` when it ends for `reason`. The daemon does it before
    /// it writes the end, while the session still holds the item, so the item is never ready in
    /// between. `head` is the one the end names. `None` for a session that does not exist.
    pub fn after_end(
        &self,
        session: SessionId,
        reason: EndReason,
        head: Option<CommitId>,
    ) -> Option<AfterEnd> {
        let session = self.session(session)?;
        if session.role() != SessionRole::Work {
            return Some(AfterEnd::Nothing);
        }
        let item = session.item();
        let used = |state| self.state_if_used(item, state);
        let move_to = |state: Option<ItemState>| state.map_or(AfterEnd::Nothing, AfterEnd::MoveTo);
        Some(match reason {
            EndReason::Submitted => match (self.needs_review(item), head) {
                (true, _) => move_to(used(ItemState::Review)),
                (false, Some(commit)) => match used(ItemState::Done) {
                    Some(_) => AfterEnd::MergeThenDone {
                        commit,
                        wait_in: self.after_failed_merge(item, MergeFailure::Transient),
                    },
                    None => AfterEnd::Nothing,
                },
                (false, None) => move_to(used(ItemState::Done)),
            },
            EndReason::Reviewed | EndReason::Supervised | EndReason::Diagnosed => AfterEnd::Nothing,
            EndReason::Released
            | EndReason::TimedOut
            | EndReason::Failed
            | EndReason::Cancelled
            | EndReason::Interrupted
            | EndReason::MachineFault(_)
            | EndReason::LeaseExpired
            | EndReason::Outclaimed => {
                let held_by_another = self
                    .holder(item, &SessionRole::Work)
                    .is_some_and(|holder| holder.id() != session.id());
                // Only an item that is being worked on goes back: one that was moved on, to done
                // or to review, stays where it is.
                if held_by_another || self.live_state(item) != Some(ItemState::Active) {
                    AfterEnd::Nothing
                } else {
                    move_to(used(ItemState::Open))
                }
            }
        })
    }

    /// What to do after a turn. `nudges_used` counts the turns in a row that ended without a new
    /// commit on the branch, this one included. `machine_fault` is what is wrong with the machine
    /// the session runs on, if anything: a turn on a machine at fault says nothing about the work.
    pub fn turn_outcome(
        &self,
        session: SessionId,
        nudges_used: u32,
        nudges_allowed: u32,
        machine_fault: Option<MachineFault>,
    ) -> TurnOutcome {
        let Some(folded) = self.session(session).filter(|session| session.is_live()) else {
            return TurnOutcome::Close;
        };
        let item = folded.item();
        // Only work waits: the other roles act on an item that waits for them.
        let working = match folded.role() {
            SessionRole::Work => true,
            SessionRole::Review { .. } | SessionRole::Supervise | SessionRole::Diagnose => false,
        };
        let moved_on = match self.live_state(item) {
            Some(ItemState::Open | ItemState::Active) => false,
            Some(ItemState::Review | ItemState::Done | ItemState::Cancelled | ItemState::Draft)
            | None => true,
        };
        if working && moved_on {
            return TurnOutcome::Close;
        }
        // A review of a commit that new work replaced has no verdict left to give.
        if let SessionRole::Review { commit, .. } = folded.role()
            && self
                .submission(item)
                .is_none_or(|(submitted, _)| submitted != commit)
        {
            return TurnOutcome::Close;
        }
        if let Some(fault) = machine_fault {
            return TurnOutcome::MachineFault(fault);
        }
        if self.session_waits(session, &self.dependency_graph()) {
            return TurnOutcome::Park;
        }
        if nudges_used > nudges_allowed {
            TurnOutcome::GiveUp
        } else {
            TurnOutcome::Continue
        }
    }

    /// Whether `session` is a live work session whose item waits on a dependency or on an open
    /// blocker, such as the session's own question. The runner parks such a session after its
    /// turn, and the dashboards show it as parked. `graph` is the dependency graph of this state,
    /// which callers asking about many sessions build once.
    pub fn session_waits(&self, session: SessionId, graph: &DependencyGraph) -> bool {
        let Some(folded) = self.session(session) else {
            return false;
        };
        if !folded.is_live() || folded.role() != SessionRole::Work {
            return false;
        }
        let item = folded.item();
        let blocked_by_comment = match self.item(item) {
            Some(ItemEntry::Live(live)) => live.has_open_blocker(),
            Some(ItemEntry::Archived(_)) | None => false,
        };
        blocked_by_comment || !self.unfinished_dependencies(graph, item).is_empty()
    }

    /// What `session` is told first after a restart of the daemon, which last prompted it at
    /// `prompted_ms`, in milliseconds since the epoch.
    pub fn resume_after_restart(&self, session: SessionId, prompted_ms: u64) -> Resume {
        let Some(folded) = self.session(session) else {
            return Resume::AfterRestart;
        };
        let asked = match self.item(folded.item()) {
            Some(ItemEntry::Live(item)) => item.comments().values().any(|comment| {
                comment.session() == Some(session)
                    && comment.kind == CommentKind::Blocker
                    && comment.at.wall_ms() >= prompted_ms
            }),
            Some(ItemEntry::Archived(_)) | None => false,
        };
        if asked {
            Resume::AfterWait
        } else {
            Resume::AfterRestart
        }
    }

    /// The state of a live item; `None` for an archived one.
    pub fn live_state(&self, item: ItemId) -> Option<ItemState> {
        let policy = &self.policy()?.policy;
        match self.item(item)? {
            ItemEntry::Live(item) => Some(item.state(policy)),
            ItemEntry::Archived(_) => None,
        }
    }

    /// `state`, when the type of the live `item` uses it.
    pub(crate) fn state_if_used(&self, item: ItemId, state: ItemState) -> Option<ItemState> {
        let policy = &self.policy()?.policy;
        let ItemEntry::Live(item) = self.item(item)? else {
            return None;
        };
        policy
            .type_config(item.item_type())?
            .uses(state)
            .then_some(state)
    }

    fn needs_review(&self, item: ItemId) -> bool {
        let Some(policy) = self.policy().map(|document| &document.policy) else {
            return false;
        };
        match self.item(item) {
            Some(ItemEntry::Live(item)) => item.needs_review(policy),
            Some(ItemEntry::Archived(_)) | None => false,
        }
    }
}

/// At most `limit` characters of `text`, cut at a character boundary.
fn truncated(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

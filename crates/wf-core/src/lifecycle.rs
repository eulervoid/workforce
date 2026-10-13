//! What follows from a session starting, ending and finishing a turn. Pure functions of the
//! state: the daemon asks and then writes the events.

use serde::{Deserialize, Serialize};

use crate::id::{ItemId, SessionId};
use crate::identity::Actor;
use crate::item::ItemView;
use crate::name::StateName;
use crate::policy::StateCategory;
use crate::session::{CommitId, EndReason, SessionRole};
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
}

/// Why reviewed work could not be merged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeFailure {
    /// The work conflicts with the target: a session has to merge the target into the branch.
    Conflict,
    /// It may merge later: the target's checkout has changes, the target kept moving, or git
    /// failed. The dispatcher tries again.
    Transient,
}

/// How much of the last words of an agent that gave up goes into the comment, in characters.
const LAST_MESSAGE_LIMIT: usize = 600;

/// What happens to the item of a work session that ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AfterEnd {
    /// The item stays where it is.
    Nothing,
    MoveTo(StateName),
    /// The work needs no review: merge it, then move the item to `done`.
    MergeThen {
        done: StateName,
    },
}

/// What to do when a turn of a session has ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnOutcome {
    /// The session already ended, or its item was moved on: to review, done, cancelled or the
    /// backlog, or archived.
    Close,
    /// The item waits on a dependency or an open blocker, such as the session's own question.
    Park,
    /// Prompt again: continue, and submit, ask or release when done or stuck.
    Continue,
    /// The nudges are used up without a new commit.
    GiveUp,
}

impl State {
    /// What is written when `session` ends. `submitted` is the head of the session's branch when
    /// the branch has commits of its own; only a submission records it. `None` for a session that
    /// does not exist or a device that is not bound.
    pub fn closing(
        &self,
        session: SessionId,
        end: &SessionEnd,
        submitted: Option<CommitId>,
    ) -> Option<Closing> {
        let session = self.session(session)?;
        let closing = |reason, actor, note| Closing {
            reason,
            actor,
            note,
        };
        Some(match end {
            SessionEnd::Finish(finish) => {
                let by_agent = Actor::Agent {
                    agent: session.agent().clone(),
                    session: session.id(),
                };
                match finish {
                    Finish::Submit { note } => closing(
                        EndReason::Submitted { commit: submitted },
                        by_agent,
                        note.clone(),
                    ),
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

    /// Where `item` goes when its reviewed work cannot be merged: back to its open state for a
    /// conflict, which the next session resolves, and otherwise to its review state, where the
    /// dispatcher tries again. `None` when its type has no such state.
    pub fn after_failed_merge(&self, item: ItemId, failure: MergeFailure) -> Option<StateName> {
        match failure {
            MergeFailure::Conflict => self.first_state_of(item, StateCategory::Open),
            MergeFailure::Transient => self.first_state_of(item, StateCategory::Review),
        }
    }

    /// The state the daemon moves the item of a work session to when the session starts: the
    /// type's active state, if it has one.
    pub fn start_move(&self, session: SessionId) -> Option<(ItemId, StateName)> {
        let session = self.session(session)?;
        if session.role() != SessionRole::Work {
            return None;
        }
        let active = self.first_state_of(session.item(), StateCategory::Active)?;
        Some((session.item(), active))
    }

    /// What happens to the item of `session` when it ends for `reason`. The daemon does it before
    /// it writes the end, while the session still holds the item, so the item is never ready in
    /// between. `None` for a session that does not exist.
    pub fn after_end(&self, session: SessionId, reason: EndReason) -> Option<AfterEnd> {
        let session = self.session(session)?;
        if session.role() != SessionRole::Work {
            return Some(AfterEnd::Nothing);
        }
        let item = session.item();
        let first = |category| self.first_state_of(item, category);
        let move_to = |state: Option<StateName>| state.map_or(AfterEnd::Nothing, AfterEnd::MoveTo);
        Some(match reason {
            EndReason::Submitted { commit } => match (self.needs_review(item), commit) {
                (true, _) => move_to(first(StateCategory::Review)),
                (false, Some(_)) => match first(StateCategory::Done) {
                    Some(done) => AfterEnd::MergeThen { done },
                    None => AfterEnd::Nothing,
                },
                (false, None) => move_to(first(StateCategory::Done)),
            },
            EndReason::Reviewed | EndReason::Supervised | EndReason::Diagnosed => AfterEnd::Nothing,
            EndReason::Released
            | EndReason::TimedOut
            | EndReason::Failed
            | EndReason::Cancelled
            | EndReason::Interrupted => {
                let held_by_another = self
                    .claim(item)
                    .is_some_and(|holder| holder.id() != session.id());
                // Only an item that is being worked on goes back: one that was moved on, to done
                // or to review, stays where it is.
                if held_by_another || self.category_of(item) != Some(StateCategory::Active) {
                    AfterEnd::Nothing
                } else {
                    move_to(first(StateCategory::Open))
                }
            }
        })
    }

    /// What to do after a turn. `nudges_used` counts the turns in a row that ended without a new
    /// commit on the branch, this one included.
    pub fn turn_outcome(
        &self,
        session: SessionId,
        nudges_used: u32,
        nudges_allowed: u32,
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
        let moved_on = match self.category_of(item) {
            Some(StateCategory::Open | StateCategory::Active) => false,
            Some(
                StateCategory::Review
                | StateCategory::Done
                | StateCategory::Cancelled
                | StateCategory::Inactive,
            )
            | None => true,
        };
        if working && moved_on {
            return TurnOutcome::Close;
        }
        let blocked_by_comment = match self.item(item) {
            Some(ItemView::Live(live)) => live.has_open_blocker(),
            Some(ItemView::Archived(_)) | None => false,
        };
        let waits_on_dependency = !self
            .unfinished_dependencies(&self.dependency_graph(), item)
            .is_empty();
        if working && (blocked_by_comment || waits_on_dependency) {
            return TurnOutcome::Park;
        }
        if nudges_used > nudges_allowed {
            TurnOutcome::GiveUp
        } else {
            TurnOutcome::Continue
        }
    }

    fn category_of(&self, item: ItemId) -> Option<StateCategory> {
        let policy = &self.policy()?.policy;
        match self.item(item)? {
            ItemView::Live(item) => item.standing(policy).state_category(),
            ItemView::Archived(_) => None,
        }
    }

    pub(crate) fn first_state_of(
        &self,
        item: ItemId,
        category: StateCategory,
    ) -> Option<StateName> {
        let policy = &self.policy()?.policy;
        let ItemView::Live(item) = self.item(item)? else {
            return None;
        };
        policy
            .type_config(item.item_type())?
            .first_state(category)
            .cloned()
    }

    fn needs_review(&self, item: ItemId) -> bool {
        let Some(policy) = self.policy().map(|document| &document.policy) else {
            return false;
        };
        match self.item(item) {
            Some(ItemView::Live(item)) => item.needs_review(policy),
            Some(ItemView::Archived(_)) | None => false,
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

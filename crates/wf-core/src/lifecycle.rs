//! What follows from a session starting, ending and finishing a turn. Pure functions of the
//! state: the daemon asks and then writes the events.

use crate::hlc::Hlc;
use crate::id::{ItemId, SessionId};
use crate::item::{Item, ItemView};
use crate::name::StateName;
use crate::policy::{RecoveryStep, StateCategory};
use crate::session::{EndReason, SessionRole};
use crate::state::State;

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
    /// The session already ended, or someone moved its item on, such as to review.
    Close,
    /// The item waits on a dependency or an open blocker, such as the session's own question.
    Park,
    /// Prompt again: continue, and submit, ask or release when done or stuck.
    Continue,
    /// The nudges are used up without a new commit.
    GiveUp,
}

/// Where an item stands after the sessions of one role that ended without finishing (released,
/// timed out or failed) since it last moved on, as its recovery level says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recovery {
    /// No session of the role has ended without finishing since the item last moved on.
    Fresh,
    /// The level says to try again after `failures` sessions that did not finish.
    Retry { failures: usize },
    /// The principal decides, because the level says so or its steps are used up. `last` is the
    /// session that ended last.
    WaitsForPrincipal { failures: usize, last: SessionId },
}

impl Recovery {
    pub fn waits_for_principal(self) -> bool {
        match self {
            Recovery::WaitsForPrincipal { .. } => true,
            Recovery::Fresh | Recovery::Retry { .. } => false,
        }
    }
}

impl State {
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
            EndReason::Reviewed | EndReason::Supervised => AfterEnd::Nothing,
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
        // Only work waits: a review or supervise session acts on an item that waits for it.
        let working = match folded.role() {
            SessionRole::Work => true,
            SessionRole::Review { .. } | SessionRole::Supervise => false,
        };
        if working && self.category_of(item) == Some(StateCategory::Review) {
            return TurnOutcome::Close;
        }
        let blocked_by_comment = match self.item(item) {
            Some(ItemView::Live(live)) => live.has_open_blocker(),
            Some(ItemView::Archived(_)) | None => false,
        };
        let waits_on_dependency = !self.blockers(&self.dependency_graph(), item).is_empty();
        if working && (blocked_by_comment || waits_on_dependency) {
            return TurnOutcome::Park;
        }
        if nudges_used > nudges_allowed {
            TurnOutcome::GiveUp
        } else {
            TurnOutcome::Continue
        }
    }

    /// Where `item` stands after the sessions of `role` that ended without finishing. Only
    /// what happened since the item last moved on counts: since the principal last did something
    /// to it, and for work since the last submission, for review since the submission and its
    /// latest verdict, for supervision since its open blockers were written. The commit of a
    /// review role does not matter.
    pub fn recovery(&self, item: ItemId, role: SessionRole) -> Recovery {
        let (Some(policy), Some(ItemView::Live(live))) = (
            self.policy().map(|document| &document.policy),
            self.item(item),
        ) else {
            return Recovery::Fresh;
        };
        let since = self.moved_on(live, role);
        let same_role = |other: SessionRole| match (role, other) {
            (SessionRole::Work, SessionRole::Work)
            | (SessionRole::Review { .. }, SessionRole::Review { .. })
            | (SessionRole::Supervise, SessionRole::Supervise) => true,
            (SessionRole::Work, _)
            | (SessionRole::Review { .. }, _)
            | (SessionRole::Supervise, _) => false,
        };
        let mut failed: Vec<(Hlc, SessionId)> = self
            .sessions()
            .filter(|session| session.item() == item && same_role(session.role()))
            .filter_map(|session| {
                let ended = session.ended()?;
                let unfinished = match ended.reason {
                    EndReason::Released | EndReason::TimedOut | EndReason::Failed => true,
                    EndReason::Submitted { .. }
                    | EndReason::Reviewed
                    | EndReason::Supervised
                    | EndReason::Cancelled
                    | EndReason::Interrupted => false,
                };
                (unfinished && since.is_none_or(|since| ended.at() > since))
                    .then(|| (ended.at(), session.id()))
            })
            .collect();
        failed.sort();
        let Some(&(_, last)) = failed.last() else {
            return Recovery::Fresh;
        };
        let failures = failed.len();
        let steps = live
            .effective_recovery(policy)
            .and_then(|name| policy.recovery_level(name))
            .map(|level| level.steps.as_slice())
            .unwrap_or_default();
        // The step taken after the latest failure; once the principal decides, it stays so.
        let waits = steps
            .iter()
            .take(failures)
            .chain(std::iter::repeat(&RecoveryStep::Principal))
            .take(failures)
            .any(|step| match step {
                RecoveryStep::Principal => true,
                RecoveryStep::Retry => false,
            });
        if waits {
            Recovery::WaitsForPrincipal { failures, last }
        } else {
            Recovery::Retry { failures }
        }
    }

    /// Since when the sessions of `role` on `item` count for its recovery.
    fn moved_on(&self, item: &Item, role: SessionRole) -> Option<Hlc> {
        let submitted = self.submission(item.id()).map(|(_, stamp)| stamp.hlc);
        let by_role = match role {
            SessionRole::Work => submitted,
            SessionRole::Review { .. } => {
                let verdict = item
                    .verdicts()
                    .values()
                    .map(|verdict| verdict.stamp.hlc)
                    .max();
                submitted.max(verdict)
            }
            SessionRole::Supervise => item
                .open_comments()
                .into_iter()
                .filter(|(_, comment)| comment.kind == crate::event::CommentKind::Blocker)
                .map(|(_, comment)| comment.at)
                .min(),
        };
        item.principal_touched().max(by_role)
    }

    fn category_of(&self, item: ItemId) -> Option<StateCategory> {
        let policy = &self.policy()?.policy;
        match self.item(item)? {
            ItemView::Live(item) => item.standing(policy).state_category(),
            ItemView::Archived(_) => None,
        }
    }

    fn first_state_of(&self, item: ItemId, category: StateCategory) -> Option<StateName> {
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

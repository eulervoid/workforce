//! What follows from a session starting, ending and finishing a turn. Pure functions of the
//! state: the daemon asks and then writes the events.

use crate::id::{ItemId, SessionId};
use crate::item::ItemView;
use crate::name::StateName;
use crate::policy::StateCategory;
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
    /// The session already ended, or its item is blocked.
    Close,
    /// The item waits on a dependency or an open blocker, such as the session's own question.
    Park,
    /// Prompt again: continue, and submit, ask or release when done or stuck.
    Continue,
    /// The nudges are used up without a new commit.
    GiveUp,
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
                (true, _) => move_to(first(StateCategory::Blocked)),
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
            | EndReason::Cancelled => {
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
        if working && self.category_of(item) == Some(StateCategory::Blocked) {
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

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
    /// The item waits on a dependency, such as its own question.
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

    /// What happens to the item of `session`, which has ended. `None` for a session that is still
    /// live, or that does not exist.
    pub fn after_end(&self, session: SessionId) -> Option<AfterEnd> {
        let session = self.session(session)?;
        let reason = session.ended()?.reason;
        if session.role() != SessionRole::Work {
            return Some(AfterEnd::Nothing);
        }
        let item = session.item();
        let first = |category| self.first_state_of(item, category);
        let move_to = |state: Option<StateName>| state.map_or(AfterEnd::Nothing, AfterEnd::MoveTo);
        Some(match reason {
            EndReason::Submitted { commit: None } => move_to(first(StateCategory::Done)),
            EndReason::Submitted { commit: Some(_) } => match self.effective_review_of(item) {
                Some(_) => move_to(first(StateCategory::Blocked)),
                None => match first(StateCategory::Done) {
                    Some(done) => AfterEnd::MergeThen { done },
                    None => AfterEnd::Nothing,
                },
            },
            EndReason::Blocked | EndReason::Reviewed => AfterEnd::Nothing,
            EndReason::Released
            | EndReason::TimedOut
            | EndReason::Failed
            | EndReason::Cancelled => {
                if self.claim(item).is_some() {
                    // Another session works the item now, so it is not open again.
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
        // An item under review is blocked on purpose: it waits for this very session.
        let reviewing = matches!(folded.role(), SessionRole::Review { .. });
        if !reviewing && self.category_of(item) == Some(StateCategory::Blocked) {
            return TurnOutcome::Close;
        }
        if !self.blockers(&self.dependency_graph(), item).is_empty() {
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

    fn effective_review_of(&self, item: ItemId) -> Option<crate::name::ReviewName> {
        let policy = &self.policy()?.policy;
        let ItemView::Live(item) = self.item(item)? else {
            return None;
        };
        item.effective_review(policy).cloned()
    }
}

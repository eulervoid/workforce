//! What follows sessions that ended without finishing (released, timed out or failed): the steps
//! of the item's recovery level. Pure functions of the state; nothing about it is stored.

use std::collections::BTreeSet;

use crate::hlc::Hlc;
use crate::id::{ItemId, SessionId};
use crate::identity::{AgentId, DeviceKey};
use crate::item::{Item, ItemView};
use crate::name::Requirement;
use crate::policy::RecoveryStep;
use crate::session::{EndReason, Session, SessionRole};
use crate::state::State;

/// Where an item stands after the sessions of one role that ended without finishing since it
/// last moved on, as its recovery level says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Recovery {
    /// No session of the role has ended without finishing since the item last moved on.
    Fresh,
    /// The level says to try again after `failures` sessions that did not finish.
    Retry { failures: usize },
    /// An agent with these capabilities is to say what went wrong before the next attempt, in a
    /// diagnose session. Only work is diagnosed.
    Diagnose {
        failures: usize,
        requires: BTreeSet<Requirement>,
    },
    /// The principal decides, because the level says so or its steps are used up. `last` is the
    /// session that ended last.
    WaitsForPrincipal { failures: usize, last: SessionId },
}

impl Recovery {
    /// Whether another session of the role may start by itself.
    pub fn allows_attempt(&self) -> bool {
        match self {
            Recovery::Fresh | Recovery::Retry { .. } => true,
            Recovery::Diagnose { .. } | Recovery::WaitsForPrincipal { .. } => false,
        }
    }
}

/// What moves an item along its recovery level.
enum Step {
    /// A session of the role ended without finishing.
    Failed(SessionId),
    /// A diagnose session ended: the next step is taken without waiting for another failure.
    Diagnosed,
}

impl State {
    /// Where `item` stands after the sessions of `role` that ended without finishing. Only what
    /// happened since the item last moved on counts: since the principal last did something to
    /// it, and for work since the last submission, for review since the submission and its
    /// latest verdict, for supervision since its open blockers were written. Each failure takes
    /// the next step of the level, and so does each diagnosis; after the last step the principal
    /// decides. The commit of a review role does not matter.
    pub fn recovery(&self, item: ItemId, role: SessionRole) -> Recovery {
        let (Some(policy), Some(ItemView::Live(live))) = (
            self.policy().map(|document| &document.policy),
            self.item(item),
        ) else {
            return Recovery::Fresh;
        };
        let since = self.moved_on(live, role);
        let counts = |session: &Session| {
            session.item() == item
                && session
                    .ended()
                    .is_some_and(|ended| since.is_none_or(|since| ended.at() > since))
        };
        let mut steps_taken: Vec<(Hlc, Step)> = self
            .sessions()
            .filter(|session| counts(session))
            .filter_map(|session| {
                let reason = session.ended()?.reason;
                let at = session.ended()?.at();
                match (same_role(role, session.role()), session.role(), reason) {
                    (true, _, EndReason::Released | EndReason::TimedOut | EndReason::Failed) => {
                        Some((at, Step::Failed(session.id())))
                    }
                    (false, SessionRole::Diagnose, reason) if role == SessionRole::Work => {
                        match reason {
                            EndReason::Interrupted
                            | EndReason::Cancelled
                            | EndReason::MachineFault(_) => None,
                            EndReason::Diagnosed
                            | EndReason::Released
                            | EndReason::TimedOut
                            | EndReason::Failed
                            | EndReason::Submitted { .. }
                            | EndReason::Reviewed
                            | EndReason::Supervised => Some((at, Step::Diagnosed)),
                        }
                    }
                    _ => None,
                }
            })
            .collect();
        steps_taken.sort_by_key(|(at, _)| *at);

        let level = live
            .effective_recovery_level(policy)
            .and_then(|name| policy.recovery_level(name))
            .map(|level| level.steps.as_slice())
            .unwrap_or_default();
        let mut recovery = Recovery::Fresh;
        let mut failures = 0;
        let mut last = None;
        for (position, (_, step)) in steps_taken.into_iter().enumerate() {
            if let Step::Failed(session) = step {
                failures += 1;
                last = Some(session);
            }
            recovery = match (level.get(position), last) {
                (Some(RecoveryStep::Retry), _) => Recovery::Retry { failures },
                (Some(RecoveryStep::Agent { requires }), _) if role == SessionRole::Work => {
                    Recovery::Diagnose {
                        failures,
                        requires: requires.clone(),
                    }
                }
                // Only work is diagnosed; reviews and supervision try again instead.
                (Some(RecoveryStep::Agent { .. }), _) => Recovery::Retry { failures },
                (Some(RecoveryStep::Principal) | None, Some(last)) => {
                    Recovery::WaitsForPrincipal { failures, last }
                }
                (Some(RecoveryStep::Principal) | None, None) => Recovery::Fresh,
            };
        }
        recovery
    }

    /// The items whose work waits for a diagnosis that `agent` can give on `device`: the agent's
    /// capabilities and the device's hardware meet what the step requires, and no diagnose
    /// session holds the item. Oldest first.
    pub fn diagnosable(&self, device: DeviceKey, agent: &AgentId) -> Vec<ItemId> {
        let Some(definition) = self.agent(&agent.name, agent.principal) else {
            return Vec::new();
        };
        let offered = self.offered(device, definition);
        let mut found: Vec<&Item> = self
            .live_items()
            .filter(|item| {
                let Recovery::Diagnose { requires, .. } =
                    self.recovery(item.id(), SessionRole::Work)
                else {
                    return false;
                };
                let capable = requires.iter().all(|need| offered.contains(need));
                let held = self.live_sessions().any(|session| {
                    session.item() == item.id() && session.role() == SessionRole::Diagnose
                });
                capable && !held
            })
            .collect();
        found.sort_by_key(|item| (item.created(), item.id()));
        found.into_iter().map(Item::id).collect()
    }

    /// Since when the sessions of `role` on `item` count for its recovery.
    fn moved_on(&self, item: &Item, role: SessionRole) -> Option<Hlc> {
        let submitted = self.submission(item.id()).map(|(_, stamp)| stamp.hlc);
        let by_role = match role {
            SessionRole::Work | SessionRole::Diagnose => submitted,
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
}

/// Whether two roles are the same, whatever commit a review looks at.
fn same_role(left: SessionRole, right: SessionRole) -> bool {
    match (left, right) {
        (SessionRole::Work, SessionRole::Work)
        | (SessionRole::Review { .. }, SessionRole::Review { .. })
        | (SessionRole::Supervise, SessionRole::Supervise)
        | (SessionRole::Diagnose, SessionRole::Diagnose) => true,
        (SessionRole::Work, _)
        | (SessionRole::Review { .. }, _)
        | (SessionRole::Supervise, _)
        | (SessionRole::Diagnose, _) => false,
    }
}

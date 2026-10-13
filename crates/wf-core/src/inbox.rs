//! What waits on a person, worked out from the state.

use serde::{Deserialize, Serialize};

use crate::id::{CommentId, HashId, ItemId, SessionId};
use crate::item::{ItemView, Standing};
use crate::policy::{ReviewStep, StateCategory};
use crate::recovery::Recovery;
use crate::review::ReviewStatus;
use crate::session::{CommitId, SessionRole};
use crate::state::State;
use crate::supervision::WaitsOn;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Waiting {
    /// A blocker on the item waits on this person: resolving it is the answer.
    Blocker { comment: CommentId },
    /// The item waits in its review state without submitted work, such as a proposed decision.
    /// Moving it on decides.
    Proposal,
    /// The review of the work has reached a step for a person: `wf review`.
    PrincipalReview { commit: Option<CommitId> },
    /// The work is ready to merge, but the target branch is checked out with changes.
    Merge { commit: CommitId },
    /// Sessions of `role` on the item ended without finishing, and its recovery level leaves the
    /// next step to the principal: starting a session, commenting, editing or moving the item.
    Recovery {
        role: SessionRole,
        failures: usize,
        last: SessionId,
    },
}

impl Waiting {
    /// The kind of what waits, as `wf inbox --json` names it.
    pub fn kind(&self) -> &'static str {
        match self {
            Waiting::Blocker { .. } => "blocker",
            Waiting::Proposal => "proposal",
            Waiting::PrincipalReview { .. } => "principal_review",
            Waiting::Merge { .. } => "merge",
            Waiting::Recovery { .. } => "recovery",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxEntry {
    pub item: ItemId,
    pub waiting: Waiting,
}

/// The kind of a permission request that an agent's session holds, which the daemon keeps.
pub const PERMISSION_KIND: &str = "permission";

/// What a permission request held for the principal says: who asks to do what, and how to answer.
pub fn permission_waits(agent: Option<&str>, what: &str, session: SessionId) -> String {
    format!(
        "{} asks to {what}; `wf session allow` or `deny {}`",
        agent.unwrap_or("an agent"),
        session.short()
    )
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
            .filter(|item| item.standing(policy).state_category() == Some(StateCategory::Review))
            .filter_map(|item| {
                let waiting = match self.review_status(item.id()) {
                    Some(ReviewStatus::Awaiting {
                        step: ReviewStep::Principal,
                    }) => {
                        let (commit, _) = self.submission(item.id())?;
                        Waiting::PrincipalReview { commit }
                    }
                    Some(ReviewStatus::Awaiting {
                        step: ReviewStep::Agent { .. },
                    }) => return None,
                    Some(ReviewStatus::Approved { .. })
                    | Some(ReviewStatus::ChangesRequested)
                    | Some(ReviewStatus::Merged { .. })
                    | None => match merges.iter().find(|pending| pending.item == item.id()) {
                        Some(pending) => Waiting::Merge {
                            commit: pending.commit,
                        },
                        None => Waiting::Proposal,
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
        for item in self.live_items() {
            let category = item.standing(policy).state_category();
            let roles = match category {
                Some(StateCategory::Open | StateCategory::Active) => {
                    vec![SessionRole::Work, SessionRole::Supervise]
                }
                Some(StateCategory::Review) => {
                    vec![SessionRole::Review { commit: None }, SessionRole::Supervise]
                }
                Some(StateCategory::Inactive | StateCategory::Done | StateCategory::Cancelled)
                | None => Vec::new(),
            };
            for role in roles {
                if let Recovery::WaitsForPrincipal { failures, last } =
                    self.recovery(item.id(), role)
                {
                    entries.push((
                        item.updated(),
                        InboxEntry {
                            item: item.id(),
                            waiting: Waiting::Recovery {
                                role,
                                failures,
                                last,
                            },
                        },
                    ));
                }
            }
        }
        for blocker in self.open_blockers() {
            if let WaitsOn::Principal(_) = blocker.waits_on {
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

impl State {
    /// The title the inbox shows for an item.
    pub fn inbox_title(&self, item: ItemId) -> String {
        match self.item(item) {
            Some(ItemView::Live(live)) => live.title().to_string(),
            Some(ItemView::Archived(_)) | None => "(archived)".to_string(),
        }
    }

    /// What an entry of the inbox says to the principal.
    pub fn what_waits(&self, entry: &InboxEntry) -> String {
        let Some(policy) = self.policy().map(|document| &document.policy) else {
            return String::new();
        };
        match entry.waiting {
            Waiting::Blocker { comment } => format!(
                "blocker {}: answer with `wf comment {} \"...\" --resolve {}`",
                comment.short(),
                entry.item.short(),
                comment.short()
            ),
            Waiting::Proposal => {
                let state_name = match self.item(entry.item) {
                    Some(ItemView::Live(live)) => match live.standing(policy) {
                        Standing::Known { state, .. } => state.to_string(),
                        Standing::Unknown { state } => state
                            .map_or_else(|| "an unknown state".to_string(), ToString::to_string),
                    },
                    Some(ItemView::Archived(_)) | None => "an unknown state".to_string(),
                };
                format!("{state_name}; `wf item move` it on")
            }
            Waiting::PrincipalReview { commit } => match commit {
                Some(commit) => format!(
                    "review of {}: `wf item diff`, then `wf review`",
                    &commit.to_string()[..8]
                ),
                None => "review of work without commits: read it, then `wf review`".to_string(),
            },
            Waiting::Merge { commit } => format!(
                "{} is ready to merge, but {} is checked out with changes",
                &commit.to_string()[..8],
                policy.merge.target
            ),
            Waiting::Recovery {
                role,
                failures,
                last,
            } => self.recovery_waits(entry.item, role, failures, last),
        }
    }

    /// What a recovery step that waits on the principal says: who ended last and how, and what
    /// to do.
    pub fn recovery_waits(
        &self,
        item: ItemId,
        role: SessionRole,
        failures: usize,
        last: SessionId,
    ) -> String {
        let sessions = match (role, failures) {
            (SessionRole::Work, 1) => "a session".to_string(),
            (SessionRole::Work, _) => format!("{failures} sessions"),
            (SessionRole::Review { .. }, 1) => "a review session".to_string(),
            (SessionRole::Review { .. }, _) => format!("{failures} review sessions"),
            (SessionRole::Supervise, 1) => "a supervise session".to_string(),
            (SessionRole::Supervise, _) => format!("{failures} supervise sessions"),
            (SessionRole::Diagnose, 1) => "a diagnose session".to_string(),
            (SessionRole::Diagnose, _) => format!("{failures} diagnose sessions"),
        };
        let ended = self
            .session(last)
            .and_then(|session| {
                let reason = session.ended()?.reason.name().replace('_', " ");
                Some(format!("@{} {reason}", session.agent().name))
            })
            .unwrap_or_else(|| "the last one ended".to_string());
        let next = match role {
            SessionRole::Work | SessionRole::Diagnose => format!(
                "comment with what to do differently, or `wf session start {}`",
                item.short()
            ),
            SessionRole::Review { .. } => {
                "review it yourself, or comment to have it tried again".into()
            }
            SessionRole::Supervise => {
                "answer the blocker, or comment to have it tried again".into()
            }
        };
        format!("{sessions} did not finish (last: {ended}); {next}")
    }
}

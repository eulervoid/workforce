//! What waits on the principal, worked out from the state.

use serde::{Deserialize, Serialize};

use crate::id::{CommentId, HashId, ItemId, SessionId};
use crate::item::{ItemEntry, ItemState};
use crate::policy::ReviewStep;
use crate::recovery::Recovery;
use crate::review::ReviewStatus;
use crate::session::{CommitId, SessionRole};
use crate::state::State;
use crate::supervision::WaitsOn;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Waiting {
    /// A blocker on the item waits on the principal: resolving it is the answer.
    Blocker { comment: CommentId },
    /// The item waits in its review state without submitted work, such as a proposed decision.
    /// Moving it on decides.
    Proposal,
    /// The review of the work has reached a step for the principal: `wf review`.
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

/// The kind of an entry of the inbox: what waits in the state, or what the daemon keeps: a
/// permission request that an agent's session holds, or what is wrong with its machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboxKind {
    Blocker,
    Proposal,
    PrincipalReview,
    Merge,
    Recovery,
    Permission,
    MachineFault,
}

impl InboxKind {
    /// The kind as `wf inbox --json` names it.
    pub fn as_str(self) -> &'static str {
        match self {
            InboxKind::Blocker => "blocker",
            InboxKind::Proposal => "proposal",
            InboxKind::PrincipalReview => "principal_review",
            InboxKind::Merge => "merge",
            InboxKind::Recovery => "recovery",
            InboxKind::Permission => "permission",
            InboxKind::MachineFault => "machine_fault",
        }
    }
}

impl std::fmt::Display for InboxKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Waiting {
    pub fn kind(&self) -> InboxKind {
        match self {
            Waiting::Blocker { .. } => InboxKind::Blocker,
            Waiting::Proposal => InboxKind::Proposal,
            Waiting::PrincipalReview { .. } => InboxKind::PrincipalReview,
            Waiting::Merge { .. } => InboxKind::Merge,
            Waiting::Recovery { .. } => InboxKind::Recovery,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxEntry {
    pub item: ItemId,
    pub waiting: Waiting,
}

/// What a permission request held for the principal says: who asks to do what, and how to answer.
pub fn permission_waits(agent: Option<&str>, what: &str, session: SessionId) -> String {
    format!(
        "{} asks to {what}; `wf session allow` or `deny {}`",
        agent.unwrap_or("an agent"),
        session.short()
    )
}

impl State {
    /// What waits on the principal, the item changed last first. Permission requests are held by the
    /// daemon and are not part of the state.
    pub fn inbox(&self) -> Vec<InboxEntry> {
        let Some(policy) = self.policy().map(|document| &document.policy) else {
            return Vec::new();
        };
        let merges = self.pending_merges();
        let finished = self.finished_reviews();
        let mut entries: Vec<_> = self
            .live_items()
            .filter(|item| item.state(policy) == ItemState::Review)
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
                    // The merge task moves it to `done`.
                    Some(ReviewStatus::Merged { .. })
                        if merges.iter().any(|pending| pending.item == item.id()) =>
                    {
                        return None;
                    }
                    Some(ReviewStatus::Approved { .. })
                    | Some(ReviewStatus::ChangesRequested)
                    | Some(ReviewStatus::Merged { .. })
                    | None => {
                        if let Some(pending) =
                            merges.iter().find(|pending| pending.item == item.id())
                        {
                            Waiting::Merge {
                                commit: pending.commit,
                            }
                        } else if finished.contains(&item.id()) {
                            // The daemon moves it to `done`.
                            return None;
                        } else if item.has_open_blocker() {
                            // The blocker is what waits, such as a check that fails on the
                            // target too; it is listed below.
                            return None;
                        } else {
                            Waiting::Proposal
                        }
                    }
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
            let roles = match item.state(policy) {
                ItemState::Open | ItemState::Active => {
                    vec![SessionRole::Work, SessionRole::Supervise]
                }
                ItemState::Review => {
                    vec![SessionRole::Review { commit: None }, SessionRole::Supervise]
                }
                ItemState::Draft | ItemState::Done | ItemState::Cancelled => Vec::new(),
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
                let Some(crate::item::ItemEntry::Live(item)) = self.item(blocker.item) else {
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
            Some(ItemEntry::Live(live)) => live.title().to_string(),
            Some(ItemEntry::Archived(_)) | None => "(archived)".to_string(),
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
            Waiting::Proposal => format!(
                "proposed: accept it with `wf item done {}`, reject it with `wf item cancel {}`",
                entry.item.short(),
                entry.item.short()
            ),
            Waiting::PrincipalReview { commit } => match commit {
                Some(commit) => format!(
                    "review of {}: `wf item diff`, then `wf review`",
                    commit.short()
                ),
                None => "review of work without commits: read it, then `wf review`".to_string(),
            },
            Waiting::Merge { commit } => format!(
                "{} is ready to merge, but {} is checked out with changes; the daemon's comment on the item names them",
                commit.short(),
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
                let reason = session.ended()?.reason.label();
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

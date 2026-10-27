//! What waits on the principal, worked out from the state.

use serde::{Deserialize, Serialize};

use crate::brief::speaker;
use crate::event::CommentKind;
use crate::id::{CommentId, HashId, ItemId, SessionId};
use crate::identity::Actor;
use crate::item::{Item, ItemEntry, ItemState};
use crate::policy::ReviewStep;
use crate::recovery::Recovery;
use crate::refs::Token;
use crate::resolve::MentionTarget;
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
    /// The review of the work waits for the principal: `wf review`. `stale` says when the branch
    /// stopped merging into the target after the review was offered.
    PrincipalReview {
        commit: Option<CommitId>,
        stale: Option<Stale>,
        reason: PrincipalReviewReason,
    },
    /// Sessions of `role` on the item ended without finishing, and its recovery level leaves the
    /// next step to the principal: starting a session, commenting, editing or moving the item.
    Recovery {
        role: SessionRole,
        failures: usize,
        last: SessionId,
    },
    /// A comment on the item mentions a principal, who has not acted on the item since: acting on
    /// it (commenting, editing, moving it) takes it out of the inbox.
    Mention { comment: CommentId },
}

/// Why a review waits for the principal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PrincipalReviewReason {
    /// The review level has reached a step for the principal, or an agent escalated.
    Step,
    /// An agent's review requested changes after `rounds` rounds of them, as many as the review
    /// level's `max_rounds` allows: the open change requests did not send the work back.
    RoundLimit { rounds: u32 },
}

/// The daemon found that the work no longer merges into the target branch, after it offered the
/// review: its open change request on the item says in which files.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stale {
    pub comment: CommentId,
    pub since_ms: u64,
}

/// The kind of an entry of the inbox: what waits in the state, on an item or as a push of the
/// principal that lost a fork of the target; or what the daemon keeps: a permission request that
/// an agent's session holds, or what is wrong with its machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboxKind {
    Blocker,
    Proposal,
    PrincipalReview,
    Recovery,
    Mention,
    Permission,
    MachineFault,
    LostPush,
}

impl InboxKind {
    /// The kind as `wf inbox --json` names it.
    pub fn as_str(self) -> &'static str {
        match self {
            InboxKind::Blocker => "blocker",
            InboxKind::Proposal => "proposal",
            InboxKind::PrincipalReview => "principal_review",
            InboxKind::Recovery => "recovery",
            InboxKind::Mention => "mention",
            InboxKind::Permission => "permission",
            InboxKind::MachineFault => "machine_fault",
            InboxKind::LostPush => "lost_push",
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
            Waiting::Recovery { .. } => InboxKind::Recovery,
            Waiting::Mention { .. } => InboxKind::Mention,
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
        let merges = self.pending_merges();
        let finished = self.finished_reviews();
        let mut entries: Vec<_> = self
            .live_items()
            .filter(|item| item.state() == ItemState::Review)
            .filter_map(|item| {
                let waiting = match self.review_status(item.id()) {
                    // Agent steps that are open come first: the principal is waited for only
                    // when no agent is.
                    Some(ReviewStatus::Awaiting { steps, .. })
                        if steps
                            .iter()
                            .all(|step| matches!(step, ReviewStep::Principal)) =>
                    {
                        let (commit, submitted) = self.submission(item.id())?;
                        Waiting::PrincipalReview {
                            commit,
                            stale: self.stale_since(item.id(), submitted.hlc),
                            reason: PrincipalReviewReason::Step,
                        }
                    }
                    Some(ReviewStatus::RoundLimitReached { rounds }) => {
                        let (commit, submitted) = self.submission(item.id())?;
                        Waiting::PrincipalReview {
                            commit,
                            stale: self.stale_since(item.id(), submitted.hlc),
                            reason: PrincipalReviewReason::RoundLimit { rounds },
                        }
                    }
                    Some(ReviewStatus::Awaiting { .. }) => return None,
                    Some(ReviewStatus::Approved { .. })
                    | Some(ReviewStatus::ChangesRequested)
                    | Some(ReviewStatus::Merged { .. })
                    | None => {
                        if merges.iter().any(|pending| pending.item == item.id()) {
                            // The merge task merges it and moves it to `done`.
                            return None;
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
            let roles = match item.state() {
                ItemState::Open | ItemState::Active => {
                    vec![SessionRole::Work, SessionRole::Supervise]
                }
                ItemState::Review => {
                    vec![
                        SessionRole::Review {
                            commit: None,
                            step: None,
                        },
                        SessionRole::Supervise,
                    ]
                }
                ItemState::Draft | ItemState::Done | ItemState::Cancelled => Vec::new(),
            };
            for role in roles {
                if let Recovery::WaitsForPrincipal { failures, last } =
                    self.recovery(item.id(), &role)
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
        for item in self.live_items() {
            for comment in self.unanswered_mentions(item) {
                entries.push((
                    item.updated(),
                    InboxEntry {
                        item: item.id(),
                        waiting: Waiting::Mention { comment },
                    },
                ));
            }
        }
        entries.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.item.cmp(&right.1.item)));
        entries.into_iter().map(|(_, entry)| entry).collect()
    }
}

impl State {
    /// The comments on `item`, oldest first, that mention a principal other than their writer and
    /// that no principal acted on the item after. Writing a comment touches the item at the
    /// comment's own stamp, so a principal's comment that mentions another waits as well. A
    /// blocker waits in the inbox as a blocker.
    fn unanswered_mentions(&self, item: &Item) -> Vec<CommentId> {
        let resolver = self.resolver();
        let mut mentions: Vec<_> = item
            .comments()
            .iter()
            .filter(|(_, comment)| comment.kind != CommentKind::Blocker)
            .filter(|(_, comment)| {
                item.principal_touched()
                    .is_none_or(|touched| touched <= comment.at)
            })
            .filter(|(_, comment)| {
                comment.tokens.iter().any(|token| match token {
                    Token::Mention(mention) => match resolver.mention(mention) {
                        Some(MentionTarget::Principal(principal)) => {
                            comment.by != Actor::Principal(principal)
                        }
                        Some(MentionTarget::Agent(_) | MentionTarget::Session(_)) | None => false,
                    },
                    Token::ItemRef(_) | Token::CommentRef { .. } => false,
                })
            })
            .map(|(id, comment)| (comment.at, *id))
            .collect();
        mentions.sort();
        mentions.into_iter().map(|(_, id)| id).collect()
    }
}

/// A number of review rounds, as the inbox and `wf item show` say it.
pub fn round_count(rounds: u32) -> String {
    match rounds {
        1 => "1 round".to_string(),
        _ => format!("{rounds} rounds"),
    }
}

/// A point in time, given in milliseconds since the epoch, as the inbox and the dashboard show it.
pub fn timestamp(at_ms: u64) -> String {
    jiff::Timestamp::from_millisecond(i64::try_from(at_ms).unwrap_or(i64::MAX))
        .map_or_else(|_| at_ms.to_string(), |at| at.to_string())
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
        match &entry.waiting {
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
            Waiting::PrincipalReview {
                commit,
                stale,
                reason,
            } => {
                let review = match (reason, commit) {
                    (PrincipalReviewReason::Step, Some(commit)) => format!(
                        "review of {}: `wf item diff`, then `wf review`",
                        commit.short()
                    ),
                    (PrincipalReviewReason::Step, None) => {
                        "review of work without commits: read it, then `wf review`".to_string()
                    }
                    (PrincipalReviewReason::RoundLimit { rounds }, _) => format!(
                        "changes requested again after {}, the round limit: approve with `wf review --approve`, send it back with `wf review --request-changes`, or edit or cancel it",
                        round_count(*rounds)
                    ),
                };
                match stale {
                    Some(stale) => format!(
                        "{review}. Stale since {}: it no longer merges into {}, see the daemon's change request {}",
                        timestamp(stale.since_ms),
                        policy.merge.target,
                        stale.comment.short()
                    ),
                    None => review,
                }
            }
            Waiting::Recovery {
                role,
                failures,
                last,
            } => self.recovery_waits(entry.item, role, *failures, *last),
            Waiting::Mention { comment } => {
                let by = self
                    .item(entry.item)
                    .and_then(|item| match item {
                        ItemEntry::Live(item) => item.comments().get(comment),
                        ItemEntry::Archived(_) => None,
                    })
                    .map_or_else(
                        || "someone".to_string(),
                        |written| speaker(self, &written.by),
                    );
                format!(
                    "{by} mentions you in {}: answer with `wf comment {} \"...\"`",
                    comment.short(),
                    entry.item.short()
                )
            }
        }
    }

    /// What a recovery step that waits on the principal says: who ended last and how, and what
    /// to do.
    pub fn recovery_waits(
        &self,
        item: ItemId,
        role: &SessionRole,
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

//! Offered reviews: work is checked against the target branch before a reviewer spends a review
//! on it, because another item may have merged since the work was submitted.

use std::collections::BTreeSet;
use std::path::Path;

use wf_core::event::{CommentKind, Commented, EventBody};
use wf_core::id::{CommentId, HashId, ItemId};
use wf_core::identity::Actor;
use wf_core::policy::ReviewStep;
use wf_core::session::CommitId;
use wf_vcs::{GitCli, MergeTree};

use super::{SessionError, blocking};
use crate::{Daemon, Loaded};

/// What the daemon remembers of checking offered reviews against the target. It lives in memory:
/// a restarted daemon cannot tell which principal reviews it already offered, so its first pass
/// keeps the ones that conflict in the inbox as stale instead of sending them back to work.
/// Only principal steps are remembered: an agent step never waits in the inbox.
pub(super) struct CheckedReviews {
    seen: BTreeSet<(ItemId, CommitId)>,
    first_pass: bool,
}

impl CheckedReviews {
    pub(super) fn new() -> Self {
        Self {
            seen: BTreeSet::new(),
            first_pass: true,
        }
    }
}

impl Daemon {
    /// Checks that the work of every offered review merges into the target. What does not merge
    /// the first time it is offered goes back to work with a change request that names the files.
    /// What merged then and stopped merging while a principal waits to review it stays in the
    /// inbox, with the change request to say so since when; an agent step has no one waiting yet,
    /// so that goes back to work as well. Once such work merges again, the daemon resolves that
    /// change request. After a restart, the first pass counts every principal step as offered.
    pub(super) async fn check_offered_reviews(
        &self,
        loaded: &Loaded,
        repository: &Path,
        checked: &mut CheckedReviews,
    ) -> Result<(), SessionError> {
        let state = self.snapshot(loaded).await?;
        let Some(target) = state
            .policy()
            .map(|document| document.policy.merge.target.clone())
        else {
            return Ok(());
        };
        let principal = self.principal_of_device(&state)?;
        let offered = state.offered_reviews();
        let after_restart = std::mem::take(&mut checked.first_pass);
        checked.seen.retain(|(item, commit)| {
            offered
                .iter()
                .any(|(offered, offered_commit, _)| offered == item && offered_commit == commit)
        });
        for (item, commit, step) in offered {
            let merges = {
                let (repository, target) = (repository.to_path_buf(), target.clone());
                blocking(move || GitCli::open(&repository)?.merge_tree(&target, commit)).await?
            };
            let paths = match merges {
                Ok(MergeTree::Landed { .. } | MergeTree::Clean { .. }) => {
                    if matches!(step, ReviewStep::Principal) && checked.seen.insert((item, commit))
                    {
                        tracing::info!("#{}: merges into {target}", item.short());
                        self.reviews_found_merging
                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                    if let Some((_, submitted)) = state.submission(item)
                        && let Some(stale) = state.stale_since(item, submitted.hlc)
                    {
                        self.author(
                            loaded,
                            Actor::Daemon(principal),
                            EventBody::Commented(Commented {
                                resolves: vec![stale.comment],
                                ..Commented::plain(item, format!("Merges into {target} again."))
                            }),
                        )
                        .await?;
                    }
                    continue;
                }
                Ok(MergeTree::Conflict { paths }) => paths,
                Err(error) => {
                    tracing::warn!(
                        "could not check whether #{} merges into {target}: {}",
                        item.short(),
                        wf_core::error_chain(&error)
                    );
                    continue;
                }
            };
            let first_offer =
                matches!(step, ReviewStep::Principal) && checked.seen.insert((item, commit));
            let stale = match step {
                ReviewStep::Principal if after_restart || !first_offer => {
                    let Some((_, submitted)) = state.submission(item) else {
                        continue;
                    };
                    if state.stale_since(item, submitted.hlc).is_some() {
                        continue;
                    }
                    true
                }
                ReviewStep::Principal | ReviewStep::Agent { .. } => false,
            };
            let text = format!(
                "Merging into {target} conflicts in {}. Rebase the branch onto {target} and resolve the conflicts; its submission rebases it.",
                paths.join(", ")
            );
            let written = self
                .author(
                    loaded,
                    Actor::Daemon(principal),
                    EventBody::Commented(Commented {
                        kind: CommentKind::ChangeRequest,
                        ..Commented::plain(item, text)
                    }),
                )
                .await?;
            tracing::info!("#{}: does not merge into {target}", item.short());
            if stale {
                continue;
            }
            if let Some(open) =
                state.after_failed_merge(item, wf_core::lifecycle::MergeFailure::Conflict)
            {
                let because = Some(CommentId::from(written.id()));
                self.move_item(loaded, principal, item, open, because, None)
                    .await?;
            }
        }
        Ok(())
    }
}

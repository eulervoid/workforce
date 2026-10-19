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

/// The reviews the daemon has checked against the target: by item and commit.
pub(super) type CheckedReviews = BTreeSet<(ItemId, CommitId)>;

impl Daemon {
    /// Checks that the work of every offered review merges into the target. What does not merge
    /// the first time it is offered goes back to work with a change request that names the files.
    /// What merged then and stopped merging while a principal waits to review it stays in the
    /// inbox, with the change request to say so since when; an agent step has no one waiting yet,
    /// so that goes back to work as well.
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
        checked.retain(|(item, commit)| {
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
                    checked.insert((item, commit));
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
            let first_offer = checked.insert((item, commit));
            let stale = match step {
                ReviewStep::Principal if !first_offer => {
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

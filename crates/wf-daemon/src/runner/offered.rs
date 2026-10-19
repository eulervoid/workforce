//! Offered reviews: work is checked against the target branch before a reviewer spends a review
//! on it, because another item may have merged since the work was submitted.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use wf_core::event::{CommentKind, Commented, EventBody};
use wf_core::id::{CommentId, HashId, ItemId};
use wf_core::identity::Actor;
use wf_core::inbox::Stale;
use wf_core::item::ItemState;
use wf_core::lifecycle::MergeFailure;
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
    /// What merging an offered commit into the target made, and the head of the target it was
    /// made against. It only changes when the commit or the head does, so `merge-tree` runs once
    /// for each pair.
    merges: BTreeMap<(ItemId, CommitId), (CommitId, MergeTree)>,
    first_pass: bool,
}

impl CheckedReviews {
    pub(super) fn new() -> Self {
        Self {
            seen: BTreeSet::new(),
            merges: BTreeMap::new(),
            first_pass: true,
        }
    }
}

/// An offered review with what the check needs to know of its item.
struct Offer {
    item: ItemId,
    commit: CommitId,
    step: ReviewStep,
    /// The open change request of the daemon that says the work stopped merging.
    stale: Option<Stale>,
    /// Where the item goes back to when its work does not merge.
    after_conflict: Option<ItemState>,
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
        let device = self.device();
        let Some((target, principal, offers)) = self
            .read(loaded, move |state| {
                let target = state.policy()?.policy.merge.target.clone();
                let principal = state.principal_of(&device)?;
                let offers: Vec<Offer> = state
                    .offered_reviews()
                    .into_iter()
                    .map(|(item, commit, step)| Offer {
                        item,
                        commit,
                        step,
                        stale: state
                            .submission(item)
                            .and_then(|(_, submitted)| state.stale_since(item, submitted.hlc)),
                        after_conflict: state.after_failed_merge(item, MergeFailure::Conflict),
                    })
                    .collect();
                Some((target, principal, offers))
            })
            .await?
        else {
            return Ok(());
        };
        let head = {
            let (repository, target) = (repository.to_path_buf(), target.clone());
            blocking(move || GitCli::open(&repository)?.branch_head(&target)).await?
        };
        let head = match head {
            Ok(head) => head,
            Err(error) => {
                tracing::warn!(
                    "could not check the offered reviews against {target}: {}",
                    wf_core::error_chain(&error)
                );
                return Ok(());
            }
        };
        let after_restart = std::mem::take(&mut checked.first_pass);
        let is_offered = |item: ItemId, commit: CommitId| {
            offers
                .iter()
                .any(|offer| offer.item == item && offer.commit == commit)
        };
        checked
            .seen
            .retain(|(item, commit)| is_offered(*item, *commit));
        checked
            .merges
            .retain(|(item, commit), _| is_offered(*item, *commit));
        for Offer {
            item,
            commit,
            step,
            stale,
            after_conflict,
        } in offers
        {
            let merges = match checked.merges.get(&(item, commit)) {
                Some((checked_head, merges)) if *checked_head == head => Ok(merges.clone()),
                _ => {
                    let (repository, target) = (repository.to_path_buf(), target.clone());
                    let merges =
                        blocking(move || GitCli::open(&repository)?.merge_tree(&target, commit))
                            .await?;
                    if let Ok(merges) = &merges {
                        checked
                            .merges
                            .insert((item, commit), (head, merges.clone()));
                    }
                    merges
                }
            };
            let paths = match merges {
                Ok(MergeTree::Landed { .. } | MergeTree::Clean { .. }) => {
                    if matches!(step, ReviewStep::Principal) && checked.seen.insert((item, commit))
                    {
                        tracing::info!("#{}: merges into {target}", item.short());
                        self.reviews_found_merging
                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                    if let Some(stale) = stale {
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
                    if stale.is_some() {
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
            if let Some(open) = after_conflict {
                let because = Some(CommentId::from(written.id()));
                self.move_item(loaded, principal, item, open, because, None)
                    .await?;
            }
        }
        Ok(())
    }
}

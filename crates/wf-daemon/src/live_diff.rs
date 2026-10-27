//! The live diff of an item: what its work session on some device has changed so far.
//!
//! The device of the session answers from its own agent repository over `wf/query/1`, and a device that
//! asks for it dials that one, so the answer is the diff as the session's device shows it, against
//! the target it has.

use std::path::PathBuf;

use wf_core::id::{ItemId, WorkforceId};
use wf_core::session::{CommitId, SessionRole};
use wf_net::{DiffAnswer, Work};
use wf_vcs::{GitCli, VcsError, item_repository};

use crate::proto::LiveDiff;
use crate::{Daemon, StorageClosed};

#[derive(Debug, thiserror::Error)]
pub enum LiveDiffError {
    #[error("workforce {0} is not loaded")]
    NotLoaded(WorkforceId),
    #[error("the workforce has no policy, so there is no target to compare against")]
    NoPolicy,
    #[error(transparent)]
    Storage(#[from] StorageClosed),
    #[error("could not read the work of the item")]
    Git(#[from] VcsError),
    #[error("reading the work of the item was cut short")]
    Task(#[source] tokio::task::JoinError),
}

impl Daemon {
    /// The work of `item` as this device has it: the head of its branch against the target, read
    /// the way `wf item diff` reads it. [`DiffAnswer::NotHere`] when there is no agent repository
    /// or branch of the item here, and [`DiffAnswer::Unchanged`] while the head is `since`.
    pub(crate) async fn diff_here(
        &self,
        workforce: WorkforceId,
        item: ItemId,
        since: Option<CommitId>,
    ) -> Result<DiffAnswer, LiveDiffError> {
        let loaded = self
            .loaded_workforce(&workforce)
            .ok_or(LiveDiffError::NotLoaded(workforce))?;
        let target = loaded
            .storage
            .with(|replica| {
                replica
                    .state()
                    .policy()
                    .map(|config| config.policy.merge.target.clone())
            })
            .await?
            .ok_or(LiveDiffError::NoPolicy)?;
        self.ensure_bare_repository(workforce, &loaded.storage)
            .await;
        let agent_repository = item_repository(&self.workforce_directory(workforce), item);
        let repository = self.bare_repository(workforce);
        tokio::task::spawn_blocking(move || {
            read_work(repository, agent_repository, item, &target, since)
        })
        .await
        .map_err(LiveDiffError::Task)?
    }

    /// The live diff of `item`, from the device of its live work session: here without the
    /// network when the session runs here.
    pub(crate) async fn live_diff(
        &self,
        workforce: WorkforceId,
        item: ItemId,
        since: Option<CommitId>,
    ) -> Result<LiveDiff, LiveDiffError> {
        let loaded = self
            .loaded_workforce(&workforce)
            .ok_or(LiveDiffError::NotLoaded(workforce))?;
        let session_device = loaded
            .storage
            .with(move |replica| {
                replica
                    .state()
                    .holder(item, &SessionRole::Work)
                    .map(|session| session.device())
            })
            .await?;
        let Some(device) = session_device else {
            return Ok(LiveDiff::NoSession);
        };
        if device == self.device() {
            return Ok(LiveDiff::Answer(
                self.diff_here(workforce, item, since).await?,
            ));
        }
        let asked = async {
            let node = self.node().await?;
            node.query_diff(wf_net::endpoint_id(device), workforce, item, since)
                .await
        };
        Ok(match asked.await {
            Ok(answer) => LiveDiff::Answer(answer),
            Err(error) => LiveDiff::Unreachable {
                device,
                reason: wf_core::error_chain(&error),
            },
        })
    }
}

fn read_work(
    repository: PathBuf,
    agent_repository: PathBuf,
    item: ItemId,
    target: &str,
    since: Option<CommitId>,
) -> Result<DiffAnswer, LiveDiffError> {
    if !agent_repository.is_dir() {
        return Ok(DiffAnswer::NotHere);
    }
    let git = GitCli::open(&repository)?;
    let Some(work) = git.branch_work(item, target)? else {
        return Ok(DiffAnswer::NotHere);
    };
    let head = work.commit();
    if since == Some(head) {
        return Ok(DiffAnswer::Unchanged);
    }
    Ok(DiffAnswer::Work(Work {
        target: target.to_string(),
        head,
        committed: git.parseable_diff(&work)?,
    }))
}

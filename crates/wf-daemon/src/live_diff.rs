//! The live diff of an item: what its work session on some device has changed so far, and what it
//! had changed at the snapshot a comment shows.
//!
//! The device of the session answers from its own repositories over `wf/query/2`, and a device that
//! asks for it dials that one, so the answer is the diff as the session's device shows it, against
//! the target it has. What is not committed is read only from the snapshots of the session
//! (SPEC §10), never by running git with the agent repository's `.git`.

use std::path::PathBuf;

use wf_core::id::{HashId, ItemId, SessionId, WorkforceId};
use wf_core::session::{CommentSnapshot, SessionRole};
use wf_core::state::State;
use wf_net::{DiffAnswer, Uncommitted, Work, WorkAt};
use wf_vcs::{GitCli, VcsError, item_repository};

use crate::proto::{LiveDiff, SnapshotDiff};
use crate::runner::snapshots::find_snapshot;
use crate::{Daemon, StorageClosed};

#[derive(Debug, thiserror::Error)]
pub enum LiveDiffError {
    #[error("workforce {0} is not loaded")]
    NotLoaded(WorkforceId),
    #[error("the workforce has no policy, so there is no target to compare against")]
    NoPolicy,
    #[error("session {} is not known here", .0.short())]
    UnknownSession(SessionId),
    #[error(transparent)]
    Storage(#[from] StorageClosed),
    #[error("could not read the work of the item")]
    Git(#[from] VcsError),
    #[error("could not read the transcript of the session")]
    Transcript(#[source] std::io::Error),
    #[error("reading the work of the item was cut short")]
    Task(#[source] tokio::task::JoinError),
}

pub(crate) fn target_of(state: &State) -> Option<String> {
    state
        .policy()
        .map(|config| config.policy.merge.target.clone())
}

impl Daemon {
    /// The work of `item` as this device has it: the head of its branch against the target, read
    /// the way `wf item diff` reads it, and the latest snapshot of its work session against the
    /// head. [`DiffAnswer::NotHere`] when there is no agent repository or branch of the item here,
    /// and [`DiffAnswer::Unchanged`] while the work is at `since`.
    pub(crate) async fn diff_here(
        &self,
        workforce: WorkforceId,
        item: ItemId,
        since: Option<WorkAt>,
    ) -> Result<DiffAnswer, LiveDiffError> {
        let loaded = self
            .loaded_workforce(&workforce)
            .ok_or(LiveDiffError::NotLoaded(workforce))?;
        let (target, session) = loaded
            .storage
            .with(move |replica| {
                let state = replica.state();
                let session = state
                    .holder(item, &SessionRole::Work)
                    .map(|session| session.id());
                (target_of(state), session)
            })
            .await?;
        let target = target.ok_or(LiveDiffError::NoPolicy)?;
        self.ensure_bare_repository(workforce, &loaded.storage)
            .await;
        let workforce_directory = self.workforce_directory(workforce);
        tokio::task::spawn_blocking(move || {
            read_work(workforce_directory, item, &target, session, since)
        })
        .await
        .map_err(LiveDiffError::Task)?
    }

    /// The work on `item` at the snapshot `at`, of a session of this device, against the target:
    /// [`DiffAnswer::NotHere`] when the session is not one of this device on `item`, or its
    /// transcript names no such snapshot.
    pub(crate) async fn snapshot_here(
        &self,
        workforce: WorkforceId,
        item: ItemId,
        at: CommentSnapshot,
    ) -> Result<DiffAnswer, LiveDiffError> {
        let loaded = self
            .loaded_workforce(&workforce)
            .ok_or(LiveDiffError::NotLoaded(workforce))?;
        let here = self.device();
        let (target, ours) = loaded
            .storage
            .with(move |replica| {
                let state = replica.state();
                let ours = state
                    .session(at.session())
                    .is_some_and(|session| session.device() == here && session.item() == item);
                (target_of(state), ours)
            })
            .await?;
        let target = target.ok_or(LiveDiffError::NoPolicy)?;
        if !ours {
            return Ok(DiffAnswer::NotHere);
        }
        self.ensure_bare_repository(workforce, &loaded.storage)
            .await;
        let workforce_directory = self.workforce_directory(workforce);
        let transcript = self.launch.data_dir.transcript(&workforce, at.session());
        tokio::task::spawn_blocking(move || {
            read_snapshot(workforce_directory, transcript, item, &target, at)
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
        since: Option<WorkAt>,
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

    /// The work on `item` at the snapshot `at`, from the device of its session: here without the
    /// network when the session ran here.
    pub(crate) async fn snapshot_diff(
        &self,
        workforce: WorkforceId,
        item: ItemId,
        at: CommentSnapshot,
    ) -> Result<SnapshotDiff, LiveDiffError> {
        let loaded = self
            .loaded_workforce(&workforce)
            .ok_or(LiveDiffError::NotLoaded(workforce))?;
        let device = loaded
            .storage
            .with(move |replica| {
                replica
                    .state()
                    .session(at.session())
                    .map(|session| session.device())
            })
            .await?
            .ok_or(LiveDiffError::UnknownSession(at.session()))?;
        if device == self.device() {
            return Ok(SnapshotDiff::Answer(
                self.snapshot_here(workforce, item, at).await?,
            ));
        }
        let asked = async {
            let node = self.node().await?;
            node.query_snapshot(wf_net::endpoint_id(device), workforce, item, at)
                .await
        };
        Ok(match asked.await {
            Ok(answer) => SnapshotDiff::Answer(answer),
            Err(error) => SnapshotDiff::Unreachable {
                device,
                reason: wf_core::error_chain(&error),
            },
        })
    }
}

fn read_work(
    workforce_directory: PathBuf,
    item: ItemId,
    target: &str,
    session: Option<SessionId>,
    since: Option<WorkAt>,
) -> Result<DiffAnswer, LiveDiffError> {
    if !item_repository(&workforce_directory, item).is_dir() {
        return Ok(DiffAnswer::NotHere);
    }
    let git = GitCli::open(&wf_vcs::bare_repository(&workforce_directory))?;
    let Some(work) = git.branch_work(item, target)? else {
        return Ok(DiffAnswer::NotHere);
    };
    let head = work.commit();
    let snapshot = match session {
        Some(session) => wf_vcs::latest_snapshot(&workforce_directory, item, session)?,
        None => None,
    };
    if since == Some(WorkAt { head, snapshot }) {
        return Ok(DiffAnswer::Unchanged);
    }
    let uncommitted = match snapshot {
        Some(snapshot) => Some(Uncommitted {
            snapshot,
            diff: wf_vcs::snapshot_diff(&workforce_directory, item, head, snapshot)?,
        }),
        None => None,
    };
    Ok(DiffAnswer::Work(Work {
        target: target.to_string(),
        head,
        committed: git.parseable_diff(&work)?,
        uncommitted,
    }))
}

fn read_snapshot(
    workforce_directory: PathBuf,
    transcript: PathBuf,
    item: ItemId,
    target: &str,
    at: CommentSnapshot,
) -> Result<DiffAnswer, LiveDiffError> {
    let transcript = match std::fs::read_to_string(&transcript) {
        Ok(transcript) => transcript,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DiffAnswer::NotHere);
        }
        Err(error) => return Err(LiveDiffError::Transcript(error)),
    };
    let Some(recorded) = find_snapshot(&transcript, at) else {
        return Ok(DiffAnswer::NotHere);
    };
    // A snapshot of a branch that was not there has nothing to be against.
    let Some(head) = recorded.head else {
        return Ok(DiffAnswer::NotHere);
    };
    let git = GitCli::open(&wf_vcs::bare_repository(&workforce_directory))?;
    // Against the target as it was: once the item is merged, the target has every recorded head.
    let committed = match recorded.base {
        Some(base) => git.parseable_diff_between(base, head)?,
        None => git.parseable_diff(&wf_vcs::Work::Unmerged {
            target: target.to_string(),
            commit: head,
        })?,
    };
    Ok(DiffAnswer::Work(Work {
        target: target.to_string(),
        head,
        committed,
        uncommitted: Some(Uncommitted {
            snapshot: recorded.tree,
            diff: wf_vcs::snapshot_diff(&workforce_directory, item, head, recorded.tree)?,
        }),
    }))
}

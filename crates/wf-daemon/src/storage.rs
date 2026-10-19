//! The storage task: one thread owns a replica and everything else talks to it over a channel.
//! Redb is blocking, so the thread keeps disk work off the async executor, and a single owner
//! keeps writes ordered.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::mpsc;

use tokio::sync::{oneshot, watch};
use wf_core::event::Event;
use wf_core::id::{EventId, WorkforceId};
use wf_core::identity::{DeviceKey, Signature};
use wf_core::invite::JoinRefusal;
use wf_core::sync::{BatchReceived, Versions};
use wf_net::{LogError, SyncLog};
use wf_store::{HistorySource, RedbStore, Replica, StoreError};

type Job = Box<dyn FnOnce(&mut Replica<RedbStore>) + Send>;

#[derive(Debug, thiserror::Error)]
#[error("the storage task has stopped")]
pub struct StorageClosed;

impl From<StorageClosed> for LogError {
    fn from(error: StorageClosed) -> Self {
        LogError::new(error)
    }
}

/// A handle to the task that owns one replica. Cheap to clone.
#[derive(Clone)]
pub struct StorageHandle {
    jobs: mpsc::Sender<Job>,
    heads: watch::Receiver<BTreeSet<EventId>>,
}

impl StorageHandle {
    /// Opens the store at `path` as a replica of `workforce` and starts the task that owns it.
    ///
    /// `history` gives the replica, each time it is opened, where to look for the history of the
    /// workforce it was forked from. After a write failed, what the replica holds in memory may
    /// be ahead of the log, so the task opens it again; `on_write_failure` is told the failed
    /// write, with its causes, and the error if opening again fails too. If that happens, or a
    /// job panics, the task ends and [`StorageHandle::is_closed`] turns true.
    pub fn open(
        path: PathBuf,
        workforce: WorkforceId,
        history: impl Fn() -> Option<Box<dyn HistorySource>> + Send + 'static,
        on_write_failure: impl Fn(&(dyn std::error::Error + 'static)) + Send + 'static,
    ) -> Result<Self, StoreError> {
        let open = move || {
            let mut replica = Replica::open(RedbStore::open(&path)?, workforce)?;
            if let Some(source) = history() {
                replica.set_history_source(source);
            }
            Ok(replica)
        };
        let replica = open()?;
        Ok(Self::spawn(replica, move |failed| {
            on_write_failure(&std::io::Error::other(failed.to_owned()));
            open().inspect_err(|error| on_write_failure(error))
        }))
    }

    fn spawn(
        replica: Replica<RedbStore>,
        reopen: impl Fn(&str) -> Result<Replica<RedbStore>, StoreError> + Send + 'static,
    ) -> Self {
        let (jobs, queue) = mpsc::channel::<Job>();
        let (publish, heads) = watch::channel(replica.state().heads().clone());
        std::thread::spawn(move || {
            let mut current = Some(replica);
            for job in queue {
                let Some(replica) = current.as_mut() else {
                    break;
                };
                job(replica);
                if let Some(failed) = replica.failed_write().map(str::to_owned) {
                    // The old replica holds the lock on the store until it is dropped.
                    drop(current.take());
                    current = reopen(&failed).ok();
                }
                let Some(replica) = current.as_ref() else {
                    break;
                };
                publish.send_if_modified(|heads| {
                    let current = replica.state().heads();
                    let changed = heads != current;
                    if changed {
                        heads.clone_from(current);
                    }
                    changed
                });
            }
        });
        Self { jobs, heads }
    }

    /// Whether the task has ended: the workforce has to be opened again.
    pub fn is_closed(&self) -> bool {
        self.heads.has_changed().is_err()
    }

    /// Runs `job` on the storage thread, after everything sent before it.
    pub async fn with<T: Send + 'static>(
        &self,
        job: impl FnOnce(&mut Replica<RedbStore>) -> T + Send + 'static,
    ) -> Result<T, StorageClosed> {
        let (reply, answer) = oneshot::channel();
        self.jobs
            .send(Box::new(move |replica| {
                let _ = reply.send(job(replica));
            }))
            .map_err(|_| StorageClosed)?;
        answer.await.map_err(|_| StorageClosed)
    }

    /// Like [`StorageHandle::with`], for code that runs on a thread of its own, such as another
    /// storage task. Blocks until the job has run.
    pub fn with_blocking<T: Send + 'static>(
        &self,
        job: impl FnOnce(&mut Replica<RedbStore>) -> T + Send + 'static,
    ) -> Result<T, StorageClosed> {
        let (reply, answer) = mpsc::channel();
        self.jobs
            .send(Box::new(move |replica| {
                let _ = reply.send(job(replica));
            }))
            .map_err(|_| StorageClosed)?;
        answer.recv().map_err(|_| StorageClosed)
    }

    /// Changes whenever the heads of the replica do.
    pub fn watch_heads(&self) -> watch::Receiver<BTreeSet<EventId>> {
        self.heads.clone()
    }
}

impl SyncLog for StorageHandle {
    async fn versions(&self) -> Result<Versions, LogError> {
        Ok(self.with(|replica| replica.versions()).await?)
    }

    async fn events_after(&self, theirs: &Versions) -> Result<Vec<Event>, LogError> {
        let theirs = theirs.clone();
        self.with(move |replica| replica.events_after(&theirs))
            .await?
            .map_err(LogError::new)
    }

    async fn events_by_id(&self, ids: &[EventId]) -> Result<Vec<Event>, LogError> {
        let ids = ids.to_vec();
        self.with(move |replica| replica.events_by_id(&ids))
            .await?
            .map_err(LogError::new)
    }

    async fn receive(&self, events: Vec<Event>) -> Result<BatchReceived, LogError> {
        self.with(move |replica| replica.receive_batch(events))
            .await?
            .map_err(LogError::new)
    }

    async fn is_bound(&self, device: DeviceKey) -> Result<bool, LogError> {
        Ok(self
            .with(move |replica| replica.state().principal_of(&device).is_some())
            .await?)
    }

    async fn check_join(
        &self,
        invite: EventId,
        device: DeviceKey,
        proof: Signature,
        now_ms: u64,
    ) -> Result<Result<(), JoinRefusal>, LogError> {
        Ok(self
            .with(move |replica| replica.state().check_join(&invite, device, &proof, now_ms))
            .await?)
    }

    async fn fill_history(&self) -> Result<(), LogError> {
        self.with(|replica| replica.fill_history())
            .await?
            .map(drop)
            .map_err(LogError::new)
    }

    async fn missing_parents(&self) -> Result<Vec<EventId>, LogError> {
        Ok(self.with(|replica| replica.missing_parents()).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wf_core::testing::Fixture;

    fn handle(fixture: &Fixture) -> StorageHandle {
        let workforce = fixture.workforce();
        let mut replica = Replica::open(RedbStore::in_memory().unwrap(), workforce).unwrap();
        replica.receive(fixture.genesis.clone()).unwrap();
        StorageHandle::spawn(replica, move |_| {
            Replica::open(RedbStore::in_memory().unwrap(), workforce)
        })
    }

    #[tokio::test]
    async fn a_job_that_panics_ends_the_task_and_says_so() {
        let fixture = Fixture::new(1);
        let storage = handle(&fixture);
        assert!(!storage.is_closed());
        let result = storage.with(|_| panic!("a bug in a job")).await;
        assert!(result.is_err());
        // The thread has unwound by the time the reply channel closed, give or take a moment.
        for _ in 0..100 {
            if storage.is_closed() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(storage.is_closed());
        assert!(storage.with(|_| ()).await.is_err());
    }

    #[tokio::test]
    async fn the_task_serves_jobs_in_order() {
        let fixture = Fixture::new(1);
        let storage = handle(&fixture);
        let count = storage
            .with(|replica| replica.state().event_count())
            .await
            .unwrap();
        assert_eq!(count, 1);
    }
}

//! The storage task: one thread owns each replica and everything else talks to it over a channel.
//! Redb is blocking, so the thread keeps disk work off the async executor, and a single owner
//! keeps writes ordered.

use std::collections::BTreeSet;
use std::sync::mpsc;

use tokio::sync::{oneshot, watch};
use wf_core::event::Event;
use wf_core::id::EventId;
use wf_core::identity::DeviceKey;
use wf_core::invite::JoinRefusal;
use wf_core::sync::Versions;
use wf_net::{LogError, ReceiveSummary, Signature, SyncLog, replica};
use wf_store::{RedbStore, Replica};

type Job = Box<dyn FnOnce(&mut Replica<RedbStore>) + Send>;

#[derive(Debug, thiserror::Error)]
#[error("the storage task has stopped")]
pub struct StorageClosed;

impl From<StorageClosed> for LogError {
    fn from(error: StorageClosed) -> Self {
        LogError(Box::new(error))
    }
}

/// A handle to the task that owns one replica. Cheap to clone.
#[derive(Clone)]
pub struct StorageHandle {
    jobs: mpsc::Sender<Job>,
    heads: watch::Receiver<BTreeSet<EventId>>,
}

impl StorageHandle {
    pub fn spawn(replica: Replica<RedbStore>) -> Self {
        let (jobs, queue) = mpsc::channel::<Job>();
        let (publish, heads) = watch::channel(replica.state().heads().clone());
        std::thread::spawn(move || {
            let mut replica = replica;
            for job in queue {
                job(&mut replica);
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
        Ok(self.with(|replica| replica::versions(replica)).await?)
    }

    async fn events_after(&self, theirs: &Versions) -> Result<Vec<Event>, LogError> {
        let theirs = theirs.clone();
        self.with(move |replica| replica::events_after(replica, &theirs))
            .await?
            .map_err(LogError::from)
    }

    async fn events_by_id(&self, ids: &[EventId]) -> Result<Vec<Event>, LogError> {
        let ids = ids.to_vec();
        self.with(move |replica| replica::events_by_id(replica, &ids))
            .await?
            .map_err(LogError::from)
    }

    async fn receive(&self, events: Vec<Event>) -> Result<ReceiveSummary, LogError> {
        self.with(move |replica| replica::receive(replica, events))
            .await?
            .map_err(LogError::from)
    }

    async fn is_bound(&self, device: DeviceKey) -> Result<bool, LogError> {
        Ok(self
            .with(move |replica| replica::is_bound(replica, device))
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
            .with(move |replica| replica::check_join(replica, invite, device, proof, now_ms))
            .await?)
    }

    async fn missing(&self) -> Result<Vec<EventId>, LogError> {
        self.with(replica::missing).await?.map_err(LogError::from)
    }
}

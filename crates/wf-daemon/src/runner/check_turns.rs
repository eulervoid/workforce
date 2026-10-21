//! Turns at the checks of a workforce, which share its check worktree: one run at a time, and a
//! merge before a submission, so that approved work does not wait behind work nobody reviewed.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;
use wf_core::id::WorkforceId;

use super::check::CheckStage;

/// Who holds the checks of each workforce and who waits for them. A workforce is in the map while
/// a run holds its checks.
#[derive(Default)]
pub(crate) struct CheckTurns {
    held: Mutex<BTreeMap<WorkforceId, Waiting>>,
}

/// The runs that wait for the checks of one workforce, each list in the order they came.
#[derive(Default)]
struct Waiting {
    merges: VecDeque<oneshot::Sender<CheckTurn>>,
    submissions: VecDeque<oneshot::Sender<CheckTurn>>,
}

/// Holds the checks of one workforce until dropped, which hands them to the next run.
pub(super) struct CheckTurn {
    turns: Arc<CheckTurns>,
    workforce: WorkforceId,
}

impl CheckTurns {
    /// Waits for the checks of `workforce`: a merge after the merges that wait already, a
    /// submission after every waiting run.
    pub(super) async fn take(
        self: &Arc<Self>,
        workforce: WorkforceId,
        stage: CheckStage,
    ) -> CheckTurn {
        let (send, receive) = oneshot::channel();
        {
            let mut held = self.lock();
            let Some(waiting) = held.get_mut(&workforce) else {
                held.insert(workforce, Waiting::default());
                return CheckTurn {
                    turns: Arc::clone(self),
                    workforce,
                };
            };
            match stage {
                CheckStage::Merge => waiting.merges.push_back(send),
                CheckStage::Submission => waiting.submissions.push_back(send),
            }
        }
        // A wait cancelled after the turn was sent drops the turn with the channel, which hands it
        // on.
        receive
            .await
            .expect("a waiting run is dropped from the list only to be sent its turn")
    }

    /// Hands the checks of `workforce` to the next waiting run, or frees them.
    fn hand_on(self: &Arc<Self>, workforce: WorkforceId) {
        let next = {
            let mut held = self.lock();
            let Some(waiting) = held.get_mut(&workforce) else {
                return;
            };
            let next = waiting
                .merges
                .pop_front()
                .or_else(|| waiting.submissions.pop_front());
            if next.is_none() {
                held.remove(&workforce);
            }
            next
        };
        if let Some(next) = next {
            // A run that stopped waiting drops the turn it was sent, which hands it on again.
            let _ = next.send(CheckTurn {
                turns: Arc::clone(self),
                workforce,
            });
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<WorkforceId, Waiting>> {
        self.held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Drop for CheckTurn {
    fn drop(&mut self) {
        self.turns.hand_on(self.workforce);
    }
}

#[cfg(test)]
mod tests {
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    use super::*;

    fn workforce(seed: u8) -> WorkforceId {
        WorkforceId::from_bytes([seed; 32])
    }

    /// Polls `future` once: whether it has its turn yet.
    fn poll<T>(future: std::pin::Pin<&mut impl Future<Output = T>>) -> Option<T> {
        match future.poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(value) => Some(value),
            Poll::Pending => None,
        }
    }

    #[test]
    fn a_waiting_merge_runs_before_a_submission_that_waited_longer() {
        let turns = Arc::new(CheckTurns::default());
        let running = poll(pin!(turns.take(workforce(1), CheckStage::Submission))).unwrap();
        let mut submission = pin!(turns.take(workforce(1), CheckStage::Submission));
        let mut merge = pin!(turns.take(workforce(1), CheckStage::Merge));
        assert!(poll(submission.as_mut()).is_none());
        assert!(poll(merge.as_mut()).is_none());

        drop(running);
        assert!(poll(submission.as_mut()).is_none(), "the merge goes first");
        let merging = poll(merge.as_mut()).unwrap();
        drop(merging);
        assert!(poll(submission.as_mut()).is_some());
        assert!(turns.lock().is_empty());
    }

    #[test]
    fn merges_run_in_the_order_they_came_and_other_workforces_do_not_wait() {
        let turns = Arc::new(CheckTurns::default());
        let running = poll(pin!(turns.take(workforce(1), CheckStage::Merge))).unwrap();
        let mut first = pin!(turns.take(workforce(1), CheckStage::Merge));
        let mut second = pin!(turns.take(workforce(1), CheckStage::Merge));
        assert!(poll(first.as_mut()).is_none());
        assert!(poll(second.as_mut()).is_none());
        assert!(poll(pin!(turns.take(workforce(2), CheckStage::Submission))).is_some());

        drop(running);
        assert!(poll(second.as_mut()).is_none());
        let first = poll(first.as_mut()).unwrap();
        drop(first);
        assert!(poll(second.as_mut()).is_some());
    }

    #[test]
    fn a_run_that_stopped_waiting_hands_its_turn_on() {
        let turns = Arc::new(CheckTurns::default());
        let running = poll(pin!(turns.take(workforce(1), CheckStage::Submission))).unwrap();
        let mut submission = pin!(turns.take(workforce(1), CheckStage::Submission));
        assert!(poll(submission.as_mut()).is_none());
        {
            let mut merge = Box::pin(turns.take(workforce(1), CheckStage::Merge));
            assert!(poll(merge.as_mut()).is_none());
            drop(running);
            // The merge was sent the turn but stops waiting before it sees it.
        }
        assert!(poll(submission.as_mut()).is_some());
        assert!(turns.lock().is_empty());
    }
}

//! The checks of the policy at submission: the daemon runs them on the merge of the work into the
//! target, in the sandbox of agents, while the session waits, and the work is reviewed only when
//! they pass.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;
use tokio::task::AbortHandle;
use wf_core::event::{CommentKind, Commented, EventBody};
use wf_core::id::{CommentId, HashId, ItemId, SessionId, WorkforceId};
use wf_core::identity::Actor;
use wf_core::lifecycle::{AfterFailedChecks, Finish, SessionEnd};
use wf_core::policy::{CheckConfig, MergeMode};
use wf_core::session::CommitId;
use wf_vcs::{GitCli, MergeTree};

use super::check::{Blame, CheckStage, FailedCheck, told_all};
use super::{SessionError, blocking};
use crate::proto::EndAnswer;
use crate::{Daemon, Loaded};

/// Where the checks of a submission are, per session.
#[derive(Default)]
pub(crate) struct SubmissionChecks {
    sessions: Mutex<BTreeMap<SessionId, Checks>>,
    changed: Notify,
}

enum Checks {
    /// Running on the merge of `commit`.
    Running { commit: CommitId, task: AbortHandle },
    /// Passed on the merge of `commit`, so that submitting it ends the session.
    Passed { commit: CommitId },
    /// Failed by the work's fault: the next prompt of the session, which has not been given yet.
    Failed { prompt: String },
}

impl SubmissionChecks {
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<SessionId, Checks>> {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Starts the checks of `session` with `spawn`, which stops the ones running.
    fn begin(
        &self,
        session: SessionId,
        commit: CommitId,
        spawn: impl FnOnce() -> tokio::task::JoinHandle<()>,
    ) {
        let mut sessions = self.lock();
        if let Some(Checks::Running { task, .. }) = sessions.remove(&session) {
            task.abort();
        }
        // Spawned with the lock held, so that the task cannot settle before it is entered.
        let task = spawn().abort_handle();
        sessions.insert(session, Checks::Running { commit, task });
    }

    /// Whether the checks of `session` run now.
    fn is_running(&self, session: SessionId) -> bool {
        matches!(self.lock().get(&session), Some(Checks::Running { .. }))
    }

    /// Whether the session waits for its checks, which run or have passed and end it.
    fn is_pending(&self, session: SessionId) -> bool {
        matches!(
            self.lock().get(&session),
            Some(Checks::Running { .. } | Checks::Passed { .. })
        )
    }

    /// The commit whose checks of `session` passed.
    fn passed(&self, session: SessionId) -> Option<CommitId> {
        match self.lock().get(&session) {
            Some(Checks::Passed { commit }) => Some(*commit),
            Some(Checks::Running { .. } | Checks::Failed { .. }) | None => None,
        }
    }

    /// What the checks of the `commit` of `session` came to, unless they were stopped or replaced
    /// meanwhile.
    fn settle(&self, session: SessionId, commit: CommitId, settled: Option<Checks>) {
        let mut sessions = self.lock();
        let pending = matches!(
            sessions.get(&session),
            Some(Checks::Running { commit: pending, .. } | Checks::Passed { commit: pending })
                if *pending == commit
        );
        if pending {
            match settled {
                Some(settled) => sessions.insert(session, settled),
                None => sessions.remove(&session),
            };
        }
        drop(sessions);
        self.changed.notify_waiters();
    }

    /// Completes once the session does not wait for its checks any more.
    pub(super) async fn until_settled(&self, session: SessionId) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if !self.is_pending(session) {
                return;
            }
            changed.await;
        }
    }

    /// The prompt that tells `session` which checks failed, once.
    pub(super) fn take_failure(&self, session: SessionId) -> Option<String> {
        let mut sessions = self.lock();
        match sessions.remove(&session) {
            Some(Checks::Failed { prompt }) => Some(prompt),
            Some(other) => {
                sessions.insert(session, other);
                None
            }
            None => None,
        }
    }

    /// Forgets the checks of `session`, stopping those that run.
    pub(super) fn clear(&self, session: SessionId) {
        if let Some(Checks::Running { task, .. }) = self.lock().remove(&session) {
            task.abort();
        }
        self.changed.notify_waiters();
    }
}

/// What a submission is checked on.
pub(super) struct Submission {
    pub workforce: WorkforceId,
    pub session: SessionId,
    pub item: ItemId,
    pub commit: CommitId,
    /// How the session ends once the checks pass.
    pub finish: Finish,
}

impl Daemon {
    /// Whether the submission of `commit` by `session` has been checked: its checks passed, or
    /// the policy has none.
    pub(super) fn submission_checked(&self, session: SessionId, commit: CommitId) -> bool {
        self.submissions.passed(session) == Some(commit)
    }

    /// The checks of the policy to run on the merge, none when the policy has none.
    pub(super) fn checks_of(
        state: &wf_core::state::State,
    ) -> Result<Vec<CheckConfig>, SessionError> {
        let policy = &state.policy().ok_or(SessionError::NoPolicy)?.policy;
        Ok(match policy.merge.mode {
            MergeMode::Local => policy.merge.checks.clone(),
        })
    }

    /// Starts the checks of a submission in the background, and answers that they run. The session
    /// goes on until they are done: the agent's next prompt tells what failed, and when they pass
    /// the session ends as the agent said.
    pub(super) fn begin_submission_checks(
        self: &Arc<Self>,
        loaded: &Arc<Loaded>,
        submission: Submission,
        checks: Vec<CheckConfig>,
    ) -> Result<EndAnswer, SessionError> {
        if self.submissions.is_running(submission.session) {
            return Err(SessionError::ChecksRunning);
        }
        let names = checks.iter().map(|check| check.name.clone()).collect();
        let (daemon, loaded) = (Arc::clone(self), Arc::clone(loaded));
        let (session, commit) = (submission.session, submission.commit);
        self.submissions.begin(session, commit, move || {
            tokio::spawn(async move {
                let settled = match daemon.check_submission(&loaded, &submission, &checks).await {
                    Ok(settled) => settled,
                    Err(error) => {
                        tracing::warn!(
                            "could not check the submission of session {}: {}",
                            submission.session.short(),
                            wf_core::error_chain(&error)
                        );
                        Some(Checks::Failed {
                            prompt: format!(
                                "Your submission was not sent to review: the checks could not \
                                 run: {}\nSubmit again to try again, or release the item with \
                                 `wf work release --note \"<why>\"`.",
                                wf_core::error_chain(&error)
                            ),
                        })
                    }
                };
                daemon
                    .submissions
                    .settle(submission.session, submission.commit, settled);
            })
        });
        Ok(EndAnswer::Checking { checks: names })
    }

    /// Runs `checks` on the merge of the submitted work and tells what follows from it: `None` when
    /// nothing is left to tell the session, as when its submission went through.
    async fn check_submission(
        self: &Arc<Self>,
        loaded: &Arc<Loaded>,
        submission: &Submission,
        checks: &[CheckConfig],
    ) -> Result<Option<Checks>, SessionError> {
        let Submission {
            workforce, commit, ..
        } = *submission;
        let state = self.snapshot(loaded).await?;
        let target = state
            .policy()
            .ok_or(SessionError::NoPolicy)?
            .policy
            .merge
            .target
            .clone();
        let repository = self
            .repository_of(workforce)
            .ok_or(SessionError::NoRepository)?;
        let made = {
            let (repository, target) = (repository.clone(), target.clone());
            blocking(move || GitCli::open(&repository)?.merge_tree(&target, commit)).await??
        };
        let failed = match made {
            MergeTree::Clean { base, tree } => {
                self.check_merge(
                    workforce,
                    &repository,
                    checks,
                    base,
                    tree,
                    CheckStage::Submission,
                )
                .await?
            }
            // Nothing is left to merge, or the merge conflicts: the merge tells.
            MergeTree::Landed { .. } | MergeTree::Conflict { .. } => Vec::new(),
        };
        let (by_work, not_by_work): (Vec<_>, Vec<_>) = failed
            .into_iter()
            .partition(|failed| failed.blame == Blame::Work);
        if by_work.is_empty() {
            self.submission_passed(loaded, submission, &target, &not_by_work)
                .await?;
            return Ok(None);
        }
        self.submission_failed(loaded, submission, &target, &by_work, &not_by_work)
            .await
    }

    /// Tells the principal of the checks that failed for another reason than the work, and ends the
    /// session as the agent said, now that the checks passed.
    async fn submission_passed(
        self: &Arc<Self>,
        loaded: &Arc<Loaded>,
        submission: &Submission,
        target: &str,
        not_by_work: &[FailedCheck],
    ) -> Result<(), SessionError> {
        // Before the session ends, so that the item goes to review with the blocker open, and
        // the merge waits for the principal.
        if !not_by_work.is_empty() {
            let state = self.snapshot(loaded).await?;
            let principal = self.principal_of_device(&state)?;
            for failed in not_by_work {
                let text = failed.told(target);
                self.comment_to_principal(
                    loaded,
                    &state,
                    principal,
                    submission.item,
                    CommentKind::Blocker,
                    text,
                )
                .await?;
            }
        }
        self.submissions.settle(
            submission.session,
            submission.commit,
            Some(Checks::Passed {
                commit: submission.commit,
            }),
        );
        self.finish_session(
            submission.workforce,
            loaded,
            submission.session,
            SessionEnd::Finish(submission.finish.clone()),
        )
        .await?;
        Ok(())
    }

    /// Tells the agent of the checks that failed by the work, as a change request, unless they
    /// failed too often since the principal last acted: then the principal is told and the session
    /// waits for them. What failed for another reason is told to the principal.
    async fn submission_failed(
        &self,
        loaded: &Loaded,
        submission: &Submission,
        target: &str,
        by_work: &[FailedCheck],
        not_by_work: &[FailedCheck],
    ) -> Result<Option<Checks>, SessionError> {
        let item = submission.item;
        // The checks took a while: the principal may have acted on the item meanwhile.
        let state = self.snapshot(loaded).await?;
        let principal = self.principal_of_device(&state)?;
        if !not_by_work.is_empty() {
            let text = format!(
                "Not sent back for these, which are not the work's fault:\n\n{}",
                told_all(not_by_work, target)
            );
            self.comment_to_principal(loaded, &state, principal, item, CommentKind::Plain, text)
                .await?;
        }
        match state.after_failed_checks(item) {
            AfterFailedChecks::BackToWork { .. } => {
                let failures = told_all(by_work, target);
                let written = self
                    .author(
                        loaded,
                        Actor::Daemon(principal),
                        EventBody::Commented(Commented {
                            kind: CommentKind::CheckFailure,
                            ..Commented::plain(item, failures.clone())
                        }),
                    )
                    .await?;
                let comment = CommentId::from(written.id());
                Ok(Some(Checks::Failed {
                    prompt: wf_core::brief::checks_failed_nudge(item, &comment.short(), &failures),
                }))
            }
            AfterFailedChecks::WaitsForPrincipal { runs } => {
                let text = format!(
                    "The checks failed on the merge into {target} again, after {runs} failed runs \
                     since you last acted on the item, so the work is not sent back by itself. \
                     Resolve this to check it again and send it back with what fails.\n\n{}",
                    told_all(by_work.iter().chain(not_by_work), target)
                );
                self.comment_to_principal(
                    loaded,
                    &state,
                    principal,
                    item,
                    CommentKind::Blocker,
                    text,
                )
                .await?;
                // The blocker parks the session; there is nothing to prompt it with.
                Ok(None)
            }
        }
    }
}

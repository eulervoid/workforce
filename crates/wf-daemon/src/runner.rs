//! Running sessions: the daemon starts a harness for a work session, prompts it turn after turn
//! until the session ends, and applies what follows from the end to the item.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Instant;
use wf_core::event::{Comment, Event, EventBody, ItemMoved, Merged, SessionEnded, SessionStarted};
use wf_core::id::{CommentId, EventId, HashId, ItemId, SessionId, WorkforceId};
use wf_core::identity::Actor;
use wf_core::item::{ItemView, Standing};
use wf_core::lifecycle::{AfterEnd, TurnOutcome};
use wf_core::name::{AgentName, HarnessName, StateName};
use wf_core::session::{CommitId, EndReason, SessionRole};
use wf_core::state::State;
use wf_harness::{
    AcpAgentConfig, Conversation, Launch, PermissionMode, Permissions, ReviewPackage, WorkPackage,
};
use wf_store::AuthorError;
use wf_vcs::{GitCli, MergeOutcome, VcsAdapter, VcsError, item_branch, item_name};

use crate::permissions::Asking;
use crate::proto::Finish;
use crate::{Daemon, Loaded, StorageClosed};

/// How often a merge that found the target moved is tried again before giving up for now.
const MERGE_ATTEMPTS: usize = 3;

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("this machine is not bound to a member of the workforce")]
    NotBound,
    #[error("`{0}` is not an agent of this machine's member")]
    UnknownAgent(AgentName),
    #[error("harness `{0}` is not one wf knows how to start; see `wf agent list`")]
    UnknownHarness(HarnessName),
    #[error("there is no item {0}")]
    UnknownItem(ItemId),
    #[error("{0} is archived")]
    Archived(ItemId),
    #[error("{0} already has a live session; stop it first")]
    Claimed(ItemId),
    #[error("there is no session {0}")]
    UnknownSession(SessionId),
    #[error("the session runs on another machine")]
    NotOurs(SessionId),
    #[error("only a work session can be started this way")]
    NotWork,
    #[error("{0}")]
    Vcs(#[from] VcsError),
    #[error(transparent)]
    Authoring(#[from] AuthorError),
    #[error(transparent)]
    Storage(#[from] StorageClosed),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Tells the task of a running session that it has ended and its harness is to be stopped.
#[derive(Clone, Copy)]
enum Stop {
    /// Cancel the turn that is running.
    Now,
    /// The agent ended the session from inside its turn, so it is still waiting for the answer
    /// of the command that did: let it finish the turn, within a grace period.
    AfterTurn,
}

/// How long an agent that ended its own session gets to finish its turn.
const GRACE: Duration = Duration::from_secs(20);

/// The sessions this daemon is running. Lost with the process, which is why a daemon that starts
/// ends every live session of its machine.
#[derive(Default)]
pub(crate) struct Runners {
    running: Mutex<BTreeMap<SessionId, mpsc::UnboundedSender<Stop>>>,
}

impl Runners {
    fn add(&self, session: SessionId, stop: mpsc::UnboundedSender<Stop>) {
        self.lock().insert(session, stop);
    }

    /// Stops the task of `session`, if it runs here.
    fn stop(&self, session: SessionId, how: Stop) {
        if let Some(stop) = self.lock().remove(&session) {
            let _ = stop.send(how);
        }
    }

    pub(crate) fn is_running(&self, session: SessionId) -> bool {
        self.lock().contains_key(&session)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<SessionId, mpsc::UnboundedSender<Stop>>> {
        self.running
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Whether a merge is the first try of a piece of work, or one more try of work that waited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Attempt {
    /// Made as the session that did the work ends, so the item moves for that session.
    First { session: SessionId },
    /// Waiting again is not worth another comment.
    Retry,
}

impl Attempt {
    fn by_session(self) -> Option<SessionId> {
        match self {
            Attempt::First { session } => Some(session),
            Attempt::Retry => None,
        }
    }
}

/// Reviewed work to merge into the target branch, and what to do with its item afterwards.
pub(crate) struct MergeRequest {
    item: ItemId,
    /// The reviewed commit.
    commit: CommitId,
    /// Where the item goes once merged.
    done: StateName,
    /// The comment that says why, for that move.
    because: Option<CommentId>,
    attempt: Attempt,
}

/// The items being merged right now, so that one merge is made once.
#[derive(Default)]
pub(crate) struct Merging {
    items: Mutex<std::collections::BTreeSet<ItemId>>,
}

pub(crate) struct MergeGuard<'a> {
    merging: &'a Merging,
    item: ItemId,
}

impl Merging {
    fn start(&self, item: ItemId) -> Option<MergeGuard<'_>> {
        let mut items = self
            .items
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        items.insert(item).then_some(MergeGuard {
            merging: self,
            item,
        })
    }
}

impl Drop for MergeGuard<'_> {
    fn drop(&mut self) {
        self.merging
            .items
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.item);
    }
}

/// What a session is, to run it.
struct Run {
    workforce: WorkforceId,
    session: SessionId,
    item: ItemId,
    role: SessionRole,
    agent: AgentName,
    harness: AcpAgentConfig,
    worktree: PathBuf,
    transcript: PathBuf,
    branch: String,
    target: String,
    repository: PathBuf,
    nudges: u32,
    park_timeout: Duration,
    max_duration: Option<Duration>,
    permissions: Arc<dyn Permissions>,
}

/// How the task of a session ends.
enum End {
    /// Someone else ended the session, or stopped it.
    Over,
    Finish(Finish),
}

impl Daemon {
    pub(crate) async fn snapshot(&self, loaded: &Loaded) -> Result<State, StorageClosed> {
        loaded.storage.with(|replica| replica.state().clone()).await
    }

    /// Writes an event as the device. `actor` says on whose behalf.
    pub(crate) async fn author(
        &self,
        loaded: &Loaded,
        actor: Actor,
        body: EventBody,
    ) -> Result<Event, SessionError> {
        let signer = self.signer().clone();
        let now_ms = crate::now_ms();
        Ok(loaded
            .storage
            .with(move |replica| replica.author(&signer, actor, now_ms, body))
            .await??)
    }

    fn principal_of_device(
        &self,
        state: &State,
    ) -> Result<wf_core::identity::PrincipalKey, SessionError> {
        state
            .principal_of(&self.device())
            .ok_or(SessionError::NotBound)
    }

    fn harness_command(&self, harness: &HarnessName) -> Option<AcpAgentConfig> {
        self.config
            .harnesses
            .get(harness)
            .cloned()
            .or_else(|| wf_harness::command_for(harness.as_str()))
    }

    fn workforce_directory(&self, workforce: WorkforceId) -> PathBuf {
        self.config.data_dir.join(workforce.to_string())
    }

    /// Where the repository of a workforce is on this machine, as the last `wf` run there said.
    pub(crate) fn repository_of(&self, workforce: WorkforceId) -> Option<PathBuf> {
        let text = std::fs::read_to_string(
            self.workforce_directory(workforce)
                .join(crate::REPOSITORY_FILE),
        )
        .ok()?;
        Some(PathBuf::from(text.trim())).filter(|path| !path.as_os_str().is_empty())
    }

    fn remember_repository(
        &self,
        workforce: WorkforceId,
        repository: &Path,
    ) -> Result<(), SessionError> {
        let directory = self.workforce_directory(workforce);
        crate::remember_repository(&directory, repository).map_err(|source| SessionError::Io {
            path: directory,
            source,
        })
    }

    /// Starts a work session of `agent` on `item`: the worktree and the branch, the
    /// `SessionStarted` event, the item in its active state, and a task that runs the harness.
    pub(crate) async fn start_session(
        self: &Arc<Self>,
        workforce: WorkforceId,
        loaded: &Arc<Loaded>,
        agent: AgentName,
        item: ItemId,
        repository: PathBuf,
    ) -> Result<SessionId, SessionError> {
        self.begin_session(
            workforce,
            loaded,
            agent,
            item,
            SessionRole::Work,
            repository,
        )
        .await
    }

    /// Starts a review session of `agent` on the work submitted for `item`: a detached checkout
    /// of `commit`, or an empty directory when it was submitted without commits.
    pub(crate) async fn start_review(
        self: &Arc<Self>,
        workforce: WorkforceId,
        loaded: &Arc<Loaded>,
        agent: AgentName,
        item: ItemId,
        commit: Option<CommitId>,
        repository: PathBuf,
    ) -> Result<SessionId, SessionError> {
        self.begin_session(
            workforce,
            loaded,
            agent,
            item,
            SessionRole::Review { commit },
            repository,
        )
        .await
    }

    /// Where a review session looks at a commit of an item.
    fn review_worktree(
        &self,
        workforce: WorkforceId,
        item: ItemId,
        commit: Option<CommitId>,
    ) -> PathBuf {
        let suffix = commit.map_or("answer".to_string(), |commit| {
            commit.to_string()[..8].into()
        });
        self.workforce_directory(workforce)
            .join("worktrees")
            .join(format!("{}-{suffix}", item_name(item)))
    }

    async fn begin_session(
        self: &Arc<Self>,
        workforce: WorkforceId,
        loaded: &Arc<Loaded>,
        agent: AgentName,
        item: ItemId,
        role: SessionRole,
        repository: PathBuf,
    ) -> Result<SessionId, SessionError> {
        let state = self.snapshot(loaded).await?;
        let principal = self.principal_of_device(&state)?;
        let definition = state
            .agent(&agent, principal)
            .ok_or_else(|| SessionError::UnknownAgent(agent.clone()))?
            .clone();
        let harness = self
            .harness_command(&definition.harness)
            .ok_or_else(|| SessionError::UnknownHarness(definition.harness.clone()))?;
        match state.item(item).ok_or(SessionError::UnknownItem(item))? {
            ItemView::Live(_) => {}
            ItemView::Archived(_) => return Err(SessionError::Archived(item)),
        }
        if role == SessionRole::Work && state.claim(item).is_some() {
            return Err(SessionError::Claimed(item));
        }
        let target = state
            .policy()
            .expect("a workforce has a policy")
            .policy
            .merge
            .target
            .clone();
        let branch = item_branch(item);
        let (worktree, repository) = match role {
            SessionRole::Work => {
                let worktree = self
                    .workforce_directory(workforce)
                    .join("worktrees")
                    .join(item_name(item));
                let (branch, path, target) = (branch.clone(), worktree.clone(), target.clone());
                let repository = blocking(move || {
                    let git = GitCli::open(&repository)?;
                    git.prepare_worktree(&branch, &path, &target)?;
                    Ok::<_, VcsError>(git.repository().to_path_buf())
                })
                .await??;
                (worktree, repository)
            }
            SessionRole::Review { commit } => {
                let worktree = self.review_worktree(workforce, item, commit);
                let path = worktree.clone();
                let repository = blocking(move || {
                    let git = GitCli::open(&repository)?;
                    // A look at the same commit by an earlier review may have left its checkout.
                    let _ = git.remove_worktree(&path);
                    let _ = std::fs::remove_dir_all(&path);
                    match commit {
                        Some(commit) => git.detached_worktree(&path, commit)?,
                        None => std::fs::create_dir_all(&path)?,
                    }
                    Ok::<_, VcsError>(git.repository().to_path_buf())
                })
                .await??;
                (worktree, repository)
            }
        };
        self.remember_repository(workforce, &repository)?;

        let started = self
            .author(
                loaded,
                Actor::Human(principal),
                EventBody::SessionStarted(SessionStarted {
                    agent: agent.clone(),
                    item,
                    role,
                }),
            )
            .await?;
        let session = SessionId::from(started.id());
        let state = self.snapshot(loaded).await?;
        if let Some((item, to)) = state.start_move(session) {
            self.move_item(loaded, principal, item, to, None, Some(session))
                .await?;
        }

        let transcripts = self.workforce_directory(workforce).join("transcripts");
        std::fs::create_dir_all(&transcripts).map_err(|source| SessionError::Io {
            path: transcripts.clone(),
            source,
        })?;
        let autonomy = definition.autonomy;
        let run = Run {
            workforce,
            session,
            item,
            role,
            agent,
            harness,
            worktree,
            transcript: transcripts.join(format!("{session}.jsonl")),
            branch,
            target,
            repository,
            nudges: autonomy.nudges,
            park_timeout: autonomy.park_timeout,
            max_duration: autonomy.max_duration,
            permissions: match autonomy.permissions {
                wf_core::autonomy::PermissionMode::Allow => Arc::new(PermissionMode::Allow),
                wf_core::autonomy::PermissionMode::Deny => Arc::new(PermissionMode::Deny),
                wf_core::autonomy::PermissionMode::Ask => Arc::new(Asking {
                    held: Arc::clone(&self.held),
                    workforce,
                    session,
                }),
            },
        };
        let (stop, stopped) = mpsc::unbounded_channel();
        self.runners.add(session, stop);
        tokio::spawn(run_session(
            Arc::clone(self),
            Arc::clone(loaded),
            run,
            stopped,
        ));
        Ok(session)
    }

    async fn move_item(
        &self,
        loaded: &Loaded,
        principal: wf_core::identity::PrincipalKey,
        item: ItemId,
        to: StateName,
        because: Option<CommentId>,
        by_session: Option<SessionId>,
    ) -> Result<(), SessionError> {
        self.author(
            loaded,
            Actor::Human(principal),
            EventBody::ItemMoved(ItemMoved {
                item,
                to,
                by_session,
                because,
            }),
        )
        .await?;
        Ok(())
    }

    /// Ends a live session of this machine and applies what follows to its item. Ending one that
    /// has ended already only makes sure its task is stopped.
    pub(crate) async fn finish_session(
        self: &Arc<Self>,
        workforce: WorkforceId,
        loaded: &Arc<Loaded>,
        session: SessionId,
        finish: Finish,
    ) -> Result<(), SessionError> {
        let state = self.snapshot(loaded).await?;
        let folded = state
            .session(session)
            .ok_or(SessionError::UnknownSession(session))?;
        if folded.device() != self.device() {
            return Err(SessionError::NotOurs(session));
        }
        if !folded.is_live() {
            self.runners.stop(session, Stop::Now);
            return Ok(());
        }
        let principal = self.principal_of_device(&state)?;
        let item = folded.item();
        let role = folded.role();
        let by_agent = Actor::Agent {
            agent: folded.agent().clone(),
            session,
        };
        let by_device = Actor::Human(principal);

        let (reason, actor, note) = match &finish {
            Finish::Submit { note } => {
                let commit = self.submitted_commit(workforce, &state, item).await?;
                (
                    EndReason::Submitted { commit },
                    by_agent.clone(),
                    note.clone(),
                )
            }
            Finish::Release { note } => (EndReason::Released, by_agent.clone(), note.clone()),
            Finish::Reviewed => (EndReason::Reviewed, by_agent.clone(), None),
            Finish::Stop => (EndReason::Cancelled, by_device.clone(), None),
            Finish::Blocked => (EndReason::Blocked, by_device.clone(), None),
            Finish::TimedOut => (EndReason::TimedOut, by_device.clone(), None),
            Finish::Failed => (EndReason::Failed, by_device.clone(), None),
            Finish::GaveUp { turns } => (
                EndReason::Released,
                by_device.clone(),
                Some(format!(
                    "Gave up: {turns} turns in a row without a new commit."
                )),
            ),
        };
        // A submission with open change requests is refused before anything is written.
        state
            .check_policy(&EventBody::SessionEnded(SessionEnded { session, reason }))
            .map_err(|violation| SessionError::Authoring(violation.into()))?;
        // The comment comes first: once the session has ended it can no longer write.
        let because = match note {
            Some(note) => {
                let written = self
                    .author(
                        loaded,
                        actor.clone(),
                        EventBody::Comment(Comment::plain(item, note)),
                    )
                    .await?;
                Some(CommentId::from(written.id()))
            }
            None => None,
        };
        // The item moves while the session still holds it, so that it is never ready in between.
        let effect = match state.after_end(session, reason) {
            None | Some(AfterEnd::Nothing) => Ok(()),
            Some(AfterEnd::MoveTo(to)) => {
                self.move_item(loaded, principal, item, to, because, Some(session))
                    .await
            }
            Some(AfterEnd::MergeThen { done }) => {
                let EndReason::Submitted {
                    commit: Some(commit),
                } = reason
                else {
                    unreachable!("only submitted work with commits is merged");
                };
                self.merge_item(
                    workforce,
                    loaded,
                    MergeRequest {
                        item,
                        commit,
                        done,
                        because,
                        attempt: Attempt::First { session },
                    },
                )
                .await
            }
        };
        self.author(
            loaded,
            actor,
            EventBody::SessionEnded(SessionEnded { session, reason }),
        )
        .await?;
        let how = match finish {
            Finish::Submit { .. } | Finish::Release { .. } | Finish::Reviewed => Stop::AfterTurn,
            Finish::Stop
            | Finish::Blocked
            | Finish::TimedOut
            | Finish::Failed
            | Finish::GaveUp { .. } => Stop::Now,
        };
        self.runners.stop(session, how);
        if let SessionRole::Review { commit } = role {
            self.discard_review_worktree(workforce, item, commit).await;
        }
        if matches!(reason, EndReason::Submitted { commit: None }) {
            self.discard_branch(workforce, item).await;
        }
        effect?;
        Ok(())
    }

    /// Removes the checkout a review session looked at.
    async fn discard_review_worktree(
        &self,
        workforce: WorkforceId,
        item: ItemId,
        commit: Option<CommitId>,
    ) {
        let Some(repository) = self.repository_of(workforce) else {
            return;
        };
        let worktree = self.review_worktree(workforce, item, commit);
        let removed = blocking(move || match commit {
            Some(_) => GitCli::open(&repository)?.remove_worktree(&worktree),
            None => Ok(std::fs::remove_dir_all(&worktree)?),
        })
        .await;
        if let Ok(Err(error)) = removed {
            eprintln!(
                "could not remove the checkout of the review of #{}: {}",
                item.short(),
                wf_core::report::report(&error)
            );
        }
    }

    /// Removes the worktree and the branch of an item whose work left nothing to keep.
    async fn discard_branch(&self, workforce: WorkforceId, item: ItemId) {
        let Some(repository) = self.repository_of(workforce) else {
            return;
        };
        let branch = item_branch(item);
        let worktree = self
            .workforce_directory(workforce)
            .join("worktrees")
            .join(item_name(item));
        let removed = blocking(move || {
            let git = GitCli::open(&repository)?;
            git.remove_worktree(&worktree)?;
            git.delete_branch(&branch)
        })
        .await;
        match removed {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                eprintln!(
                    "could not remove the branch of #{}: {}",
                    item.short(),
                    wf_core::report::report(&error)
                );
            }
            Err(error) => {
                eprintln!(
                    "could not remove the branch of #{}: {}",
                    item.short(),
                    wf_core::report::report(&error)
                );
            }
        }
    }

    /// The head of the branch of `item`, if it has commits of its own.
    async fn submitted_commit(
        &self,
        workforce: WorkforceId,
        state: &State,
        item: ItemId,
    ) -> Result<Option<CommitId>, SessionError> {
        let Some(repository) = self.repository_of(workforce) else {
            return Ok(None);
        };
        let target = state
            .policy()
            .expect("a workforce has a policy")
            .policy
            .merge
            .target
            .clone();
        let branch = item_branch(item);
        let commit = blocking(move || {
            let git = GitCli::open(&repository)?;
            let head = git.branch_head(&branch)?;
            Ok::<_, VcsError>((git.commits_ahead(&target, head)? > 0).then_some(head))
        })
        .await??;
        Ok(commit)
    }

    /// Merges reviewed work into the target branch and finishes the item. What cannot be merged
    /// now leaves the item where a person or a later session will see it.
    pub(crate) async fn merge_item(
        &self,
        workforce: WorkforceId,
        loaded: &Loaded,
        request: MergeRequest,
    ) -> Result<(), SessionError> {
        let MergeRequest {
            item,
            commit,
            done,
            because,
            attempt,
        } = request;
        let Some(_merging) = self.merging.start(item) else {
            return Ok(());
        };
        let state = self.snapshot(loaded).await?;
        if matches!(
            state.review_status(item),
            Some(wf_core::review::ReviewStatus::Merged { .. })
        ) {
            return Ok(());
        }
        let principal = self.principal_of_device(&state)?;
        let Some(repository) = self.repository_of(workforce) else {
            return Ok(());
        };
        let ItemView::Live(live) = state.item(item).ok_or(SessionError::UnknownItem(item))? else {
            return Err(SessionError::Archived(item));
        };
        let policy = &state.policy().expect("a workforce has a policy").policy;
        let target = policy.merge.target.clone();
        let message = format!("Merge #{}: {}", HashId::short(&item), live.title());
        let open = policy
            .type_config(live.item_type())
            .and_then(|config| config.first_state(wf_core::policy::StateCategory::Open))
            .cloned();
        let blocked = policy
            .type_config(live.item_type())
            .and_then(|config| config.first_state(wf_core::policy::StateCategory::Blocked))
            .cloned();
        let comment = |body: String| {
            self.author(
                loaded,
                Actor::Human(principal),
                EventBody::Comment(Comment::plain(item, body)),
            )
        };

        let mut outcome = MergeOutcome::TargetMoved;
        for _ in 0..MERGE_ATTEMPTS {
            let (repository, target, message) =
                (repository.clone(), target.clone(), message.clone());
            outcome = blocking(move || GitCli::open(&repository)?.merge(&target, commit, &message))
                .await??;
            if outcome != MergeOutcome::TargetMoved {
                break;
            }
        }
        match outcome {
            MergeOutcome::Merged { commit: merged } => {
                self.author(
                    loaded,
                    Actor::Human(principal),
                    EventBody::Merged(Merged {
                        item,
                        reviewed: commit,
                        commit: merged,
                    }),
                )
                .await?;
                let (branch, worktree) = (
                    item_branch(item),
                    self.workforce_directory(workforce)
                        .join("worktrees")
                        .join(item_name(item)),
                );
                let cleaned = blocking(move || {
                    let git = GitCli::open(&repository)?;
                    git.remove_worktree(&worktree)?;
                    git.delete_branch(&branch)
                })
                .await?;
                if let Err(error) = cleaned {
                    eprintln!(
                        "could not remove the branch of #{}: {}",
                        item.short(),
                        wf_core::report::report(&error)
                    );
                }
                self.move_item(loaded, principal, item, done, because, attempt.by_session())
                    .await?;
            }
            MergeOutcome::Conflict { paths } => {
                comment(format!(
                    "Merging into {target} conflicts in {}. The next session merges {target} into the branch first.",
                    paths.join(", ")
                ))
                .await?;
                if let Some(open) = open {
                    self.move_item(loaded, principal, item, open, None, attempt.by_session())
                        .await?;
                }
            }
            MergeOutcome::DirtyCheckout { .. } | MergeOutcome::TargetMoved
                if attempt == Attempt::Retry => {}
            MergeOutcome::DirtyCheckout { worktree } => {
                comment(format!(
                    "Waiting to merge: {target} is checked out in {} with changes.",
                    worktree.display()
                ))
                .await?;
                if let Some(blocked) = blocked {
                    self.move_item(loaded, principal, item, blocked, None, attempt.by_session())
                        .await?;
                }
            }
            MergeOutcome::TargetMoved => {
                comment(format!("Waiting to merge: {target} kept moving.")).await?;
                if let Some(blocked) = blocked {
                    self.move_item(loaded, principal, item, blocked, None, attempt.by_session())
                        .await?;
                }
            }
        }
        Ok(())
    }

    /// Ends the sessions of this machine that the previous daemon left live: nothing runs them.
    pub(crate) async fn end_stale_sessions(
        self: &Arc<Self>,
        workforce: WorkforceId,
        loaded: &Arc<Loaded>,
    ) {
        let Ok(state) = self.snapshot(loaded).await else {
            return;
        };
        let stale: Vec<SessionId> = state
            .live_sessions()
            .filter(|session| session.device() == self.device())
            .filter(|session| !self.runners.is_running(session.id()))
            .map(|session| session.id())
            .collect();
        for session in stale {
            if let Err(error) = self
                .finish_session(workforce, loaded, session, Finish::Failed)
                .await
            {
                eprintln!(
                    "could not end stale session {session}: {}",
                    wf_core::report::report(&error)
                );
            }
        }
    }
}

async fn blocking<T: Send + 'static>(
    job: impl FnOnce() -> T + Send + 'static,
) -> Result<T, SessionError> {
    tokio::task::spawn_blocking(job)
        .await
        .map_err(|error| SessionError::Io {
            path: PathBuf::new(),
            source: std::io::Error::other(error),
        })
}

/// The task of one session: runs the harness, and ends the session if nobody else does.
async fn run_session(
    daemon: Arc<Daemon>,
    loaded: Arc<Loaded>,
    run: Run,
    mut stopped: mpsc::UnboundedReceiver<Stop>,
) {
    let end = drive(&daemon, &loaded, &run, &mut stopped).await;
    if let End::Finish(finish) = end
        && let Err(error) = daemon
            .finish_session(run.workforce, &loaded, run.session, finish)
            .await
    {
        eprintln!(
            "could not end session {}: {}",
            run.session,
            wf_core::report::report(&error)
        );
    }
    daemon.runners.stop(run.session, Stop::Now);
    daemon.held.forget(run.session);
}

async fn drive(
    daemon: &Arc<Daemon>,
    loaded: &Arc<Loaded>,
    run: &Run,
    stopped: &mut mpsc::UnboundedReceiver<Stop>,
) -> End {
    let Ok(transcript) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&run.transcript)
    else {
        return End::Finish(Finish::Failed);
    };
    let mut path =
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect::<Vec<_>>();
    if let Some(directory) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
    {
        path.insert(0, directory);
    }
    let path = std::env::join_paths(path).unwrap_or_default();
    let agent = run
        .harness
        .clone()
        .env("WF_SESSION", run.session.to_string())
        .env("WF_WORKFORCE", run.workforce.to_string())
        .env("PATH", path.to_string_lossy());
    let mut conversation = match Conversation::start(Launch {
        agent,
        directory: run.worktree.clone(),
        permissions: Arc::clone(&run.permissions),
        transcript,
    })
    .await
    {
        Ok(conversation) => conversation,
        Err(error) => {
            eprintln!(
                "could not start {} for session {}: {}",
                run.agent,
                run.session,
                wf_core::report::report(&error)
            );
            return End::Finish(Finish::Failed);
        }
    };
    let end = turns(daemon, loaded, run, stopped, &mut conversation).await;
    let _ = conversation.close().await;
    end
}

async fn turns(
    daemon: &Arc<Daemon>,
    loaded: &Arc<Loaded>,
    run: &Run,
    stopped: &mut mpsc::UnboundedReceiver<Stop>,
    conversation: &mut Conversation,
) -> End {
    let Ok(state) = daemon.snapshot(loaded).await else {
        return End::Finish(Finish::Failed);
    };
    let mut prompt = match run.role {
        SessionRole::Work => work_package(daemon, loaded, run, &state).await,
        SessionRole::Review { commit } => review_package(loaded, run, &state, commit).await,
    };
    let cancel = conversation.cancel();
    let deadline = run.max_duration.map(|duration| Instant::now() + duration);
    let mut last_head = branch_head(run).await;
    let mut turns_without_commit = 0;
    loop {
        let turn = conversation.prompt(prompt);
        tokio::pin!(turn);
        let result = tokio::select! {
            result = &mut turn => result,
            Some(how) = stopped.recv() => {
                if matches!(how, Stop::AfterTurn)
                    && tokio::time::timeout(GRACE, &mut turn).await.is_ok()
                {
                    return End::Over;
                }
                cancel.cancel();
                let _ = (&mut turn).await;
                return End::Over;
            }
            () = sleep_until(deadline) => {
                cancel.cancel();
                let _ = (&mut turn).await;
                return End::Finish(Finish::TimedOut);
            }
        };
        let Ok(_stop_reason) = result else {
            return End::Finish(Finish::Failed);
        };

        let head = branch_head(run).await;
        if head != last_head {
            turns_without_commit = 0;
            last_head = head;
        } else {
            turns_without_commit += 1;
        }
        let Ok(state) = daemon.snapshot(loaded).await else {
            return End::Finish(Finish::Failed);
        };
        match state.turn_outcome(run.session, turns_without_commit, run.nudges) {
            TurnOutcome::Close => {
                let live = state
                    .session(run.session)
                    .is_some_and(|session| session.is_live());
                return if live {
                    End::Finish(Finish::Blocked)
                } else {
                    End::Over
                };
            }
            TurnOutcome::Continue => prompt = wf_harness::CONTINUE.to_string(),
            TurnOutcome::GiveUp => {
                return End::Finish(Finish::GaveUp {
                    turns: turns_without_commit,
                });
            }
            TurnOutcome::Park => match park(daemon, loaded, run, stopped, &deadline).await {
                Parked::Ready => {
                    let state = daemon.snapshot(loaded).await.unwrap_or(state);
                    prompt = resumed(daemon, loaded, run, &state).await;
                    turns_without_commit = 0;
                }
                Parked::Over => return End::Over,
                Parked::TimedOut => return End::Finish(Finish::TimedOut),
                Parked::Blocked => return End::Finish(Finish::Blocked),
            },
        }
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

async fn branch_head(run: &Run) -> Option<CommitId> {
    let (repository, branch) = (run.repository.clone(), run.branch.clone());
    blocking(move || GitCli::open(&repository)?.branch_head(&branch))
        .await
        .ok()?
        .ok()
}

enum Parked {
    Ready,
    Over,
    TimedOut,
    Blocked,
}

/// Waits until the item is ready again, keeping the harness and its conversation.
async fn park(
    daemon: &Arc<Daemon>,
    loaded: &Arc<Loaded>,
    run: &Run,
    stopped: &mut mpsc::UnboundedReceiver<Stop>,
    deadline: &Option<Instant>,
) -> Parked {
    let mut heads = loaded.storage.watch_heads();
    let wake = Instant::now() + run.park_timeout;
    loop {
        let Ok(state) = daemon.snapshot(loaded).await else {
            return Parked::Over;
        };
        match state.turn_outcome(run.session, 0, run.nudges) {
            TurnOutcome::Park => {}
            TurnOutcome::Close => {
                let live = state
                    .session(run.session)
                    .is_some_and(|session| session.is_live());
                return if live { Parked::Blocked } else { Parked::Over };
            }
            TurnOutcome::Continue | TurnOutcome::GiveUp => return Parked::Ready,
        }
        tokio::select! {
            changed = heads.changed() => {
                if changed.is_err() {
                    return Parked::Over;
                }
            }
            Some(_) = stopped.recv() => return Parked::Over,
            () = tokio::time::sleep_until(wake) => return Parked::TimedOut,
            () = sleep_until(*deadline) => return Parked::TimedOut,
        }
    }
}

async fn work_package(
    daemon: &Arc<Daemon>,
    loaded: &Arc<Loaded>,
    run: &Run,
    state: &State,
) -> String {
    let Some(ItemView::Live(item)) = state.item(run.item) else {
        return String::new();
    };
    let body = item.body().render_numbered();
    let earlier = earlier_comments(loaded, item).await;
    let graph = state.dependency_graph();
    let policy = &state.policy().expect("a workforce has a policy").policy;
    let mut dependencies = Vec::new();
    for dependency in graph.depends_on(run.item) {
        let Some(ItemView::Live(dependency)) = state.item(dependency) else {
            continue;
        };
        let note = last_comment(loaded, dependency).await;
        dependencies.push(wf_harness::Dependency {
            item: HashId::short(&dependency.id()),
            title: dependency.title().to_string(),
            state: match dependency.standing(policy) {
                Standing::Known { state, .. } => state.to_string(),
                Standing::Unknown { state } => {
                    state.map_or_else(|| "unknown".to_string(), ToString::to_string)
                }
            },
            note,
        });
    }
    let _ = daemon;
    WorkPackage {
        item: HashId::short(&run.item),
        title: item.title().to_string(),
        body,
        branch: run.branch.clone(),
        target: run.target.clone(),
        dependencies,
        earlier,
    }
    .render()
}

async fn review_package(
    loaded: &Arc<Loaded>,
    run: &Run,
    state: &State,
    commit: Option<CommitId>,
) -> String {
    let Some(ItemView::Live(item)) = state.item(run.item) else {
        return String::new();
    };
    let step = match state.review_status(run.item) {
        Some(wf_core::review::ReviewStatus::Awaiting {
            step: wf_core::policy::ReviewStep::Agent { requires },
        }) => format!(
            "the review by an agent with {}",
            requires
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Some(
            wf_core::review::ReviewStatus::Awaiting {
                step: wf_core::policy::ReviewStep::Human,
            }
            | wf_core::review::ReviewStatus::ChangesRequested
            | wf_core::review::ReviewStatus::Approved { .. }
            | wf_core::review::ReviewStatus::Merged { .. },
        )
        | None => "a review".to_string(),
    };
    let (repository, target) = (run.repository.clone(), run.target.clone());
    let diff = match commit {
        Some(commit) => blocking(move || GitCli::open(&repository)?.diff(&target, commit))
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default(),
        None => String::new(),
    };
    ReviewPackage {
        item: HashId::short(&run.item),
        title: item.title().to_string(),
        body: item.body().render_numbered(),
        commit: commit.map(|commit| commit.to_string()),
        target: run.target.clone(),
        step,
        earlier: earlier_comments(loaded, item).await,
        diff,
    }
    .render()
}

/// What to say when a parked session can go on.
async fn resumed(_daemon: &Arc<Daemon>, loaded: &Arc<Loaded>, run: &Run, state: &State) -> String {
    let mut text = String::from("What you were waiting for is done.");
    let graph = state.dependency_graph();
    for dependency in graph.depends_on(run.item) {
        let Some(ItemView::Live(dependency)) = state.item(dependency) else {
            continue;
        };
        if let Some(note) = last_comment(loaded, dependency).await {
            text.push_str(&format!(
                "\n- #{} {}: {note}",
                HashId::short(&dependency.id()),
                dependency.title()
            ));
        }
    }
    text.push_str("\nContinue.");
    text
}

async fn earlier_comments(loaded: &Loaded, item: &wf_core::item::Item) -> Vec<wf_harness::Earlier> {
    let mut comments: Vec<_> = item.comments().iter().collect();
    comments.sort_by_key(|(id, comment)| (comment.at, **id));
    let ids: Vec<EventId> = comments.iter().map(|(id, _)| EventId::from(**id)).collect();
    let events = loaded
        .storage
        .with(move |replica| {
            ids.iter()
                .map(|id| {
                    wf_store::EventStore::event(replica.store(), id)
                        .ok()
                        .flatten()
                })
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();
    comments
        .into_iter()
        .zip(events)
        .filter_map(|((_, comment), event)| {
            let EventBody::Comment(written) = &event?.content.body else {
                return None;
            };
            Some(wf_harness::Earlier {
                by: match &comment.by {
                    Actor::Human(_) => "someone".to_string(),
                    Actor::Agent { agent, .. } => agent.name.to_string(),
                },
                kind: comment.kind.name().replace('_', " "),
                text: written.text.clone(),
            })
        })
        .collect()
}

/// The latest plain comment on an item: the answer, for a question.
async fn last_comment(loaded: &Loaded, item: &wf_core::item::Item) -> Option<String> {
    let mut comments: Vec<_> = item
        .comments()
        .iter()
        .filter(|(_, comment)| comment.kind == wf_core::event::CommentKind::Plain)
        .collect();
    comments.sort_by_key(|(id, comment)| (comment.at, **id));
    let (id, _) = comments.last()?;
    let id = EventId::from(**id);
    let event = loaded
        .storage
        .with(move |replica| {
            wf_store::EventStore::event(replica.store(), &id)
                .ok()
                .flatten()
        })
        .await
        .ok()??;
    match event.content.body {
        EventBody::Comment(comment) => Some(comment.text),
        _ => None,
    }
}

/// How often the dispatcher looks for work even when nothing changed.
const DISPATCH_TICK: Duration = Duration::from_secs(30);

/// Starts agents on the ready items of a workforce for as long as the daemon runs and the
/// storage of the workforce does: whenever the log changes, and on a tick.
pub(crate) async fn run_dispatcher(
    daemon: std::sync::Weak<Daemon>,
    workforce: WorkforceId,
    loaded: Arc<Loaded>,
) {
    let mut heads = loaded.storage.watch_heads();
    let mut failed: std::collections::BTreeSet<(ItemId, AgentName)> = Default::default();
    loop {
        let Some(strong) = daemon.upgrade() else {
            return;
        };
        dispatch_once(&strong, workforce, &loaded, &mut failed).await;
        drop(strong);
        tokio::select! {
            changed = heads.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            () = tokio::time::sleep(DISPATCH_TICK) => {}
        }
    }
}

/// Merges what is ready, then gives every hosted auto agent with a free slot a review first and
/// otherwise the first item it may take. Whatever could not be started is not tried again by
/// this daemon.
async fn dispatch_once(
    daemon: &Arc<Daemon>,
    workforce: WorkforceId,
    loaded: &Arc<Loaded>,
    failed: &mut std::collections::BTreeSet<(ItemId, AgentName)>,
) {
    let Some(repository) = daemon.repository_of(workforce) else {
        return;
    };
    let device = daemon.device();
    let Ok(mut state) = daemon.snapshot(loaded).await else {
        return;
    };
    for (item, commit) in state.pending_merges() {
        let Some(done) = state.item(item).and_then(|view| match view {
            ItemView::Live(live) => state
                .policy()?
                .policy
                .type_config(live.item_type())?
                .first_state(wf_core::policy::StateCategory::Done)
                .cloned(),
            ItemView::Archived(_) => None,
        }) else {
            continue;
        };
        if let Err(error) = daemon
            .merge_item(
                workforce,
                loaded,
                MergeRequest {
                    item,
                    commit,
                    done,
                    because: None,
                    attempt: Attempt::Retry,
                },
            )
            .await
        {
            eprintln!(
                "could not merge #{}: {}",
                item.short(),
                wf_core::report::report(&error)
            );
        }
    }
    for item in state.pending_answers() {
        let done = state.item(item).and_then(|view| match view {
            ItemView::Live(live) => state
                .policy()?
                .policy
                .type_config(live.item_type())?
                .first_state(wf_core::policy::StateCategory::Done)
                .cloned(),
            ItemView::Archived(_) => None,
        });
        let (Some(done), Ok(principal)) = (done, daemon.principal_of_device(&state)) else {
            continue;
        };
        if let Err(error) = daemon
            .move_item(loaded, principal, item, done, None, None)
            .await
        {
            eprintln!(
                "could not finish #{}: {}",
                item.short(),
                wf_core::report::report(&error)
            );
        }
    }
    let Ok(fresh) = daemon.snapshot(loaded).await else {
        return;
    };
    state = fresh;

    let agents: Vec<_> = state
        .auto_agents(device)
        .into_iter()
        .map(|(agent, _)| agent)
        .collect();
    for agent in agents {
        while state.has_free_slot(device, &agent) {
            let review = state
                .reviewable(&agent)
                .into_iter()
                .find(|(item, _)| !failed.contains(&(*item, agent.name.clone())));
            let started = match review {
                Some((item, commit)) => (
                    item,
                    daemon
                        .start_review(
                            workforce,
                            loaded,
                            agent.name.clone(),
                            item,
                            commit,
                            repository.clone(),
                        )
                        .await,
                ),
                None => {
                    let Some(item) = state
                        .dispatchable(device, &agent)
                        .into_iter()
                        .map(|item| item.id())
                        .find(|item| !failed.contains(&(*item, agent.name.clone())))
                    else {
                        break;
                    };
                    (
                        item,
                        daemon
                            .start_session(
                                workforce,
                                loaded,
                                agent.name.clone(),
                                item,
                                repository.clone(),
                            )
                            .await,
                    )
                }
            };
            if let (item, Err(error)) = started {
                failed.insert((item, agent.name.clone()));
                eprintln!(
                    "could not start {} on #{}: {}",
                    agent.name,
                    item.short(),
                    wf_core::report::report(&error)
                );
            }
            let Ok(fresh) = daemon.snapshot(loaded).await else {
                return;
            };
            state = fresh;
        }
    }
}

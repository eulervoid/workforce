//! Running sessions: the daemon starts a harness for a work session, prompts it turn after turn
//! until the session ends, and applies what follows from the end to the item.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Instant;
use wf_core::event::{
    Comment, Event, EventBody, ItemTransitioned, Merged, SessionEnded, SessionStarted,
};
use wf_core::id::{HashId, ItemId, SessionId, WorkforceId};
use wf_core::identity::Actor;
use wf_core::item::{ItemView, Standing};
use wf_core::lifecycle::{AfterEnd, TurnOutcome};
use wf_core::name::{AgentName, HarnessName, StateName};
use wf_core::session::{CommitId, EndReason, SessionRole};
use wf_core::state::State;
use wf_harness::{AcpAgentConfig, Conversation, Launch, PermissionMode, WorkPackage};
use wf_store::AuthorError;
use wf_vcs::{GitCli, MergeOutcome, VcsAdapter, VcsError, item_branch, item_name};

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
struct Stop;

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
    fn stop(&self, session: SessionId) {
        if let Some(stop) = self.lock().remove(&session) {
            let _ = stop.send(Stop);
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

/// What a session is, to run it.
struct Run {
    workforce: WorkforceId,
    session: SessionId,
    item: ItemId,
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
    permissions: PermissionMode,
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
        let text =
            std::fs::read_to_string(self.workforce_directory(workforce).join("repository")).ok()?;
        Some(PathBuf::from(text.trim()))
    }

    fn remember_repository(
        &self,
        workforce: WorkforceId,
        repository: &Path,
    ) -> Result<(), SessionError> {
        let path = self.workforce_directory(workforce).join("repository");
        std::fs::write(&path, repository.to_string_lossy().as_bytes())
            .map_err(|source| SessionError::Io { path, source })
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
        let target = state
            .policy()
            .expect("a workforce has a policy")
            .policy
            .merge
            .target
            .clone();
        let branch = item_branch(item);
        let worktree = self
            .workforce_directory(workforce)
            .join("worktrees")
            .join(item_name(item));
        {
            let (repository, branch, worktree, target) = (
                repository.clone(),
                branch.clone(),
                worktree.clone(),
                target.clone(),
            );
            blocking(move || {
                GitCli::open(&repository)?.prepare_worktree(&branch, &worktree, &target)
            })
            .await??;
        }
        self.remember_repository(workforce, &repository)?;

        let started = self
            .author(
                loaded,
                Actor::Human(principal),
                EventBody::SessionStarted(SessionStarted {
                    agent: agent.clone(),
                    item,
                    role: SessionRole::Work,
                }),
            )
            .await?;
        let session = SessionId::from(started.id());
        let state = self.snapshot(loaded).await?;
        if let Some((item, to)) = state.start_move(session) {
            self.move_item(loaded, principal, item, to, None).await?;
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
            // Held requests come with the inbox; until then `ask` refuses what it cannot ask.
            permissions: match autonomy.permissions {
                wf_core::autonomy::PermissionMode::Allow => PermissionMode::Allow,
                wf_core::autonomy::PermissionMode::Ask
                | wf_core::autonomy::PermissionMode::Deny => PermissionMode::Deny,
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
        note: Option<String>,
    ) -> Result<(), SessionError> {
        self.author(
            loaded,
            Actor::Human(principal),
            EventBody::ItemTransitioned(ItemTransitioned { item, to, note }),
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
            self.runners.stop(session);
            return Ok(());
        }
        let principal = self.principal_of_device(&state)?;
        let item = folded.item();
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
        // The comment comes first: once the session has ended it can no longer write.
        if let (EndReason::Released, Some(note)) = (reason, &note) {
            self.author(
                loaded,
                actor.clone(),
                EventBody::Comment(Comment {
                    item,
                    body: note.clone(),
                }),
            )
            .await?;
        }
        self.author(
            loaded,
            actor,
            EventBody::SessionEnded(SessionEnded { session, reason }),
        )
        .await?;
        self.runners.stop(session);

        let state = self.snapshot(loaded).await?;
        let transition_note = match reason {
            EndReason::Submitted { .. } => note,
            _ => None,
        };
        match state.after_end(session) {
            None | Some(AfterEnd::Nothing) => {}
            Some(AfterEnd::MoveTo(to)) => {
                self.move_item(loaded, principal, item, to, transition_note)
                    .await?;
            }
            Some(AfterEnd::MergeThen { done }) => {
                let EndReason::Submitted {
                    commit: Some(commit),
                } = reason
                else {
                    unreachable!("only submitted work with commits is merged");
                };
                self.merge_item(workforce, loaded, item, commit, done, transition_note)
                    .await?;
            }
        }
        Ok(())
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
        item: ItemId,
        commit: CommitId,
        done: StateName,
        note: Option<String>,
    ) -> Result<(), SessionError> {
        let state = self.snapshot(loaded).await?;
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
                EventBody::Comment(Comment { item, body }),
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
                self.move_item(loaded, principal, item, done, note).await?;
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
                    eprintln!("could not remove the branch of #{}: {error}", item.short());
                }
            }
            MergeOutcome::Conflict { paths } => {
                comment(format!(
                    "Merging into {target} conflicts in {}. The next session merges {target} into the branch first.",
                    paths.join(", ")
                ))
                .await?;
                if let Some(open) = open {
                    self.move_item(loaded, principal, item, open, None).await?;
                }
            }
            MergeOutcome::DirtyCheckout { worktree } => {
                comment(format!(
                    "Waiting to merge: {target} is checked out in {} with changes.",
                    worktree.display()
                ))
                .await?;
                if let Some(blocked) = blocked {
                    self.move_item(loaded, principal, item, blocked, None)
                        .await?;
                }
            }
            MergeOutcome::TargetMoved => {
                comment(format!("Waiting to merge: {target} kept moving.")).await?;
                if let Some(blocked) = blocked {
                    self.move_item(loaded, principal, item, blocked, None)
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
    daemon.runners.stop(run.session);
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
        .env("PATH", path.to_string_lossy());
    let mut conversation = match Conversation::start(Launch {
        agent,
        directory: run.worktree.clone(),
        permissions: run.permissions,
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
    let mut prompt = work_package(daemon, loaded, run, &state).await;
    let cancel = conversation.cancel();
    let deadline = run.max_duration.map(|duration| Instant::now() + duration);
    let mut last_head = branch_head(run).await;
    let mut turns_without_commit = 0;
    loop {
        let turn = conversation.prompt(prompt);
        tokio::pin!(turn);
        let result = tokio::select! {
            result = &mut turn => result,
            Some(Stop) = stopped.recv() => {
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
            Some(Stop) = stopped.recv() => return Parked::Over,
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
    let earlier = earlier_notes(loaded, item).await;
    let graph = state.dependency_graph();
    let policy = &state.policy().expect("a workforce has a policy").policy;
    let mut dependencies = Vec::new();
    for dependency in graph.depends_on(run.item) {
        let Some(ItemView::Live(dependency)) = state.item(dependency) else {
            continue;
        };
        let note = last_transition_note(loaded, dependency).await;
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

/// What to say when a parked session can go on.
async fn resumed(_daemon: &Arc<Daemon>, loaded: &Arc<Loaded>, run: &Run, state: &State) -> String {
    let mut text = String::from("What you were waiting for is done.");
    let graph = state.dependency_graph();
    for dependency in graph.depends_on(run.item) {
        let Some(ItemView::Live(dependency)) = state.item(dependency) else {
            continue;
        };
        if let Some(note) = last_transition_note(loaded, dependency).await {
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

async fn earlier_notes(loaded: &Loaded, item: &wf_core::item::Item) -> Vec<wf_harness::Earlier> {
    let mut notes: Vec<_> = item.notes().iter().collect();
    notes.sort_by_key(|(id, note)| (note.at, **id));
    let ids: Vec<_> = notes.iter().map(|(id, _)| **id).collect();
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
    notes
        .into_iter()
        .zip(events)
        .filter_map(|((_, note), event)| {
            let text = match &event?.content.body {
                EventBody::Comment(comment) => comment.body.clone(),
                EventBody::Report(report) => report.note.clone(),
                EventBody::ItemTransitioned(transition) => transition.note.clone()?,
                _ => return None,
            };
            Some(wf_harness::Earlier {
                by: match &note.by {
                    Actor::Human(_) => "someone".to_string(),
                    Actor::Agent { agent, .. } => agent.name.to_string(),
                },
                kind: note.kind.name().to_string(),
                text,
            })
        })
        .collect()
}

async fn last_transition_note(loaded: &Loaded, item: &wf_core::item::Item) -> Option<String> {
    let mut notes: Vec<_> = item
        .notes()
        .iter()
        .filter(|(_, note)| note.kind == wf_core::item::NoteKind::Transition)
        .collect();
    notes.sort_by_key(|(id, note)| (note.at, **id));
    let (id, _) = notes.last()?;
    let id = **id;
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
        EventBody::ItemTransitioned(transition) => transition.note,
        _ => None,
    }
}

//! Starting sessions: the worktree or directory a session runs in, its harness and sandbox.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;
use wf_core::event::{EventBody, SessionStarted};
use wf_core::id::{HashId, ItemId, SessionId, WorkforceId};
use wf_core::identity::Actor;
use wf_core::item::ItemView;
use wf_core::lifecycle::{Interruption, SessionEnd};
use wf_core::name::AgentName;
use wf_core::session::{CommitId, SessionRole};
use wf_harness::{AcpAgentConfig, Permission, PermissionRequest, Permissions};
use wf_vcs::{GitCli, VcsAdapter, VcsError, item_branch, item_name, item_worktree, worktrees};

use super::drive::run_session;
use super::{Run, SessionError, blocking};
use crate::decide::{AgentContext, DeciderRequest, Deciding, ItemContext};
use crate::machine_config::SandboxConfig;
use crate::{Daemon, Loaded};

/// The agent as the machine config says to run it: in a sandbox, or as it is. A config that is
/// broken stops the session, so that an agent never runs unconfined by mistake.
pub(super) fn sandboxed(
    daemon: &Daemon,
    run: &Run,
    agent: AcpAgentConfig,
) -> Result<AcpAgentConfig, String> {
    let config = match &run.sandbox {
        SandboxConfig::Fence(config) => config,
        SandboxConfig::Off {} => return Ok(agent),
    };
    let access = crate::sandbox::Access {
        worktree: &run.worktree,
        repository: &run.repository,
        socket: &daemon.config.socket,
    };
    let settings = daemon
        .workforce_directory(run.workforce)
        .join("sandbox")
        .join(format!("{}.json", run.session));
    crate::sandbox::wrap(config, &access, &agent, &settings).map_err(|error| error.to_string())
}

/// Whether `program` is a file in one of the directories of `PATH`.
fn on_path(program: &str) -> bool {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .any(|directory| directory.join(program).is_file())
}

impl Daemon {
    /// The empty directory a supervise or diagnose session runs in. One session of each runs per
    /// item.
    pub(super) fn empty_directory(
        &self,
        workforce: WorkforceId,
        item: ItemId,
        purpose: &str,
    ) -> PathBuf {
        worktrees(&self.workforce_directory(workforce))
            .join(format!("{}-{purpose}", item_name(item)))
    }

    /// Where a review session looks at a commit of an item.
    pub(super) fn review_worktree(
        &self,
        workforce: WorkforceId,
        item: ItemId,
        commit: Option<CommitId>,
    ) -> PathBuf {
        let suffix = commit.map_or("answer".to_string(), |commit| {
            commit.to_string()[..8].into()
        });
        worktrees(&self.workforce_directory(workforce))
            .join(format!("{}-{suffix}", item_name(item)))
    }

    /// Starts a session of `agent` on `item`, and a task that runs the harness. A work session
    /// runs in the item's worktree on its branch, and moves the item to its active state; a
    /// review session in a detached checkout of the commit under review, or an empty directory
    /// when it was submitted without commits; a supervise or diagnose session in an empty
    /// directory.
    pub(crate) async fn start_session(
        self: &Arc<Self>,
        workforce: WorkforceId,
        loaded: &Arc<Loaded>,
        agent: AgentName,
        item: ItemId,
        role: SessionRole,
        repository: PathBuf,
    ) -> Result<SessionId, SessionError> {
        // A start by hand and one by the dispatcher must not both see the item free: the state is
        // read and the claim checked under a lock held until `SessionStarted` is written.
        let _starting = match role {
            SessionRole::Work => Some(
                self.starting
                    .take(item)
                    .ok_or(SessionError::Claimed(item))?,
            ),
            SessionRole::Review { .. } | SessionRole::Supervise | SessionRole::Diagnose => None,
        };
        let state = self.snapshot(loaded).await?;
        let principal = self.principal_of_device(&state)?;
        let definition = state
            .agent(&agent, principal)
            .ok_or_else(|| SessionError::UnknownAgent(agent.clone()))?
            .clone();
        let harness = self
            .harness_command(&definition.harness)
            .ok_or_else(|| SessionError::UnknownHarness(definition.harness.clone()))?;
        let sandbox = self.machine_config()?.sandbox;
        match &sandbox {
            SandboxConfig::Fence(_) if !on_path("fence") => return Err(SessionError::NoFence),
            SandboxConfig::Fence(_) | SandboxConfig::Off {} => {}
        }
        let item_context = match state.item(item).ok_or(SessionError::UnknownItem(item))? {
            ItemView::Live(live) => {
                let policy = &state.policy().expect("a workforce has a policy").policy;
                let state_name = match live.standing(policy) {
                    wf_core::item::Standing::Known { state, .. } => state.to_string(),
                    wf_core::item::Standing::Unknown { state } => {
                        state.map(ToString::to_string).unwrap_or_default()
                    }
                };
                ItemContext {
                    id: item.to_base32(),
                    title: live.title().to_string(),
                    item_type: live.item_type().to_string(),
                    state: state_name,
                }
            }
            ItemView::Archived(_) => return Err(SessionError::Archived(item)),
        };
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
                let worktree = item_worktree(&self.workforce_directory(workforce), item);
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
            SessionRole::Supervise | SessionRole::Diagnose => {
                let purpose = match role {
                    SessionRole::Diagnose => "diagnose",
                    SessionRole::Work | SessionRole::Review { .. } | SessionRole::Supervise => {
                        "supervise"
                    }
                };
                let worktree = self.empty_directory(workforce, item, purpose);
                let path = worktree.clone();
                let repository = blocking(move || {
                    let git = GitCli::open(&repository)?;
                    let _ = std::fs::remove_dir_all(&path);
                    std::fs::create_dir_all(&path)?;
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
                Actor::Daemon(principal),
                EventBody::SessionStarted(SessionStarted {
                    agent: agent.clone(),
                    item,
                    role,
                }),
            )
            .await?;
        let session = SessionId::from(started.id());
        // Once the session is in the log, a failure to set it up ends it: otherwise it would hold
        // the item and a slot until the next daemon starts.
        let prepared = async {
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
            Ok::<_, SessionError>(transcripts)
        }
        .await;
        let transcripts = match prepared {
            Ok(transcripts) => transcripts,
            Err(error) => {
                let because = format!(
                    "the daemon could not set it up: {}",
                    wf_core::error_chain(&error)
                );
                if let Err(ending) = self
                    .finish_session(
                        workforce,
                        loaded,
                        session,
                        SessionEnd::Interruption(Interruption::Failed { because }),
                    )
                    .await
                {
                    tracing::warn!(
                        "could not end session {session} after it failed to start: {}",
                        wf_core::error_chain(&ending)
                    );
                }
                return Err(error);
            }
        };
        let autonomy = definition.autonomy;
        let permissions: Arc<dyn Permissions> = match sandbox.permissions(autonomy.permissions) {
            wf_core::autonomy::PermissionMode::Allow => Arc::new(Permission::Allow),
            wf_core::autonomy::PermissionMode::Deny => Arc::new(Permission::Deny),
            wf_core::autonomy::PermissionMode::Ask => Arc::new(Deciding {
                held: Arc::clone(&self.held),
                workforce,
                session,
                machine_config: self.config.machine_config.clone(),
                agents: crate::judges::WorkforceJudges {
                    daemon: Arc::downgrade(self),
                    workforce,
                },
                context: DeciderRequest {
                    hook: "permission".to_string(),
                    version: 1,
                    workforce: workforce.to_base32(),
                    item: item_context,
                    session: session.to_base32(),
                    agent: AgentContext {
                        name: agent.to_string(),
                        capabilities: definition
                            .capabilities
                            .iter()
                            .map(ToString::to_string)
                            .collect(),
                    },
                    directory: worktree.clone(),
                    tool: PermissionRequest {
                        kind: None,
                        title: String::new(),
                        input: None,
                    },
                    guidance: None,
                },
            }),
        };
        let run = Run {
            workforce,
            session,
            item,
            role,
            agent,
            harness,
            model: definition.model.clone(),
            worktree,
            transcript: transcripts.join(format!("{session}.jsonl")),
            branch,
            target,
            repository,
            nudges: autonomy.nudges,
            park_timeout: autonomy.park_timeout,
            max_duration: autonomy.max_duration,
            permissions,
            sandbox,
        };
        let isolation = match (&run.sandbox, autonomy.permissions) {
            (SandboxConfig::Fence(_), _) => "",
            (SandboxConfig::Off {}, wf_core::autonomy::PermissionMode::Allow) => {
                " without a sandbox, so what it may do is asked"
            }
            (
                SandboxConfig::Off {},
                wf_core::autonomy::PermissionMode::Ask | wf_core::autonomy::PermissionMode::Deny,
            ) => " without a sandbox",
        };
        tracing::info!(
            "session {}: {} started on item {}{isolation}",
            session.short(),
            run.agent,
            item.short()
        );
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
}

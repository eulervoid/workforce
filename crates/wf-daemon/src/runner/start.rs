//! Starting sessions: the worktree or directory a session runs in, its harness and sandbox.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;
use wf_core::event::{EventBody, SessionStarted};
use wf_core::id::{HashId, ItemId, SessionId, WorkforceId};
use wf_core::identity::{Actor, PrincipalKey};
use wf_core::item::ItemView;
use wf_core::lifecycle::{Interruption, SessionEnd};
use wf_core::name::AgentName;
use wf_core::session::{CommitId, SessionRole};
use wf_core::state::{AgentDefinition, State};
use wf_harness::{AcpAgentConfig, Permission, PermissionRequest, Permissions};
use wf_vcs::{GitCli, VcsAdapter, VcsError, item_branch, item_name, item_worktree, worktrees};

use super::drive::run_session;
use super::{Begin, Run, SessionError, blocking, ensure_unclaimed};
use crate::decide::{AgentContext, DeciderRequest, Deciding, ItemContext};
use crate::machine_config::{EnvironmentConfig, SandboxConfig};
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
        data_dir: &daemon.config.data_dir,
        machine_config: daemon.config.machine_config.as_deref(),
    };
    let settings = daemon
        .workforce_directory(run.workforce)
        .join("sandbox")
        .join(format!("{}.json", run.session));
    crate::sandbox::wrap(config, &access, &agent, &settings).map_err(|error| error.to_string())
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

    /// What it takes to run `agent` on `item` in `role`, as the state and the machine config say:
    /// checked before a session is written, so that a session that cannot run is not started.
    pub(super) fn setup(
        &self,
        state: &State,
        workforce: WorkforceId,
        agent: AgentName,
        item: ItemId,
        role: SessionRole,
    ) -> Result<Setup, SessionError> {
        let principal = self.principal_of_device(state)?;
        let handle = &state
            .member(&principal)
            .ok_or(SessionError::NotBound)?
            .handle;
        let git_identity = format!("@{handle}/{agent}");
        let definition = state
            .agent(&agent, principal)
            .ok_or_else(|| SessionError::UnknownAgent(agent.clone()))?
            .clone();
        let harness = self
            .harness_command(&definition.harness)
            .ok_or_else(|| SessionError::UnknownHarness(definition.harness.clone()))?;
        let machine_config = self.machine_config()?;
        let sandbox = machine_config.sandbox_of(&agent);
        let environment = machine_config.environment_of(&agent);
        match &sandbox {
            SandboxConfig::Fence(_) if !crate::sandbox::fence_installed() => {
                return Err(SessionError::NoFence);
            }
            SandboxConfig::Fence(_) | SandboxConfig::Off {} => {}
        }
        let policy = &state.policy().expect("a workforce has a policy").policy;
        let item_context = match state.item(item).ok_or(SessionError::UnknownItem(item))? {
            ItemView::Live(live) => ItemContext {
                id: item.to_base32(),
                title: live.title().to_string(),
                item_type: live.item_type().to_string(),
                state: live.state(policy).to_string(),
            },
            ItemView::Archived(_) => return Err(SessionError::Archived(item)),
        };
        let transcripts = self.workforce_directory(workforce).join("transcripts");
        Ok(Setup {
            workforce,
            principal,
            agent,
            item,
            role,
            definition,
            harness,
            sandbox,
            environment,
            git_identity,
            agent_signing: machine_config.commits.agent_signing.signing(),
            item_context,
            target: policy.merge.target.clone(),
            transcripts,
        })
    }

    /// The worktree or directory a session of `role` on `item` runs in.
    pub(super) fn session_directory(
        &self,
        workforce: WorkforceId,
        item: ItemId,
        role: SessionRole,
    ) -> PathBuf {
        match role {
            SessionRole::Work => item_worktree(&self.workforce_directory(workforce), item),
            SessionRole::Review { commit } => self.review_worktree(workforce, item, commit),
            SessionRole::Supervise => self.empty_directory(workforce, item, "supervise"),
            SessionRole::Diagnose => self.empty_directory(workforce, item, "diagnose"),
        }
    }

    /// Starts a session of `agent` on `item`, and a task that runs the harness. A work session
    /// runs in the item's worktree on its branch, and moves the item to `active`; a
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
                    .ok_or(SessionError::Starting(item))?,
            ),
            SessionRole::Review { .. } | SessionRole::Supervise | SessionRole::Diagnose => None,
        };
        let state = self.snapshot(loaded).await?;
        let setup = self.setup(&state, workforce, agent, item, role)?;
        if role == SessionRole::Work {
            ensure_unclaimed(&state, item)?;
        }
        let worktree = self.session_directory(workforce, item, role);
        let path = worktree.clone();
        let repository = match role {
            SessionRole::Work => {
                let (branch, target) = (item_branch(item), setup.target.clone());
                blocking(move || {
                    let git = GitCli::open(&repository)?;
                    git.prepare_worktree(&branch, &path, &target)?;
                    Ok::<_, VcsError>(git.repository().to_path_buf())
                })
                .await??
            }
            SessionRole::Review { commit } => {
                blocking(move || {
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
                .await??
            }
            SessionRole::Supervise | SessionRole::Diagnose => {
                blocking(move || {
                    let git = GitCli::open(&repository)?;
                    let _ = std::fs::remove_dir_all(&path);
                    std::fs::create_dir_all(&path)?;
                    Ok::<_, VcsError>(git.repository().to_path_buf())
                })
                .await??
            }
        };
        self.remember_repository(workforce, &repository)?;

        let started = self
            .author(
                loaded,
                Actor::Daemon(setup.principal),
                EventBody::SessionStarted(SessionStarted {
                    agent: setup.agent.clone(),
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
                self.move_item(loaded, setup.principal, item, to, None, Some(session))
                    .await?;
            }
            std::fs::create_dir_all(&setup.transcripts).map_err(|source| SessionError::Io {
                path: setup.transcripts.clone(),
                source,
            })
        }
        .await;
        if let Err(error) = prepared {
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
        let run = self.run(setup, session, worktree, repository, Begin::New);
        tracing::info!(
            "session {}: {} started on item {}{}",
            session.short(),
            run.agent,
            item.short(),
            run.isolation()
        );
        self.spawn_run(loaded, run);
        Ok(session)
    }

    /// The session as its task runs it.
    pub(super) fn run(
        self: &Arc<Self>,
        setup: Setup,
        session: SessionId,
        worktree: PathBuf,
        repository: PathBuf,
        begin: Begin,
    ) -> Run {
        let Setup {
            workforce,
            principal: _,
            agent,
            item,
            role,
            definition,
            harness,
            sandbox,
            environment,
            git_identity,
            agent_signing,
            item_context,
            target,
            transcripts,
        } = setup;
        // The agent commits as itself, `@principal/agent`, as it is named everywhere else.
        let mut git_environment = agent_signing.environment();
        for variable in ["GIT_AUTHOR_NAME", "GIT_COMMITTER_NAME"] {
            git_environment.push((variable.to_string(), git_identity.clone()));
        }
        let autonomy = definition.autonomy;
        let permissions: Arc<dyn Permissions> = match sandbox.permissions(autonomy.permissions) {
            wf_core::autonomy::PermissionMode::Allow => Arc::new(Permission::Allow),
            wf_core::autonomy::PermissionMode::Deny => Arc::new(Permission::Deny),
            wf_core::autonomy::PermissionMode::Ask => Arc::new(Deciding {
                held: Arc::clone(&self.held),
                workforce,
                item,
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
        Run {
            workforce,
            session,
            item,
            role,
            agent,
            harness,
            model: definition.model.clone(),
            worktree,
            transcript: transcripts.join(format!("{session}.jsonl")),
            agent_session: super::resume::record_path(&transcripts, session),
            begin,
            branch: item_branch(item),
            target,
            repository,
            nudges: autonomy.nudges,
            park_timeout: autonomy.park_timeout,
            max_duration: autonomy.max_duration,
            permissions,
            sandbox,
            git_environment,
            environment,
            autonomy_permissions: autonomy.permissions,
        }
    }

    /// Starts the task that runs the harness of a session.
    pub(super) fn spawn_run(self: &Arc<Self>, loaded: &Arc<Loaded>, run: Run) {
        let (stop, stopped) = mpsc::unbounded_channel();
        self.runners.add(run.session, stop);
        let task = tokio::spawn(run_session(
            Arc::clone(self),
            Arc::clone(loaded),
            run,
            stopped,
        ));
        self.runners.track(task);
    }
}

/// What it takes to run an agent on an item, before there is a session.
pub(super) struct Setup {
    pub(super) workforce: WorkforceId,
    pub(super) principal: PrincipalKey,
    pub(super) agent: AgentName,
    pub(super) item: ItemId,
    pub(super) role: SessionRole,
    pub(super) definition: AgentDefinition,
    pub(super) harness: AcpAgentConfig,
    pub(super) sandbox: SandboxConfig,
    pub(super) environment: EnvironmentConfig,
    /// The name the agent commits under.
    pub(super) git_identity: String,
    pub(super) agent_signing: wf_vcs::CommitSigning,
    pub(super) item_context: ItemContext,
    pub(super) target: String,
    pub(super) transcripts: PathBuf,
}

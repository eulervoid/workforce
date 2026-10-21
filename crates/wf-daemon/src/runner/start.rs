//! Starting sessions: the repository or directory a session runs in, its harness and sandbox.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;
use wf_core::event::{EventBody, SessionStarted};
use wf_core::id::{HashId, ItemId, SessionId, WorkforceId};
use wf_core::identity::{Actor, PrincipalKey};
use wf_core::item::ItemEntry;
use wf_core::lifecycle::{Interruption, SessionEnd};
use wf_core::name::AgentName;
use wf_core::session::{MachineFault, SessionRole};
use wf_core::state::{AgentDefinition, State};
use wf_harness::{AcpAgentConfig, Permission, PermissionRequest, Permissions};
use wf_vcs::{GitCli, item_branch, session_directory};

use super::drive::run_session;
use super::{Begin, Run, SessionError, blocking};
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
    let profile_file = daemon
        .workforce_directory(run.workforce)
        .join("sandbox")
        .join(format!("{}.json", run.session));
    sandboxed_in(daemon, &run.sandbox, &run.directory, &profile_file, agent)
}

/// `agent` in `sandbox`, working in `directory`; Fence's sandbox profile is written to `profile_file`.
pub(crate) fn sandboxed_in(
    daemon: &Daemon,
    sandbox: &SandboxConfig,
    directory: &std::path::Path,
    profile_file: &std::path::Path,
    agent: AcpAgentConfig,
) -> Result<AcpAgentConfig, String> {
    let config = match sandbox {
        SandboxConfig::Fence(_) if !crate::sandbox::fence_installed() => {
            return Err("Fence is not installed".to_string());
        }
        SandboxConfig::Fence(config) => config,
        SandboxConfig::Off {} => return Ok(agent),
    };
    let access = crate::sandbox::Access {
        directory,
        socket: &daemon.launch.socket,
        data_dir: &daemon.launch.data_dir,
        machine_config: daemon.launch.machine_config.as_deref(),
    };
    crate::sandbox::wrap(config, &access, &agent, profile_file).map_err(|error| error.to_string())
}

/// The name an agent of `principal` commits under: `@principal/agent`, as it is named everywhere
/// else.
pub(super) fn git_identity(
    state: &State,
    principal: PrincipalKey,
    agent: &AgentName,
) -> Result<String, SessionError> {
    let handle = &state
        .member(&principal)
        .ok_or(SessionError::NotBound)?
        .handle;
    Ok(format!("@{handle}/{agent}"))
}

impl Daemon {
    /// What it takes to run `agent` on `item` in `role`, as the state and the machine config say:
    /// checked before a session is written, so that a session that cannot run is not started.
    pub(super) fn setup(
        &self,
        state: &State,
        workforce: WorkforceId,
        agent: AgentName,
        item: ItemId,
        role: &SessionRole,
    ) -> Result<Setup, SessionError> {
        let principal = self.principal_of_device(state)?;
        let git_identity = git_identity(state, principal, &agent)?;
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
        let policy = &state.policy().ok_or(SessionError::NoPolicy)?.policy;
        let item_context = match state.item(item).ok_or(SessionError::UnknownItem(item))? {
            ItemEntry::Live(live) => ItemContext {
                id: item.to_base32(),
                title: live.title().to_string(),
                item_type: live.item_type().to_string(),
                state: live.state(policy).to_string(),
            },
            ItemEntry::Archived(_) => return Err(SessionError::Archived(item)),
        };
        let transcripts = self.launch.data_dir.transcripts(&workforce);
        Ok(Setup {
            workforce,
            principal,
            agent,
            item,
            role: role.clone(),
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

    /// The repository or directory `session` of `role` on `item` runs in.
    pub(super) fn session_directory(
        &self,
        workforce: WorkforceId,
        item: ItemId,
        session: SessionId,
        role: &SessionRole,
    ) -> PathBuf {
        session_directory(&self.workforce_directory(workforce), item, session, role)
    }

    /// Starts a session of `agent` on `item`, and a task that runs the harness. A work session
    /// runs in a repository of the agent's own with the branch of the item checked out, and
    /// moves the item to `active`; a review session in a repository of the same kind with the
    /// commit under review checked out, or an empty directory when it was submitted without
    /// commits; a supervise or diagnose session in an empty directory.
    pub(crate) async fn start_session(
        self: &Arc<Self>,
        workforce: WorkforceId,
        loaded: &Arc<Loaded>,
        agent: AgentName,
        item: ItemId,
        role: SessionRole,
    ) -> Result<SessionId, SessionError> {
        // A start by hand and one by the dispatcher must not both see the item free: the state is
        // read and the claim checked under a lock held until `SessionStarted` is written. What a
        // gone device's session claims is taken over: its session ends `LeaseExpired` first.
        let _starting = match role {
            SessionRole::Work => Some(
                self.starting
                    .take(item)
                    .ok_or(SessionError::Starting(item))?,
            ),
            SessionRole::Review { .. } | SessionRole::Supervise | SessionRole::Diagnose => None,
        };
        let state = self.snapshot(loaded).await?;
        let setup = self.setup(&state, workforce, agent, item, &role)?;
        let presence = self.presence_now();
        let takeover =
            state.claimable(item, &role, self.device(), &presence, wf_store::now_ms())?;
        self.ensure_bare_repository(workforce, &loaded.storage)
            .await;
        // What the log names as the start of the work, or as the work to review, has to be here.
        let head = state.work_head(item);
        match &role {
            SessionRole::Work => {
                if let Some(head) = head {
                    self.require_commit(workforce, head).await?;
                }
            }
            SessionRole::Review {
                commit: Some(commit),
                ..
            } => self.require_commit(workforce, *commit).await?,
            SessionRole::Review { commit: None, .. }
            | SessionRole::Supervise
            | SessionRole::Diagnose => {}
        }
        let repository = self.bare_repository(workforce);
        let git = blocking(move || GitCli::open(&repository)).await??;

        for session in takeover {
            self.take_over(loaded, session, &presence).await?;
        }
        let started = self
            .author(
                loaded,
                Actor::Daemon(setup.principal),
                EventBody::SessionStarted(SessionStarted {
                    agent: setup.agent.clone(),
                    item,
                    role: role.clone(),
                }),
            )
            .await?;
        let session = SessionId::from(started.id());
        // Once the session is in the log, a failure to set it up ends it: otherwise it would hold
        // the item and a slot until the next daemon starts.
        let directory = self.session_directory(workforce, item, session, &role);
        let prepared = async {
            match &role {
                // The branch is the claim's to move, so only once the claim is written.
                SessionRole::Work => {
                    let path = directory.clone();
                    let (branch, target, git) =
                        (item_branch(item), setup.target.clone(), git.clone());
                    blocking(move || {
                        if let Some(head) = head {
                            git.bring_item_branch_to(item, head)?;
                        }
                        let commit = git.starting_commit(item, &target)?;
                        git.create_agent_repository(&path, &branch, commit)?;
                        git.ensure_item_branch(item, commit)
                    })
                    .await??;
                }
                SessionRole::Review {
                    commit: Some(commit),
                    ..
                } => {
                    let (path, git, commit) = (directory.clone(), git.clone(), *commit);
                    blocking(move || git.create_review_repository(&path, commit)).await??;
                }
                SessionRole::Review { commit: None, .. }
                | SessionRole::Supervise
                | SessionRole::Diagnose => {
                    std::fs::create_dir_all(&directory).map_err(|source| SessionError::Io {
                        path: directory.clone(),
                        source,
                    })?;
                }
            }
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
            let interruption = if self.faults.note_space("the start of a session", &error) {
                Interruption::MachineFault(MachineFault::NoSpace)
            } else {
                Interruption::Failed {
                    because: format!(
                        "the daemon could not set it up: {}",
                        wf_core::error_chain(&error)
                    ),
                }
            };
            if let Err(ending) = self
                .finish_session(
                    workforce,
                    loaded,
                    session,
                    SessionEnd::Interruption(interruption),
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
        let run = self.run(setup, session, directory, git, Begin::New);
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
        directory: PathBuf,
        git: GitCli,
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
                machine_config: self.launch.machine_config.clone(),
                agents: crate::judges::WorkforceJudges {
                    daemon: Arc::downgrade(self),
                    workforce,
                },
                context: DeciderRequest {
                    header: Default::default(),
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
                    directory: directory.clone(),
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
            harness: definition.harness.clone(),
            command: harness,
            model: definition.model.clone(),
            directory,
            transcript: self.launch.data_dir.transcript(&workforce, session),
            agent_session: super::resume::record_path(&transcripts, session),
            begin,
            branch: item_branch(item),
            target,
            git,
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

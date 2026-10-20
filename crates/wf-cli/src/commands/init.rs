//! `wf init`: a new workforce, or this machine joining one.

use std::path::Path;

use anyhow::{Context, bail};
use serde::Serialize;
use wf_core::autonomy::Autonomy;
use wf_core::event::{AgentDefined, EventBody, Genesis};
use wf_core::identity::{Actor, MemberSeed, PrincipalSigner, Signed};
use wf_core::name::{Handle, LaneName};
use wf_core::policy::DEFAULT_POLICY_TOML;
use wf_store::{RedbStore, Replica};

use super::{emit, handle_from_environment};
use crate::cli::*;
use crate::harness;
use crate::interactive::{self, Mode};
use crate::workspace::{self, WORKFORCE_FILE, WorkforceFile};
use wf_daemon::DataDir;

#[derive(Serialize)]
struct InitView {
    name: String,
    id: String,
    handle: String,
    /// The harnesses found on PATH, which agents can be added for.
    harnesses: Vec<String>,
    /// The agents defined, which only `wf init -i` does.
    agents: Vec<String>,
    /// The upstream of the target in this checkout, such as `wf/main`: where `git pull` brings
    /// merged work from and `git push` sends the principal's own commits. `None` when this
    /// directory is not in a git repository.
    upstream: Option<String>,
}

pub(super) fn init(
    json: bool,
    mode: Mode,
    args: InitArgs,
    data: DataDir,
    cwd: &Path,
) -> anyhow::Result<()> {
    if workspace::find_workforce(cwd).is_ok() {
        bail!("this directory already belongs to a workforce ({WORKFORCE_FILE})");
    }
    data.require_no_daemon("creating a workforce")?;
    let path = std::env::var_os("PATH").unwrap_or_default();
    let harnesses = harness::detect(&path);
    let (name, setups) = match (args.interactive, mode) {
        (true, Mode::Interactive) => {
            let answers = interactive::init_dialog(args.name, &harnesses)?;
            (answers.name, answers.agents)
        }
        (true, Mode::Scripted) => bail!("`wf init -i` needs a terminal"),
        (false, _) => (
            args.name.context("pass --name to name the workforce")?,
            Vec::new(),
        ),
    };
    let handle = match args.handle {
        Some(handle) => Handle::new(handle)?,
        None => handle_from_environment(),
    };
    let principal = PrincipalSigner::generate(rand::random());
    let device = data.device_or_create()?;

    let genesis = Genesis {
        name: name.clone(),
        founder: MemberSeed {
            principal: principal.key(),
            handle: handle.clone(),
            devices: vec![device.key()],
            previous: Vec::new(),
        },
        members: Vec::new(),
        policy: DEFAULT_POLICY_TOML.to_string(),
        forked_from: None,
    };
    let body = EventBody::Genesis(Signed::sign(&principal, None, genesis)?);
    let actor = Actor::Principal(principal.key());
    let genesis_event =
        wf_core::event::EventBuilder::genesis(device.key(), actor.clone(), wf_store::now_ms())
            .sign(&device, body)?;
    let id: wf_core::id::WorkforceId = genesis_event.id().into();

    data.store_principal(&id, &principal)?;
    let mut replica = Replica::open(RedbStore::open(workspace::store_path(&data, &id)?)?, id)?;
    if replica.state().workforce_id().is_some() {
        bail!("a local log for workforce {id} already exists");
    }
    replica.receive(genesis_event)?;

    replica.author(
        &device,
        actor.clone(),
        wf_store::now_ms(),
        EventBody::DeviceProfile(wf_core::event::DeviceProfile {
            name: args.device_name.unwrap_or_else(crate::network::host_name),
            hardware: crate::network::detect_hardware(),
            agents: Default::default(),
            max_sessions: 1,
        }),
    )?;

    let mut agents = Vec::new();
    for setup in setups {
        let autonomy = Autonomy {
            permissions: setup.permissions,
            ..Autonomy::default()
        };
        replica.author(
            &device,
            actor.clone(),
            wf_store::now_ms(),
            EventBody::AgentDefined(AgentDefined {
                name: setup.name.clone(),
                harness: setup.harness.clone(),
                model: setup.model.clone(),
                instructions: None,
                capabilities: Vec::new(),
                lanes: vec![LaneName::new("default")?],
                autonomy,
            }),
        )?;
        let model = setup.model.as_deref().unwrap_or("its default model");
        agents.push(format!(
            "@{handle}/{} ({}, {model}, permissions {})",
            setup.name, setup.harness, setup.permissions
        ));
    }

    let target = replica
        .state()
        .policy()
        .expect("a workforce has a policy")
        .policy
        .merge
        .target
        .clone();
    let checkout = match wf_vcs::GitCli::checkout_root(cwd) {
        Ok(root) => Some(root),
        Err(wf_vcs::VcsError::NotARepository(_)) => None,
        Err(error) => return Err(error.into()),
    };
    let bare = wf_vcs::bare_repository(&data.workforce(&id));
    wf_vcs::GitCli::create_bare_repository(&bare, &target, checkout.as_deref())?;
    let upstream = checkout.map(|_| format!("{}/{target}", wf_vcs::REMOTE));

    let file = toml::to_string(&WorkforceFile { id })?;
    std::fs::write(cwd.join(WORKFORCE_FILE), file)?;
    emit(
        json,
        &InitView {
            name,
            id: id.to_string(),
            handle: handle.to_string(),
            harnesses: harnesses.into_iter().map(String::from).collect(),
            agents,
            upstream,
        },
        |init| {
            let mut out = format!(
                "created workforce \"{}\" ({})\nyou are @{}\n",
                init.name, init.id, init.handle
            );
            if let Some(upstream) = &init.upstream {
                out.push_str(&format!(
                    "merged work comes into this checkout with `git pull` and your commits go out with `git push`: {upstream}\n"
                ));
            }
            if !init.agents.is_empty() {
                for agent in &init.agents {
                    out.push_str(&format!("defined agent {agent}\n"));
                }
                return out;
            }
            if init.harnesses.is_empty() {
                out.push_str("no agent harness found on PATH; add an agent with `wf agent add`\n");
                return out;
            }
            out.push_str("found harnesses; add an agent for the one wf should use:\n");
            for harness in &init.harnesses {
                out.push_str(&format!("  wf agent add --harness {harness}\n"));
            }
            out
        },
    )
}

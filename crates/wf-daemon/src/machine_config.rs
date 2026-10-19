//! The config of this machine, `$XDG_CONFIG_HOME/workforce/config.toml`: what only makes sense
//! here, such as programs to run and model APIs to call. It is never part of the signed policy,
//! because policy that syncs must not make other machines run programs or call paid APIs.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use bytesize::ByteSize;
use serde::Deserialize;
use url::Url;
use wf_core::autonomy::PermissionMode;
use wf_core::inbox::InboxKind;
use wf_core::name::AgentName;
use wf_core::policy::CheckConfig;
use wf_harness::Decider;
use wf_vcs::CommitSigning;

mod theme;
pub use theme::{
    Background, PriorityThemeConfig, ReadinessThemeConfig, StateThemeConfig, ThemeConfig,
};

#[derive(Debug, thiserror::Error)]
pub enum MachineConfigError {
    #[error("{path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: Box<toml::de::Error>,
    },
    #[error("{path}: {problem}")]
    Invalid { path: PathBuf, problem: String },
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineConfig {
    #[serde(default)]
    pub permissions: PermissionsConfig,
    #[serde(default)]
    pub sandbox: SandboxConfig,
    #[serde(default)]
    pub commits: CommitsConfig,
    #[serde(default)]
    pub environment: EnvironmentConfig,
    /// What one agent gets on this machine, on top of what every agent gets.
    #[serde(rename = "agent", default)]
    pub agents: BTreeMap<AgentName, AgentConfig>,
    #[serde(default)]
    pub notify: NotifyConfig,
    /// Without it, the daemon deletes no cache.
    pub storage: Option<StorageConfig>,
    #[serde(default)]
    pub check: CheckRunConfig,
    /// The colours of the CLI and the dashboard.
    #[serde(default)]
    pub theme: ThemeConfig,
}

/// How the checks of the policy run on this machine: `[check]`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckRunConfig {
    /// The longest a check runs here, whatever its timeout in the policy, such as `"10m"`.
    #[serde(default, with = "humantime_serde::option")]
    pub max_timeout: Option<Duration>,
    /// The sandbox checks run in, when it is not the one of every agent: `{ kind = "off" }` for
    /// tests that need what Fence cannot allow, such as the route socket iroh opens on macOS.
    pub sandbox: Option<SandboxConfig>,
}

impl CheckRunConfig {
    /// How long `check` may run on this machine.
    pub fn timeout(&self, check: &CheckConfig) -> Duration {
        self.max_timeout
            .map_or(check.timeout, |max| max.min(check.timeout))
    }
}

/// How much disk the caches in the worktrees of each workforce may take on this machine.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    /// Over it, the daemon deletes the caches of the least recently used worktrees, such as
    /// `"20GB"`.
    pub budget: ByteSize,
    /// The paths inside a worktree that can be rebuilt, such as `["target"]`.
    pub caches: Vec<PathBuf>,
}

impl StorageConfig {
    /// A cache must stay inside its worktree, so that deleting it never deletes anything else.
    fn check(&self) -> Result<(), String> {
        for cache in &self.caches {
            let inside = cache.components().next().is_some()
                && cache
                    .components()
                    .all(|component| matches!(component, Component::Normal(_)));
            if !inside {
                return Err(format!(
                    "`{}` under `[storage]` is not a path inside a worktree",
                    cache.display()
                ));
            }
        }
        Ok(())
    }
}

/// The variables an agent gets besides the ones wf sets: a few that every program expects, such
/// as `HOME` and `PATH`, and those named here. Nothing else of the daemon's environment reaches it.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentConfig {
    /// Variables of the daemon's environment passed on, when they are set.
    #[serde(default)]
    pub inherit: Vec<String>,
    /// Variables set to these values. `~` and `$NAME` are expanded from the daemon's environment
    /// when the config is read, and `$$` is a `$`. A relative path is relative to the directory
    /// the agent runs in.
    #[serde(default)]
    pub set: BTreeMap<String, String>,
}

/// What this machine gives one agent, on top of what it gives every agent.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    #[serde(default)]
    pub environment: EnvironmentConfig,
    /// More that Fence allows the agent.
    #[serde(default)]
    pub sandbox: FenceConfig,
}

/// How the principal is told that something new waits on them.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotifyConfig {
    /// The program and its arguments, run without a shell once for each new entry of the inbox.
    /// `{kind}`, `{item}`, `{title}` and `{what}` in an argument are replaced as `wf inbox` says
    /// them. No command, no notifications.
    pub command: Option<Vec<String>>,
}

/// One entry of the inbox, as a notification tells of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub kind: InboxKind,
    /// The short id of the item.
    pub item: String,
    pub title: String,
    pub what: String,
}

impl NotifyConfig {
    /// The command for `notice`, with its placeholders replaced; `None` without a command.
    pub fn command_for(&self, notice: &Notice) -> Option<Vec<String>> {
        let command = self.command.as_ref()?;
        Some(
            command
                .iter()
                .map(|argument| fill_placeholders(argument, notice))
                .collect(),
        )
    }
}

/// Replaces the placeholders of `argument` in one pass, so that a title that says `{what}` stays
/// as it is.
fn fill_placeholders(argument: &str, notice: &Notice) -> String {
    let mut filled = String::with_capacity(argument.len());
    let mut rest = argument;
    while let Some(start) = rest.find('{') {
        filled.push_str(&rest[..start]);
        rest = &rest[start..];
        let value = [
            ("{kind}", notice.kind.as_str()),
            ("{item}", notice.item.as_str()),
            ("{title}", notice.title.as_str()),
            ("{what}", notice.what.as_str()),
        ]
        .into_iter()
        .find(|(placeholder, _)| rest.starts_with(placeholder));
        match value {
            Some((placeholder, value)) => {
                filled.push_str(value);
                rest = &rest[placeholder.len()..];
            }
            None => {
                filled.push('{');
                rest = &rest[1..];
            }
        }
    }
    filled.push_str(rest);
    filled
}

/// How commits are signed on this machine.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitsConfig {
    #[serde(default)]
    pub agent_signing: AgentSigningConfig,
    #[serde(default)]
    pub merge_signing: MergeSigningConfig,
}

/// Whether agents sign their commits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentSigningConfig {
    /// Never, so that an agent never waits for a key the principal has to unlock or approve.
    #[default]
    Unsigned,
    /// As the git config says, as for the principal's own commits.
    AsConfigured,
}

/// Who signs the merge commit that brings approved work into the target.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeSigningConfig {
    /// As the git config says: the principal, with their own key, when it signs commits.
    #[default]
    AsConfigured,
    /// Nobody.
    Unsigned,
    /// The device of the daemon, with its key: `wf device allowed-signers` lets git check it.
    Device,
}

impl AgentSigningConfig {
    pub fn signing(self) -> CommitSigning {
        match self {
            AgentSigningConfig::Unsigned => CommitSigning::Unsigned,
            AgentSigningConfig::AsConfigured => CommitSigning::AsConfigured,
        }
    }
}

/// How agents are isolated on this machine.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SandboxConfig {
    Fence(FenceConfig),
    /// Agents run with the privileges of the daemon, so an agent that is allowed everything is
    /// asked instead. A struct variant, so that unknown fields are refused.
    Off {},
}

impl Default for SandboxConfig {
    fn default() -> Self {
        SandboxConfig::Fence(FenceConfig::default())
    }
}

impl SandboxConfig {
    /// What the permission mode of an agent amounts to here: allowing everything is only safe
    /// inside a sandbox.
    pub fn permissions(&self, mode: PermissionMode) -> PermissionMode {
        match (self, mode) {
            (SandboxConfig::Off {}, PermissionMode::Allow) => PermissionMode::Ask,
            (SandboxConfig::Off {}, PermissionMode::Ask | PermissionMode::Deny)
            | (SandboxConfig::Fence(_), _) => mode,
        }
    }
}

/// Runs agents in [Fence](https://github.com/fencesandbox/fence), which has to be installed. An
/// agent may write in its worktree and where it needs to commit and talk to the daemon, and
/// reach the model APIs and package registries of Fence's `code` template; everything else is
/// denied. What this adds is on top of that, except for wf's keys and config and git's config and
/// hooks, which the sandbox always denies.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FenceConfig {
    /// More paths the agent may write to, expanded as the values of `[environment.set]`.
    #[serde(default)]
    pub allow_write: Vec<PathBuf>,
    /// More domains the agent may reach, such as `*.example.com`.
    #[serde(default)]
    pub allow_domains: Vec<String>,
}

/// How the permission requests of agents that ask are decided on this machine.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionsConfig {
    /// What may and may not be done, in words, for deciders that read: models and agents.
    pub guidance: Option<String>,
    /// Asked in this order; what all of them pass on goes to the principal.
    #[serde(rename = "decider", default = "default_deciders")]
    pub deciders: Vec<DeciderConfig>,
}

impl Default for PermissionsConfig {
    fn default() -> Self {
        PermissionsConfig {
            guidance: None,
            deciders: default_deciders(),
        }
    }
}

fn default_deciders() -> Vec<DeciderConfig> {
    vec![DeciderConfig::Rules {}]
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DeciderConfig {
    /// The built-in rules. A struct variant, so that unknown fields are refused.
    Rules {},
    /// A program that gets the request as JSON on stdin and prints its answer as JSON.
    Hook {
        /// The program and its arguments, run without a shell.
        command: Vec<String>,
        #[serde(default = "default_timeout", with = "humantime_serde")]
        timeout: Duration,
    },
    /// A decision model behind the System One API.
    Model {
        url: Url,
        model: String,
        /// The environment variable that holds the API key.
        api_key_env: String,
        question: QuestionConfig,
        #[serde(default = "default_timeout", with = "humantime_serde")]
        timeout: Duration,
    },
    /// An agent of the workforce that the daemon keeps a session of.
    Agent {
        agent: AgentName,
        /// Prompts before its session is replaced, so its context stays small.
        #[serde(default = "default_turns")]
        turns: u32,
    },
}

impl DeciderConfig {
    /// Who decides, as transcripts and the inbox name it.
    pub fn decider(&self) -> Decider {
        match self {
            DeciderConfig::Rules {} => Decider::Rules,
            DeciderConfig::Hook { command, .. } => Decider::Hook {
                program: command.first().cloned().unwrap_or_default(),
            },
            DeciderConfig::Model { model, .. } => Decider::Model {
                model: model.clone(),
            },
            DeciderConfig::Agent { agent, .. } => Decider::Agent {
                agent: agent.to_string(),
            },
        }
    }
}

/// What a decision model is asked, and how its answer becomes a decision.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum QuestionConfig {
    /// Whether the call should be allowed, as a probability: allow above `allow_above`, deny below
    /// `deny_below`, pass on in between.
    Noul { allow_above: f64, deny_below: f64 },
    /// Which of allow, deny and ask fits, taken when its confidence is at least `min_confidence`.
    Choice { min_confidence: f64 },
}

fn default_turns() -> u32 {
    50
}

fn default_timeout() -> Duration {
    Duration::from_secs(20)
}

impl MachineConfig {
    /// `$XDG_CONFIG_HOME/workforce/config.toml`, falling back to `~/.config/workforce/config.toml`.
    pub fn path_from_env() -> Option<PathBuf> {
        let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
            Some(path) => PathBuf::from(path),
            None => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
        };
        Some(base.join("workforce").join("config.toml"))
    }

    /// Reads the config at `path`; a missing file is the default config.
    pub fn load(path: &Path) -> Result<Self, MachineConfigError> {
        let Some(text) = read(path)? else {
            return Ok(Self::default());
        };
        Self::parse(&text).map_err(|error| match error {
            ParseError::Toml(source) => MachineConfigError::Parse {
                path: path.to_path_buf(),
                source: Box::new(source),
            },
            ParseError::Invalid(problem) => MachineConfigError::Invalid {
                path: path.to_path_buf(),
                problem,
            },
        })
    }

    fn parse(text: &str) -> Result<Self, ParseError> {
        Self::parse_in(text, &|name| std::env::var(name).ok())
    }

    /// Parses `text`, expanding the variables of `[environment.set]` and the paths Fence allows
    /// from `variable`.
    fn parse_in(text: &str, variable: &dyn Fn(&str) -> Option<String>) -> Result<Self, ParseError> {
        let mut config: MachineConfig = toml::from_str(text).map_err(ParseError::Toml)?;
        for decider in &config.permissions.deciders {
            decider.check().map_err(ParseError::Invalid)?;
        }
        if config.notify.command.as_ref().is_some_and(Vec::is_empty) {
            return Err(ParseError::Invalid(
                "`command` under `[notify]` needs a program".to_string(),
            ));
        }
        if let Some(storage) = &config.storage {
            storage.check().map_err(ParseError::Invalid)?;
        }
        config
            .environment
            .check()
            .map_err(|problem| ParseError::Invalid(format!("[environment]: {problem}")))?;
        for (agent, agent_config) in &config.agents {
            agent_config.environment.check().map_err(|problem| {
                ParseError::Invalid(format!("[agent.{agent}.environment]: {problem}"))
            })?;
            match &config.sandbox {
                SandboxConfig::Off {} if agent_config.sandbox != FenceConfig::default() => {
                    return Err(ParseError::Invalid(format!(
                        "[agent.{agent}.sandbox] adds to what Fence allows, but the sandbox is off"
                    )));
                }
                SandboxConfig::Off {} | SandboxConfig::Fence(_) => {}
            }
        }
        config.environment.expand(variable)?;
        for sandbox in [Some(&mut config.sandbox), config.check.sandbox.as_mut()]
            .into_iter()
            .flatten()
        {
            if let SandboxConfig::Fence(fence) = sandbox {
                fence.expand(variable)?;
            }
        }
        for agent_config in config.agents.values_mut() {
            agent_config.environment.expand(variable)?;
            agent_config.sandbox.expand(variable)?;
        }
        Ok(config)
    }

    /// What `agent` gets in its environment: what every agent gets, and what it gets on top,
    /// which wins where both set a variable.
    pub fn environment_of(&self, agent: &AgentName) -> EnvironmentConfig {
        let mut environment = self.environment.clone();
        if let Some(own) = self.agents.get(agent) {
            environment
                .inherit
                .extend(own.environment.inherit.iter().cloned());
            environment.set.extend(own.environment.set.clone());
        }
        environment
    }

    /// The sandbox checks run in: their own, or else the one of every agent.
    pub fn check_sandbox(&self) -> &SandboxConfig {
        self.check.sandbox.as_ref().unwrap_or(&self.sandbox)
    }

    /// The sandbox `agent` runs in: what Fence allows every agent, and what it allows the agent.
    pub fn sandbox_of(&self, agent: &AgentName) -> SandboxConfig {
        match (&self.sandbox, self.agents.get(agent)) {
            (SandboxConfig::Fence(fence), Some(own)) => {
                let mut fence = fence.clone();
                fence
                    .allow_write
                    .extend(own.sandbox.allow_write.iter().cloned());
                fence
                    .allow_domains
                    .extend(own.sandbox.allow_domains.iter().cloned());
                SandboxConfig::Fence(fence)
            }
            (sandbox, _) => sandbox.clone(),
        }
    }
}

/// Whether wf sets the variable `name` itself, so that the config must not.
fn set_by_wf(name: &str) -> bool {
    name == "PATH" || name.starts_with("WF_") || wf_vcs::config::is_set_by_wf(name)
}

/// `text` with `~` and `$NAME` expanded from `variable`; a variable that is not set is an error.
fn expand(text: &str, variable: &dyn Fn(&str) -> Option<String>) -> Result<String, ParseError> {
    shellexpand::full_with_context(
        text,
        || variable("HOME"),
        |name| {
            variable(name)
                .map(Some)
                .ok_or(std::env::VarError::NotPresent)
        },
    )
    .map(|expanded| expanded.into_owned())
    .map_err(|error| ParseError::Invalid(format!("`{text}`: {error}")))
}

impl EnvironmentConfig {
    fn check(&self) -> Result<(), String> {
        for name in self.inherit.iter().chain(self.set.keys()) {
            if name.is_empty() || name.contains(['=', '\0']) {
                return Err(format!("`{name}` is not a variable name"));
            }
            if set_by_wf(name) {
                return Err(format!("wf sets `{name}` itself"));
            }
        }
        Ok(())
    }

    fn expand(&mut self, variable: &dyn Fn(&str) -> Option<String>) -> Result<(), ParseError> {
        for value in self.set.values_mut() {
            *value = expand(value, variable)?;
        }
        Ok(())
    }
}

impl FenceConfig {
    fn expand(&mut self, variable: &dyn Fn(&str) -> Option<String>) -> Result<(), ParseError> {
        for path in &mut self.allow_write {
            if let Some(text) = path.to_str() {
                *path = PathBuf::from(expand(text, variable)?);
            }
        }
        Ok(())
    }
}

/// The text of the config at `path`, or `None` when there is no file.
fn read(path: &Path) -> Result<Option<String>, MachineConfigError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(MachineConfigError::Read {
            path: path.to_path_buf(),
            source,
        }),
    }
}

impl ThemeConfig {
    /// Reads only `[theme]` of the config at `path`, so a mistake elsewhere leaves the colours
    /// alone; a missing file sets nothing.
    pub fn load(path: &Path) -> Result<Self, MachineConfigError> {
        /// The machine config, seen only for its theme.
        #[derive(Deserialize)]
        struct OnlyTheme {
            #[serde(default)]
            theme: ThemeConfig,
        }
        let Some(text) = read(path)? else {
            return Ok(Self::default());
        };
        toml::from_str::<OnlyTheme>(&text)
            .map(|config| config.theme)
            .map_err(|source| MachineConfigError::Parse {
                path: path.to_path_buf(),
                source: Box::new(source),
            })
    }
}

enum ParseError {
    Toml(toml::de::Error),
    Invalid(String),
}

impl DeciderConfig {
    fn check(&self) -> Result<(), String> {
        let probability = |value: f64| (0.0..=1.0).contains(&value);
        match self {
            DeciderConfig::Rules {} | DeciderConfig::Agent { .. } => Ok(()),
            DeciderConfig::Hook { command, .. } if command.is_empty() => {
                Err("a hook needs a command".to_string())
            }
            DeciderConfig::Hook { .. } => Ok(()),
            DeciderConfig::Model { question, .. } => match *question {
                QuestionConfig::Noul {
                    allow_above,
                    deny_below,
                } if !probability(allow_above) || !probability(deny_below) => {
                    Err("`allow_above` and `deny_below` are probabilities, from 0 to 1".to_string())
                }
                QuestionConfig::Noul {
                    allow_above,
                    deny_below,
                } if deny_below >= allow_above => {
                    Err("`deny_below` must be lower than `allow_above`".to_string())
                }
                QuestionConfig::Noul { .. } => Ok(()),
                QuestionConfig::Choice { min_confidence } if !probability(min_confidence) => {
                    Err("`min_confidence` is a probability, from 0 to 1".to_string())
                }
                QuestionConfig::Choice { .. } => Ok(()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_config_asks_the_rules_and_then_the_principal() {
        assert_eq!(
            MachineConfig::parse("").ok().unwrap().permissions.deciders,
            [DeciderConfig::Rules {}]
        );
    }

    #[test]
    fn agents_run_in_fence_unless_the_sandbox_is_off() {
        let default = MachineConfig::parse("").ok().unwrap().sandbox;
        assert_eq!(default, SandboxConfig::Fence(FenceConfig::default()));
        assert_eq!(
            default.permissions(PermissionMode::Allow),
            PermissionMode::Allow
        );

        let text = "[sandbox]\nkind = \"fence\"\nallow_domains = [\"example.com\"]\n";
        let SandboxConfig::Fence(fence) = MachineConfig::parse(text).ok().unwrap().sandbox else {
            panic!("a fence sandbox");
        };
        assert_eq!(fence.allow_domains, ["example.com"]);

        let off = MachineConfig::parse("[sandbox]\nkind = \"off\"\n")
            .ok()
            .unwrap()
            .sandbox;
        assert_eq!(off, SandboxConfig::Off {});
        assert_eq!(off.permissions(PermissionMode::Allow), PermissionMode::Ask);
        assert_eq!(off.permissions(PermissionMode::Deny), PermissionMode::Deny);
        assert!(MachineConfig::parse("[sandbox]\nkind = \"off\"\nallow_domains = []\n").is_err());
    }

    #[test]
    fn agents_commit_unsigned_and_merges_are_signed_as_configured_by_default() {
        let default = MachineConfig::parse("").ok().unwrap().commits;
        assert_eq!(default.agent_signing, AgentSigningConfig::Unsigned);
        assert_eq!(default.merge_signing, MergeSigningConfig::AsConfigured);
        let text = "[commits]\nagent_signing = \"as_configured\"\nmerge_signing = \"device\"\n";
        let configured = MachineConfig::parse(text).ok().unwrap().commits;
        assert_eq!(configured.agent_signing, AgentSigningConfig::AsConfigured);
        assert_eq!(configured.merge_signing, MergeSigningConfig::Device);
        assert!(MachineConfig::parse("[commits]\nagent_signing = \"device\"\n").is_err());
        assert!(MachineConfig::parse("[commits]\nagent_signing = \"always\"\n").is_err());
    }

    #[test]
    fn a_notify_command_gets_the_entry_in_its_placeholders() {
        assert_eq!(MachineConfig::parse("").ok().unwrap().notify.command, None);
        let notice = Notice {
            kind: InboxKind::Blocker,
            item: "k3f7ab".to_string(),
            title: "Says {what}".to_string(),
            what: "answer it".to_string(),
        };
        assert_eq!(
            NotifyConfig::default().command_for(&notice),
            None,
            "no command, no notifications"
        );
        let text = r#"
            [notify]
            command = ["notify-send", "wf: {title}", "{kind} on #{item}: {what} {other}"]
        "#;
        let config = MachineConfig::parse(text).ok().unwrap().notify;
        assert_eq!(
            config.command_for(&notice).unwrap(),
            [
                "notify-send",
                "wf: Says {what}",
                "blocker on #k3f7ab: answer it {other}"
            ]
        );
        assert!(MachineConfig::parse("[notify]\ncommand = []\n").is_err());
        assert!(MachineConfig::parse("[notify]\nshell = \"x\"\n").is_err());
    }

    #[test]
    fn storage_has_a_budget_for_caches_inside_the_worktrees() {
        assert_eq!(MachineConfig::parse("").ok().unwrap().storage, None);
        let text = "[storage]\nbudget = \"20GB\"\ncaches = [\"target\", \"web/node_modules\"]\n";
        assert_eq!(
            MachineConfig::parse(text).ok().unwrap().storage,
            Some(StorageConfig {
                budget: ByteSize::gb(20),
                caches: vec!["target".into(), "web/node_modules".into()],
            })
        );
        for text in [
            "[storage]\nbudget = \"20GB\"\ncaches = [\"/tmp/target\"]",
            "[storage]\nbudget = \"20GB\"\ncaches = [\"../target\"]",
            "[storage]\nbudget = \"20GB\"\ncaches = [\"target/../..\"]",
            "[storage]\nbudget = \"20GB\"\ncaches = [\"\"]",
            "[storage]\nbudget = \"twenty\"\ncaches = [\"target\"]",
            "[storage]\ncaches = [\"target\"]",
        ] {
            assert!(MachineConfig::parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn the_longest_a_check_runs_is_capped_by_the_machine() {
        let check = |timeout: u64| CheckConfig {
            name: wf_core::name::CommandName::new("test").unwrap(),
            run: vec!["true".to_string()],
            timeout: Duration::from_secs(timeout * 60),
        };
        let uncapped = MachineConfig::parse("").ok().unwrap().check;
        assert_eq!(uncapped.timeout(&check(30)), Duration::from_secs(30 * 60));
        let capped = MachineConfig::parse("[check]\nmax_timeout = \"10m\"\n")
            .ok()
            .unwrap()
            .check;
        assert_eq!(capped.timeout(&check(30)), Duration::from_secs(10 * 60));
        assert_eq!(capped.timeout(&check(5)), Duration::from_secs(5 * 60));
        assert!(MachineConfig::parse("[check]\nmax_timeout = \"soon\"\n").is_err());
        assert!(MachineConfig::parse("[check]\ntimeout = \"10m\"\n").is_err());
    }

    #[test]
    fn checks_run_in_the_sandbox_of_agents_unless_they_have_their_own() {
        let config = |text: &str| MachineConfig::parse(text).ok().unwrap();
        let fence = SandboxConfig::Fence(FenceConfig::default());
        assert_eq!(config("").check_sandbox(), &fence);
        assert_eq!(
            config("[sandbox]\nkind = \"off\"\n").check_sandbox(),
            &SandboxConfig::Off {}
        );
        assert_eq!(
            config("[check]\nsandbox = { kind = \"off\" }\n").check_sandbox(),
            &SandboxConfig::Off {}
        );
        let own = config(
            "[sandbox]\nkind = \"off\"\n[check.sandbox]\nkind = \"fence\"\nallow_write = [\"~/cache\"]\n",
        );
        let SandboxConfig::Fence(check) = own.check_sandbox() else {
            panic!("a fence sandbox");
        };
        assert!(!check.allow_write[0].starts_with("~"), "expanded");
        assert!(MachineConfig::parse("[check]\nsandbox = { kind = \"none\" }\n").is_err());
    }

    #[test]
    fn an_agent_gets_the_environment_and_sandbox_of_every_agent_and_its_own() {
        let text = r#"
            [environment]
            inherit = ["ANTHROPIC_API_KEY"]
            set = { CARGO_TARGET_DIR = "~/.cache/target", NOTE = "$$HOME is ${HOME}" }

            [sandbox]
            kind = "fence"
            allow_write = ["$CACHE/target"]

            [agent.coder]
            environment = { inherit = ["SSH_AUTH_SOCK"], set = { CARGO_TARGET_DIR = "target" } }
            sandbox = { allow_domains = ["docs.rs"] }
        "#;
        let variable = |name: &str| match name {
            "HOME" => Some("/home/me".to_string()),
            "CACHE" => Some("/var/cache".to_string()),
            _ => None,
        };
        let config = MachineConfig::parse_in(text, &variable).ok().unwrap();
        let (coder, reviewer) = (
            AgentName::new("coder").unwrap(),
            AgentName::new("reviewer").unwrap(),
        );

        let every = config.environment_of(&reviewer);
        assert_eq!(every.inherit, ["ANTHROPIC_API_KEY"]);
        assert_eq!(every.set["CARGO_TARGET_DIR"], "/home/me/.cache/target");
        assert_eq!(every.set["NOTE"], "$HOME is /home/me");
        let own = config.environment_of(&coder);
        assert_eq!(own.inherit, ["ANTHROPIC_API_KEY", "SSH_AUTH_SOCK"]);
        assert_eq!(own.set["CARGO_TARGET_DIR"], "target");

        let SandboxConfig::Fence(fence) = config.sandbox_of(&coder) else {
            panic!("a fence sandbox");
        };
        assert_eq!(fence.allow_write, [PathBuf::from("/var/cache/target")]);
        assert_eq!(fence.allow_domains, ["docs.rs"]);
        assert_eq!(
            config.sandbox_of(&reviewer),
            SandboxConfig::Fence(FenceConfig {
                allow_write: vec!["/var/cache/target".into()],
                allow_domains: vec![],
            })
        );
    }

    #[test]
    fn bad_environments_are_refused() {
        for text in [
            "[environment]\nset = { PATH = \"/bin\" }",
            "[environment]\ninherit = [\"WF_SESSION\"]",
            "[environment]\nset = { GIT_CONFIG_KEY_0 = \"x\" }",
            "[environment]\nset = { \"A=B\" = \"x\" }",
            "[environment]\nset = { CACHE = \"$NOT_SET_ANYWHERE/x\" }",
            "[environment]\npass = []",
            "[agent.coder.environment]\ninherit = [\"GIT_AUTHOR_NAME\"]",
            "[sandbox]\nkind = \"off\"\n[agent.coder.sandbox]\nallow_write = [\"/x\"]",
            "[agent.coder.sandbox]\nkind = \"off\"",
        ] {
            assert!(MachineConfig::parse_in(text, &|_| None).is_err(), "{text}");
        }
    }

    #[test]
    fn deciders_keep_their_order() {
        let text = r#"
            [permissions]
            guidance = "no network"

            [[permissions.decider]]
            kind = "model"
            url = "https://openrouter.ai/api/v1/systemone"
            model = "typesafe/jev-1.13"
            api_key_env = "OPENROUTER_API_KEY"
            question = { noul = { allow_above = 0.95, deny_below = 0.05 } }

            [[permissions.decider]]
            kind = "rules"

            [[permissions.decider]]
            kind = "hook"
            command = ["my-judge", "--strict"]
            timeout = "5s"

            [[permissions.decider]]
            kind = "agent"
            agent = "judge"
        "#;
        let config = MachineConfig::parse(text).ok().unwrap().permissions;
        assert_eq!(config.guidance.as_deref(), Some("no network"));
        let names: Vec<String> = config
            .deciders
            .iter()
            .map(|decider| decider.decider().to_string())
            .collect();
        assert_eq!(
            names,
            [
                "model typesafe/jev-1.13",
                "rules",
                "hook my-judge",
                "agent judge"
            ]
        );
        assert!(matches!(
            config.deciders[2],
            DeciderConfig::Hook { timeout, .. } if timeout == Duration::from_secs(5)
        ));
    }

    #[test]
    fn bad_deciders_are_refused() {
        for text in [
            "[[permissions.decider]]\nkind = \"hook\"\ncommand = []",
            "[[permissions.decider]]\nkind = \"shell\"",
            "[[permissions.decider]]\nkind = \"rules\"\nextra = 1",
            "[[permissions.decider]]\nkind = \"model\"\nurl = \"x\"\nmodel = \"m\"\napi_key_env = \"K\"\nquestion = { choice = { min_confidence = 0.5 } }",
            "[[permissions.decider]]\nkind = \"model\"\nurl = \"https://x\"\nmodel = \"m\"\napi_key_env = \"K\"\nquestion = { noul = { allow_above = 0.2, deny_below = 0.8 } }",
            "[[permissions.decider]]\nkind = \"model\"\nurl = \"https://x\"\nmodel = \"m\"\napi_key_env = \"K\"\nquestion = { choice = { min_confidence = 2 } }",
        ] {
            assert!(MachineConfig::parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn the_theme_is_read_whatever_is_wrong_elsewhere() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[notify]\ncommand = []\n\n[theme]\nid = \"red\"\n").unwrap();
        assert!(MachineConfig::load(&path).is_err());
        assert!(ThemeConfig::load(&path).unwrap().id.is_some());
        assert_eq!(
            ThemeConfig::load(&dir.path().join("missing.toml")).unwrap(),
            ThemeConfig::default()
        );
    }
}

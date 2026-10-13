//! The config of this machine, `$XDG_CONFIG_HOME/workforce/config.toml`: what only makes sense
//! here, such as programs to run and model APIs to call. It is never part of the signed policy,
//! because policy that syncs must not make other machines run programs or call paid APIs.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Deserializer};
use url::Url;
use wf_core::autonomy::PermissionMode;
use wf_core::name::AgentName;

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
}

/// How the commits of agents are made on this machine.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitsConfig {
    #[serde(default)]
    pub agent_signing: AgentSigning,
}

/// Whether agents sign their commits. The merge commit the daemon makes of approved work follows
/// the git config either way, so what lands on the target is signed when signing is configured.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentSigning {
    /// Agents commit without signing, whatever the git config says, so that they never wait for
    /// a key the principal has to unlock or approve.
    #[default]
    Unsigned,
    /// The git config decides, as for the principal's own commits.
    AsConfigured,
}

impl AgentSigning {
    /// The environment that makes git in the agent's processes sign as configured here.
    pub fn git_environment(self) -> Vec<(&'static str, &'static str)> {
        match self {
            AgentSigning::Unsigned => vec![
                ("GIT_CONFIG_COUNT", "1"),
                ("GIT_CONFIG_KEY_0", "commit.gpgsign"),
                ("GIT_CONFIG_VALUE_0", "false"),
            ],
            AgentSigning::AsConfigured => Vec::new(),
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
/// denied. What this adds is on top of that.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FenceConfig {
    /// More paths the agent may write to.
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
        #[serde(default = "default_timeout", deserialize_with = "duration")]
        timeout: Duration,
    },
    /// A decision model behind the System One API.
    Model {
        url: Url,
        model: String,
        /// The environment variable that holds the API key.
        api_key_env: String,
        question: QuestionConfig,
        #[serde(default = "default_timeout", deserialize_with = "duration")]
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
    /// How the decider is named in transcripts and in the inbox.
    pub fn name(&self) -> String {
        match self {
            DeciderConfig::Rules {} => "rules".to_string(),
            DeciderConfig::Hook { command, .. } => {
                format!("hook {}", command.first().map_or("", String::as_str))
            }
            DeciderConfig::Model { model, .. } => format!("model {model}"),
            DeciderConfig::Agent { agent, .. } => format!("agent {agent}"),
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

fn duration<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
    let text = String::deserialize(deserializer)?;
    humantime::parse_duration(&text).map_err(serde::de::Error::custom)
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
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(source) => {
                return Err(MachineConfigError::Read {
                    path: path.to_path_buf(),
                    source,
                });
            }
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
        let config: MachineConfig = toml::from_str(text).map_err(ParseError::Toml)?;
        for decider in &config.permissions.deciders {
            decider.check().map_err(ParseError::Invalid)?;
        }
        Ok(config)
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
    fn agents_commit_unsigned_unless_the_config_says_otherwise() {
        let default = MachineConfig::parse("").ok().unwrap().commits.agent_signing;
        assert_eq!(default, AgentSigning::Unsigned);
        assert!(
            default
                .git_environment()
                .contains(&("GIT_CONFIG_KEY_0", "commit.gpgsign"))
        );
        let configured = MachineConfig::parse("[commits]\nagent_signing = \"as_configured\"\n")
            .ok()
            .unwrap()
            .commits
            .agent_signing;
        assert_eq!(configured, AgentSigning::AsConfigured);
        assert!(configured.git_environment().is_empty());
        assert!(MachineConfig::parse("[commits]\nagent_signing = \"always\"\n").is_err());
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
        let names: Vec<String> = config.deciders.iter().map(DeciderConfig::name).collect();
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
}

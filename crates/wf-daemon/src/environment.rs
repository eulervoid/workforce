//! The environment an agent runs with: what wf sets, what the machine config names, and the few
//! variables every program expects. Nothing else of the daemon's environment reaches it.

use std::collections::BTreeSet;

use wf_harness::AcpAgentConfig;

use crate::machine_config::EnvironmentConfig;

/// Variables of the daemon's environment that every agent keeps.
const KEPT: &[&str] = &[
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "PATH",
    "TMPDIR",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_STATE_HOME",
];

/// `agent` with the variables `config` sets, started through `env -u` so that the variables of
/// the daemon's environment (`daemon`) it does not keep are removed. Only their names are on the
/// command line; the values of what is set stay out of it.
pub(crate) fn confine(
    agent: AcpAgentConfig,
    config: &EnvironmentConfig,
    daemon: impl IntoIterator<Item = String>,
) -> AcpAgentConfig {
    let agent = agent.envs(config.set.clone());
    let removed: BTreeSet<String> = daemon
        .into_iter()
        .filter(|name| {
            !KEPT.contains(&name.as_str())
                && !config.inherit.contains(name)
                && !agent.environment().contains_key(name)
        })
        .collect();
    let mut confined = AcpAgentConfig::new("/usr/bin/env");
    for name in removed {
        confined = confined.arg("-u").arg(name);
    }
    confined
        .arg("--")
        .arg(agent.command().to_string_lossy())
        .args(agent.arguments())
        .envs(agent.environment())
}

/// The names of the variables of the daemon's environment. A name that is not UTF-8 cannot be
/// passed to `env -u`, and no program wf starts sets one.
pub(crate) fn daemon_variables() -> impl Iterator<Item = String> {
    std::env::vars_os().filter_map(|(name, _)| name.into_string().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_what_is_kept_inherited_or_set_reaches_the_agent() {
        let config = EnvironmentConfig {
            inherit: vec!["ANTHROPIC_API_KEY".to_string()],
            set: [("CARGO_TARGET_DIR".to_string(), "/cache/target".to_string())].into(),
        };
        let agent = AcpAgentConfig::new("claude-agent-acp")
            .arg("--flag")
            .env("WF_SESSION", "s1");
        let daemon = [
            "HOME",
            "OPENROUTER_API_KEY",
            "ANTHROPIC_API_KEY",
            "CARGO_TARGET_DIR",
            "WF_SESSION",
            "WF_LOG",
        ]
        .map(String::from);

        let confined = confine(agent, &config, daemon);

        assert_eq!(confined.command().to_str(), Some("/usr/bin/env"));
        assert_eq!(
            confined.arguments(),
            [
                "-u",
                "OPENROUTER_API_KEY",
                "-u",
                "WF_LOG",
                "--",
                "claude-agent-acp",
                "--flag"
            ]
        );
        assert_eq!(confined.environment()["WF_SESSION"], "s1");
        assert_eq!(confined.environment()["CARGO_TARGET_DIR"], "/cache/target");
        assert!(!confined.environment().contains_key("ANTHROPIC_API_KEY"));
    }
}

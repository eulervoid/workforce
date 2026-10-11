//! Runs an agent in Fence, so that it can only write in its worktree and where it has to.

use std::path::Path;

use serde_json::json;
use wf_harness::AcpAgentConfig;

use crate::machine_config::SandboxConfig;

/// What the agent is let to touch besides its worktree.
pub(crate) struct Access<'a> {
    pub worktree: &'a Path,
    /// The repository of the worktree: commits write their objects and refs there.
    pub repository: &'a Path,
    /// The socket of the daemon, which `wf` in the agent talks to.
    pub socket: &'a Path,
}

/// `agent`, started by Fence in the worktree. The settings Fence reads are written to `settings`.
pub(crate) fn wrap(
    config: &SandboxConfig,
    access: &Access<'_>,
    agent: &AcpAgentConfig,
    settings: &Path,
) -> std::io::Result<AcpAgentConfig> {
    let git_directory = access.repository.join(".git");
    let writable: Vec<&Path> = [access.worktree, git_directory.as_path()]
        .into_iter()
        .chain([access.socket])
        .chain(config.allow_write.iter().map(|path| path.as_path()))
        .collect();
    let document = json!({
        "extends": "code",
        "filesystem": { "allowWrite": writable },
        "network": {
            "allowedDomains": config.allow_domains,
            "allowUnixSockets": [access.socket],
        },
    });
    if let Some(parent) = settings.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(settings, serde_json::to_vec_pretty(&document)?)?;
    // Fence allows writes to "." of the directory it runs in, so it has to start in the worktree.
    Ok(AcpAgentConfig::new("sh")
        .arg("-c")
        .arg(r#"cd "$1" && shift && exec "$@""#)
        .arg("sh")
        .arg(access.worktree.to_string_lossy())
        .arg("fence")
        .arg("--settings")
        .arg(settings.to_string_lossy())
        .arg("--")
        .arg(agent.command().to_string_lossy())
        .args(agent.arguments())
        .envs(agent.environment()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_agent_starts_in_the_worktree_under_fence() {
        let directory = tempfile::tempdir().unwrap();
        let settings = directory.path().join("sandbox/session.json");
        let agent = AcpAgentConfig::new("claude-agent-acp")
            .arg("--flag")
            .env("WF_SESSION", "s1");
        let wrapped = wrap(
            &SandboxConfig {
                allow_write: vec!["/opt/cache".into()],
                allow_domains: vec!["example.com".to_string()],
            },
            &Access {
                worktree: Path::new("/work/tree"),
                repository: Path::new("/work/repo"),
                socket: Path::new("/run/wf/daemon.sock"),
            },
            &agent,
            &settings,
        )
        .unwrap();

        assert_eq!(wrapped.command(), Path::new("sh"));
        assert_eq!(
            wrapped.arguments()[3..],
            [
                "/work/tree",
                "fence",
                "--settings",
                settings.to_str().unwrap(),
                "--",
                "claude-agent-acp",
                "--flag"
            ]
        );
        assert_eq!(wrapped.environment()["WF_SESSION"], "s1");

        let written: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&settings).unwrap()).unwrap();
        assert_eq!(written["extends"], "code");
        assert_eq!(
            written["filesystem"]["allowWrite"],
            json!([
                "/work/tree",
                "/work/repo/.git",
                "/run/wf/daemon.sock",
                "/opt/cache"
            ])
        );
        assert_eq!(written["network"]["allowedDomains"], json!(["example.com"]));
        assert_eq!(
            written["network"]["allowUnixSockets"],
            json!(["/run/wf/daemon.sock"])
        );
    }
}

//! Runs an agent in Fence, so that it can only write in its own directory and where it has to.
//!
//! An agent works in a repository of its own, which it can write whole; the principal's
//! repository is not writable at all, and the daemon copies the commits an agent submits into it.
//! So the profile has no rule about the parts of a `.git`.
//!
//! The profile is Fence's `code` template (Fence 0.1.67) with these changes, written out here
//! because a profile that extends a template can only add to its lists:
//! - no writing to all of `~/.config` and `~/.local/share`, where wf keeps its config, keys,
//!   stores and the repositories of other items;
//! - no deny on writing `**/*.key`, which kept `wf init` and the test suites from writing keys in
//!   their temporary directories; the keys wf keeps are denied where they are instead;
//! - the paths of wf that would let an agent act outside its sandbox are denied;
//! - the config of the harnesses, and cargo's registry, are not writable: both run code with the
//!   principal's privileges the next time the principal runs the harness or builds. Agents get a
//!   `CARGO_HOME` of their own instead.

use std::path::{Path, PathBuf};

use serde_json::json;
use wf_harness::AcpAgentConfig;

use crate::data_dir::DataDir;
use crate::machine_config::FenceConfig;

/// What the agent is let to touch besides its directory, and what of wf it must not.
pub struct Access<'a> {
    /// The repository or directory the agent works in, which it can write whole.
    pub directory: &'a Path,
    /// The socket of the daemon, which `wf` in the agent talks to.
    pub socket: &'a Path,
    /// wf's data directory, which holds the keys of the device and of the principals.
    pub data_dir: &'a DataDir,
    /// The machine config, which says whether agents run in a sandbox at all.
    pub machine_config: Option<&'a Path>,
}

/// Whether `fence` is a file in one of the directories of `PATH`.
pub(crate) fn fence_installed() -> bool {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .any(|directory| directory.join("fence").is_file())
}

/// `agent`, started by Fence in the directory. The sandbox profile Fence reads is written to `profile_file`.
pub(crate) fn wrap(
    config: &FenceConfig,
    access: &Access<'_>,
    agent: &AcpAgentConfig,
    profile_file: &Path,
) -> std::io::Result<AcpAgentConfig> {
    if let Some(parent) = profile_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(
        profile_file,
        serde_json::to_vec_pretty(&profile(config, access))?,
    )?;
    // Made here, as Fence leaves out a path to write to that does not exist.
    let cargo_home = access.data_dir.agent_cargo_home();
    std::fs::create_dir_all(&cargo_home)?;
    // Fence allows writes to "." of the directory it runs in, so it has to start there.
    Ok(AcpAgentConfig::new("sh")
        .arg("-c")
        .arg(r#"cd "$1" && shift && exec "$@""#)
        .arg("sh")
        .arg(access.directory.to_string_lossy())
        .arg("fence")
        .arg("--settings")
        .arg(profile_file.to_string_lossy())
        .arg("--")
        .arg(agent.command().to_string_lossy())
        .args(agent.arguments())
        .envs(agent.environment())
        .env(CARGO_HOME, cargo_home.to_string_lossy()))
}

/// Set by wf in Fence, so the machine config must not set or inherit it there.
pub(crate) const CARGO_HOME: &str = "CARGO_HOME";

/// The sandbox profile Fence runs the agent with.
fn profile(config: &FenceConfig, access: &Access<'_>) -> serde_json::Value {
    let writable: Vec<PathBuf> = [
        access.directory.to_path_buf(),
        access.socket.to_path_buf(),
        access.data_dir.agent_cargo_home(),
    ]
    .into_iter()
    .chain(config.allow_write.iter().cloned())
    .collect();

    // With these an agent signs as the device or the principal, has the daemon act for the
    // principal, or turns its sandbox off.
    let mut secrets = access.data_dir.secrets();
    secrets.extend(
        access
            .machine_config
            .and_then(Path::parent)
            .map(Path::to_path_buf),
    );
    // With this an agent has git run its code, outside the sandbox, the next time the principal
    // runs git. Fence already denies `~/.gitconfig`.
    let git_config = [PathBuf::from("~/.config/git")];

    let paths = |paths: &[&str]| paths.iter().map(PathBuf::from).collect::<Vec<_>>();
    let deny_read: Vec<PathBuf> = [paths(DENY_READ), secrets.clone()].concat();
    let deny_write: Vec<PathBuf> = [
        paths(DENY_WRITE),
        paths(HARNESS_CONFIG),
        secrets,
        git_config.to_vec(),
    ]
    .concat();
    let allow_write: Vec<PathBuf> = [paths(ALLOW_WRITE), writable].concat();
    let allowed_domains: Vec<&str> = ALLOWED_DOMAINS
        .iter()
        .copied()
        .chain(config.allow_domains.iter().map(String::as_str))
        .collect();
    json!({
        "allowPty": true,
        "network": {
            "allowLocalBinding": true,
            "allowLocalOutbound": true,
            "allowedDomains": allowed_domains,
            "deniedDomains": DENIED_DOMAINS,
            "allowUnixSockets": [access.socket],
        },
        "filesystem": {
            "allowWrite": allow_write,
            "denyWrite": deny_write,
            "denyRead": deny_read,
        },
        "command": {
            "useDefaults": true,
            "deny": DENIED_COMMANDS,
        },
    })
}

const ALLOWED_DOMAINS: &[&str] = &[
    // LLM API providers
    "api.openai.com",
    "*.anthropic.com",
    "api.githubcopilot.com",
    "generativelanguage.googleapis.com",
    "api.mistral.ai",
    "api.cohere.ai",
    "api.together.xyz",
    "openrouter.ai",
    "api.morphllm.com",
    "api.minimax.io",
    "api.minimaxi.com",
    "*.amazonaws.com",
    // OpenCode
    "opencode.ai",
    "api.opencode.ai",
    // Amp
    "ampcode.com",
    "*.ampcode.com",
    // Factory CLI (droid)
    "*.factory.ai",
    "api.workos.com",
    // Cursor API
    "*.cursor.sh",
    // Charm
    "data.charm.land",
    "catwalk.charm.sh",
    // Copilot
    "*.githubcopilot.com",
    // Git hosting
    "github.com",
    "api.github.com",
    "raw.githubusercontent.com",
    "codeload.github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
    "gitlab.com",
    // Package registries
    "registry.npmjs.org",
    "*.npmjs.org",
    "registry.yarnpkg.com",
    "pypi.org",
    "files.pythonhosted.org",
    "crates.io",
    "static.crates.io",
    "index.crates.io",
    "proxy.golang.org",
    "sum.golang.org",
    "formulae.brew.sh",
    // Model registry
    "models.dev",
];

const DENIED_DOMAINS: &[&str] = &[
    // Cloud metadata APIs
    "169.254.169.254",
    "metadata.google.internal",
    "instance-data.ec2.internal",
    // Telemetry
    "statsig.anthropic.com",
    "*.sentry.io",
];

const ALLOW_WRITE: &[&str] = &[
    ".",
    "/tmp",
    "~/.cache/**",
    "~/.cache",
    // Claude Code
    "~/.claude*",
    "~/.claude/**",
    // Amp
    "~/.amp",
    // Codex
    "~/.codex/**",
    // Cursor
    "~/.cursor/**",
    // OpenCode
    "~/.opencode/**",
    "~/.local/state/**",
    // Gemini CLI
    "~/.gemini/**",
    // Factory CLI (droid)
    "~/.factory/**",
    // Pi
    "~/.pi/**",
    // Copilot
    "~/.copilot/**",
    // Kiro CLI
    "~/.kiro/**",
    "~/Library/Application Support/kiro-cli/**",
    // Package manager caches
    "~/.npm/_cacache",
    "~/.npm/_npx",
    "~/.bun/**",
    // Shell completion cache
    "~/.zcompdump*",
];

/// What of the harnesses' directories in [`ALLOW_WRITE`] the harness runs or follows when the
/// principal starts it, outside the sandbox: settings with hooks and MCP servers, commands, agents,
/// skills, plugins, instructions, the status line script, editor extensions and the harness's own
/// binary. Their state, sessions and auth stay writable. `~/.claude.json` holds Claude Code's state
/// and its user-scope MCP servers in one file, so it stays writable too, and so do the scripts
/// settings point to elsewhere in `~/.claude`.
const HARNESS_CONFIG: &[&str] = &[
    // Claude Code
    "~/.claude/settings.json",
    "~/.claude/settings.local.json",
    "~/.claude/CLAUDE.md",
    "~/.claude/hooks",
    "~/.claude/commands",
    "~/.claude/agents",
    "~/.claude/skills",
    "~/.claude/plugins",
    "~/.claude/output-styles",
    "~/.claude/statusline*",
    "~/.claude/local",
    // Amp
    "~/.amp/bin",
    // Codex
    "~/.codex/config.toml",
    "~/.codex/hooks.json",
    "~/.codex/AGENTS.md",
    "~/.codex/prompts",
    "~/.codex/skills",
    "~/.codex/rules",
    // Cursor
    "~/.cursor/cli-config.json",
    "~/.cursor/mcp.json",
    "~/.cursor/rules",
    "~/.cursor/commands",
    "~/.cursor/hooks.json",
    "~/.cursor/extensions",
    // OpenCode, whose config is in `~/.config`
    "~/.opencode/bin",
    // Gemini CLI
    "~/.gemini/settings.json",
    "~/.gemini/GEMINI.md",
    "~/.gemini/commands",
    "~/.gemini/extensions",
    // Factory CLI (droid)
    "~/.factory/settings.json",
    "~/.factory/mcp.json",
    "~/.factory/AGENTS.md",
    "~/.factory/droids",
    "~/.factory/commands",
    "~/.factory/hooks",
    "~/.factory/skills",
    // Pi
    "~/.pi/agent/settings.json",
    "~/.pi/agent/AGENTS.md",
    "~/.pi/agent/extensions",
    "~/.pi/agent/skills",
    "~/.pi/agent/prompts",
    // Copilot
    "~/.copilot/mcp-config.json",
    "~/.copilot/agents",
    "~/.copilot/skills",
    "~/.copilot/hooks",
    "~/.copilot/pkg",
    // Kiro CLI
    "~/.kiro/settings",
    "~/.kiro/agents",
    "~/.kiro/steering",
    "~/.kiro/prompts",
    "~/.kiro/hooks",
    "~/.kiro/extensions",
];

const DENY_WRITE: &[&str] = &["**/.env", "**/.env.*", "**/*.pem", "**/*.p12", "**/*.pfx"];

const DENY_READ: &[&str] = &[
    // SSH private keys and config
    "~/.ssh/id_*",
    "~/.ssh/config",
    "~/.ssh/*.pem",
    // GPG keys
    "~/.gnupg/**",
    // Cloud provider credentials
    "~/.aws/**",
    "~/.config/gcloud/**",
    "~/.kube/**",
    // Docker config, which may hold registry credentials
    "~/.docker/**",
    // Package manager tokens
    "~/.pypirc",
    "~/.netrc",
    "~/.git-credentials",
    "~/.cargo/credentials",
    "~/.cargo/credentials.toml",
];

const DENIED_COMMANDS: &[&str] = &[
    // Git commands that change remote or shared state. The agent's repository is its own, so
    // `reset`, `clean`, `checkout --` and `rebase` only change what it can write anyway.
    "git push",
    "git merge",
    // Publishing packages
    "npm publish",
    "pnpm publish",
    "yarn publish",
    "cargo publish",
    "twine upload",
    "gem push",
    // Privilege escalation
    "sudo",
    // GitHub CLI commands that change remote state
    "gh pr create",
    "gh pr merge",
    "gh pr close",
    "gh pr reopen",
    "gh pr review",
    "gh pr comment",
    "gh release create",
    "gh release delete",
    "gh repo create",
    "gh repo fork",
    "gh repo delete",
    "gh issue create",
    "gh issue close",
    "gh issue comment",
    "gh gist create",
    "gh workflow run",
    "gh api",
    "gh auth login",
    "gh secret set",
    "gh secret delete",
    "gh variable set",
    "gh variable delete",
];

#[cfg(test)]
mod tests {
    use super::*;

    const DATA: &str = "/home/ada/.local/share/workforce";
    const REPOSITORY: &str = "/home/ada/.local/share/workforce/w1/repos/k3f9";

    fn access() -> Access<'static> {
        Access {
            directory: Path::new(REPOSITORY),
            socket: Path::new("/run/wf/daemon.sock"),
            data_dir: Box::leak(Box::new(DataDir::new(PathBuf::from(DATA)))),
            machine_config: Some(Path::new("/home/ada/.config/workforce/config.toml")),
        }
    }

    fn config() -> FenceConfig {
        FenceConfig {
            allow_write: vec!["/opt/cache".into()],
            allow_domains: vec!["example.com".to_string()],
        }
    }

    /// The patterns of `list` in the filesystem section of the profile.
    fn patterns(list: &str) -> Vec<String> {
        serde_json::from_value(profile(&config(), &access())["filesystem"][list].clone()).unwrap()
    }

    /// Whether a pattern of `list` covers `path`, as Fence reads it: a glob matches with `*` inside
    /// one directory, and a plain path covers everything below it.
    fn covers(list: &str, path: &str) -> bool {
        patterns(list).iter().any(|pattern| {
            if pattern.contains('*') {
                globset::GlobBuilder::new(pattern)
                    .literal_separator(true)
                    .build()
                    .unwrap()
                    .compile_matcher()
                    .is_match(path)
            } else {
                Path::new(path).starts_with(pattern)
            }
        })
    }

    #[test]
    fn the_agent_starts_in_its_repository_under_fence() {
        let directory = tempfile::tempdir().unwrap();
        let profile_file = directory.path().join("sandbox/session.json");
        let agent = AcpAgentConfig::new("claude-agent-acp")
            .arg("--flag")
            .env("WF_SESSION", "s1");
        let repository = directory.path().join("k3f9");
        std::fs::create_dir(&repository).unwrap();
        let data_dir = DataDir::new(directory.path().join("data"));
        let access = Access {
            directory: &repository,
            data_dir: &data_dir,
            ..access()
        };
        let wrapped = wrap(&config(), &access, &agent, &profile_file).unwrap();

        assert_eq!(wrapped.command(), Path::new("sh"));
        assert_eq!(
            wrapped.arguments()[3..],
            [
                repository.to_str().unwrap(),
                "fence",
                "--settings",
                profile_file.to_str().unwrap(),
                "--",
                "claude-agent-acp",
                "--flag"
            ]
        );
        assert_eq!(wrapped.environment()["WF_SESSION"], "s1");
        let cargo_home = data_dir.agent_cargo_home();
        assert_eq!(
            wrapped.environment()["CARGO_HOME"],
            cargo_home.to_str().unwrap()
        );
        assert!(cargo_home.is_dir());

        let written: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&profile_file).unwrap()).unwrap();
        assert_eq!(written, profile(&config(), &access));
    }

    #[test]
    fn the_agent_writes_its_repository_and_talks_to_the_daemon() {
        for path in [
            "/home/ada/.local/share/workforce/w1/repos/k3f9/src/main.rs",
            "/home/ada/.local/share/workforce/w1/repos/k3f9/.git/objects/ab/cdef",
            "/home/ada/.local/share/workforce/w1/repos/k3f9/.git/refs/heads/wf/k3f9",
            "/home/ada/.local/share/workforce/w1/repos/k3f9/.git/refs/heads/wf/k3f9.lock",
            "/home/ada/.local/share/workforce/w1/repos/k3f9/.git/packed-refs",
            "/home/ada/.local/share/workforce/w1/repos/k3f9/.git/config",
            "/home/ada/.local/share/workforce/w1/repos/k3f9/.git/HEAD",
            "/run/wf/daemon.sock",
            "/opt/cache/x",
        ] {
            assert!(covers("allowWrite", path), "{path} is writable");
            assert!(!covers("denyWrite", path), "{path} is not denied");
        }
        let profile = profile(&config(), &access());
        let domains = profile["network"]["allowedDomains"].as_array().unwrap();
        assert!(domains.contains(&json!("*.anthropic.com")));
        assert!(domains.contains(&json!("example.com")));
        assert_eq!(
            profile["network"]["allowUnixSockets"],
            json!(["/run/wf/daemon.sock"])
        );
    }

    #[test]
    fn the_keys_and_the_machine_config_are_out_of_reach() {
        for path in [
            "/home/ada/.local/share/workforce/keys/device.key",
            "/home/ada/.local/share/workforce/keys/device-ssh",
            "/home/ada/.local/share/workforce/keys/principal.token",
            "/home/ada/.local/share/workforce/w1/principal.key",
            "/home/ada/.config/workforce/config.toml",
        ] {
            assert!(covers("denyRead", path), "{path} is not readable");
            assert!(covers("denyWrite", path), "{path} is not writable");
        }
    }

    #[test]
    fn the_global_git_config_cannot_be_written() {
        assert!(covers("denyWrite", "~/.config/git/config"));
    }

    #[test]
    fn the_harnesses_config_cannot_be_written() {
        for path in [
            "~/.claude/settings.json",
            "~/.claude/hooks/notify.sh",
            "~/.claude/commands/deploy.md",
            "~/.claude/agents/helper.md",
            "~/.claude/skills/review/SKILL.md",
            "~/.claude/plugins/marketplaces/x/plugin.json",
            "~/.codex/config.toml",
            "~/.codex/rules/default.rules",
            "~/.claude/statusline.sh",
            "~/.claude/statusline-command.sh",
            "~/.claude/local/claude",
            "~/.amp/bin/amp",
            "~/.cursor/mcp.json",
            "~/.cursor/hooks.json",
            "~/.cursor/extensions/x/extension.js",
            "~/.opencode/bin/opencode",
            "~/.gemini/settings.json",
            "~/.gemini/extensions/x/gemini-extension.json",
            "~/.factory/settings.json",
            "~/.pi/agent/extensions/x.ts",
            "~/.copilot/mcp-config.json",
            "~/.copilot/pkg/universal/0.0.1/index.js",
            "~/.kiro/settings/mcp.json",
            "~/.kiro/extensions/x/extension.js",
        ] {
            assert!(covers("denyWrite", path), "{path} is not writable");
        }
    }

    #[test]
    fn the_harnesses_keep_their_state_sessions_and_auth() {
        for path in [
            "~/.claude.json",
            "~/.claude/.credentials.json",
            "~/.claude/projects/-work-repo/session.jsonl",
            "~/.claude/todos/x.json",
            "~/.codex/auth.json",
            "~/.codex/sessions/2026/10/21/rollout.jsonl",
            "~/.gemini/oauth_creds.json",
            "~/.gemini/tmp/x/chats/session.json",
            "~/.factory/sessions/x.jsonl",
            "~/.copilot/session-state/x.jsonl",
        ] {
            assert!(covers("allowWrite", path), "{path} is writable");
            assert!(!covers("denyWrite", path), "{path} is not denied");
        }
    }

    #[test]
    fn agents_build_with_a_cargo_home_of_their_own() {
        for path in [
            "~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/serde-1.0.0/build.rs",
            "~/.cargo/git/checkouts/x/build.rs",
            "~/.cargo/bin/cargo",
            "~/.cargo/config.toml",
        ] {
            assert!(!covers("allowWrite", path), "{path} is not writable");
        }
        assert!(covers(
            "allowWrite",
            "/home/ada/.local/share/workforce/cargo/registry/src/x/serde-1.0.0/build.rs"
        ));
    }

    #[test]
    fn the_principals_repository_and_the_other_repositories_cannot_be_written() {
        for path in [
            "/work/repo/.git/objects/ab/cdef",
            "/work/repo/.git/refs/heads/main",
            "/work/repo/.git/refs/heads/main.lock",
            "/work/repo/.git/refs/heads/wf/k3f9",
            "/work/repo/.git/logs/refs/heads/main",
            "/work/repo/.git/packed-refs",
            "/work/repo/.git/packed-refs.lock",
            "/work/repo/.git/HEAD",
            "/work/repo/.git/config",
            "/work/repo/.git/hooks/pre-commit",
            "/work/repo/.git/worktrees/other/HEAD",
            "/work/repo/src/main.rs",
            "/home/ada/.local/share/workforce/w1/repos/other/.git/refs/heads/wf/other",
            "/home/ada/.local/share/workforce/w1/repos/other/src/main.rs",
        ] {
            assert!(!covers("allowWrite", path), "{path} is not writable");
        }
    }

    #[test]
    fn wf_and_the_other_items_are_not_writable() {
        for path in [
            "~/.config/workforce/config.toml",
            "~/.local/share/workforce/w1/store.redb",
            "~/.local/share/workforce/w1/repos/other/src/main.rs",
            "~/.local/share/workforce/bin/wf",
        ] {
            assert!(!covers("allowWrite", path), "{path} is not writable");
        }
    }

    #[test]
    fn keys_in_temporary_directories_stay_writable() {
        for path in [
            "/tmp/.tmpAbc/data/workforce/keys/device.key",
            "/tmp/.tmpAbc/w1/principal.key",
            "/var/folders/xy/T/.tmpAbc/keys/device.key",
        ] {
            assert!(!covers("denyWrite", path), "{path} is not denied");
            assert!(!covers("denyRead", path), "{path} is readable");
        }
        assert!(covers("allowWrite", "/tmp/.tmpAbc/keys/device.key"));
    }
}

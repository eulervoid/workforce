//! Runs an agent in Fence, so that it can only write in its worktree and where it has to.
//!
//! The profile is Fence's `code` template (Fence 0.1.67) with three changes, written out here
//! because a profile that extends a template can only add to its lists:
//! - no writing to all of `~/.config` and `~/.local/share`, where wf keeps its config, keys,
//!   stores and the worktrees of other items;
//! - no deny on writing `**/*.key`, which kept `wf init` and the test suites from writing keys in
//!   their temporary directories; the keys wf keeps are denied where they are instead;
//! - the paths of wf and git that would let an agent act outside its sandbox are denied.

use std::path::{Path, PathBuf};

use serde_json::json;
use wf_harness::AcpAgentConfig;

use crate::machine_config::FenceConfig;

/// What the agent is let to touch besides its worktree, and what of wf it must not.
pub(crate) struct Access<'a> {
    pub worktree: &'a Path,
    /// The repository of the worktree: commits write their objects and refs there.
    pub repository: &'a Path,
    /// The socket of the daemon, which `wf` in the agent talks to.
    pub socket: &'a Path,
    /// wf's data directory, which holds the keys of the device and of the principals.
    pub data_dir: &'a Path,
    /// The machine config, which says whether agents run in a sandbox at all.
    pub machine_config: Option<&'a Path>,
}

/// Whether `fence` is a file in one of the directories of `PATH`.
pub(crate) fn fence_installed() -> bool {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .any(|directory| directory.join("fence").is_file())
}

/// `agent`, started by Fence in the worktree. The settings Fence reads are written to `settings`.
pub(crate) fn wrap(
    config: &FenceConfig,
    access: &Access<'_>,
    agent: &AcpAgentConfig,
    settings: &Path,
) -> std::io::Result<AcpAgentConfig> {
    if let Some(parent) = settings.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(
        settings,
        serde_json::to_vec_pretty(&profile(config, access))?,
    )?;
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

/// The settings Fence runs the agent with.
fn profile(config: &FenceConfig, access: &Access<'_>) -> serde_json::Value {
    let git_directory = access.repository.join(".git");
    let writable: Vec<PathBuf> = [
        access.worktree.to_path_buf(),
        git_directory.clone(),
        access.socket.to_path_buf(),
    ]
    .into_iter()
    .chain(config.allow_write.iter().cloned())
    .collect();

    // With these an agent signs as the device or the principal, or turns its sandbox off.
    let mut secrets = vec![
        access.data_dir.join("keys"),
        access.data_dir.join("*").join("principal.key"),
    ];
    secrets.extend(
        access
            .machine_config
            .and_then(Path::parent)
            .map(Path::to_path_buf),
    );
    // With these an agent has git run its code, outside the sandbox, the next time the daemon or
    // the principal runs git. Fence already denies `~/.gitconfig`.
    let git_config = [
        git_directory.join("config"),
        git_directory.join("hooks"),
        git_directory.join("worktrees").join("*").join("commondir"),
        access.worktree.join(".git"),
        PathBuf::from("~/.config/git"),
    ];

    let paths = |paths: &[&str]| paths.iter().map(PathBuf::from).collect::<Vec<_>>();
    let deny_read: Vec<PathBuf> = [paths(DENY_READ), secrets.clone()].concat();
    let deny_write: Vec<PathBuf> = [paths(DENY_WRITE), secrets, git_config.to_vec()].concat();
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
    "~/.cargo/registry/**",
    "~/.cargo/git/**",
    "~/.cargo/.package-cache",
    // Shell completion cache
    "~/.zcompdump*",
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
    // Git commands that change remote or shared state
    "git push",
    "git reset",
    "git clean",
    "git checkout --",
    "git rebase",
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
    const WORKTREE: &str = "/home/ada/.local/share/workforce/w1/worktrees/k3f9";

    fn access() -> Access<'static> {
        Access {
            worktree: Path::new(WORKTREE),
            repository: Path::new("/work/repo"),
            socket: Path::new("/run/wf/daemon.sock"),
            data_dir: Path::new(DATA),
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
    fn the_agent_starts_in_the_worktree_under_fence() {
        let directory = tempfile::tempdir().unwrap();
        let settings = directory.path().join("sandbox/session.json");
        let agent = AcpAgentConfig::new("claude-agent-acp")
            .arg("--flag")
            .env("WF_SESSION", "s1");
        let wrapped = wrap(&config(), &access(), &agent, &settings).unwrap();

        assert_eq!(wrapped.command(), Path::new("sh"));
        assert_eq!(
            wrapped.arguments()[3..],
            [
                WORKTREE,
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
        assert_eq!(written, profile(&config(), &access()));
    }

    #[test]
    fn the_agent_writes_its_worktree_commits_and_talks_to_the_daemon() {
        for path in [
            "/home/ada/.local/share/workforce/w1/worktrees/k3f9/src/main.rs",
            "/work/repo/.git/objects/ab/cdef",
            "/work/repo/.git/refs/heads/wf/k3f9",
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
            "/home/ada/.local/share/workforce/w1/principal.key",
            "/home/ada/.config/workforce/config.toml",
        ] {
            assert!(covers("denyRead", path), "{path} is not readable");
            assert!(covers("denyWrite", path), "{path} is not writable");
        }
    }

    #[test]
    fn git_cannot_be_made_to_run_code() {
        for path in [
            "/work/repo/.git/config",
            "/work/repo/.git/hooks/pre-commit",
            "/work/repo/.git/worktrees/k3f9/commondir",
            "/home/ada/.local/share/workforce/w1/worktrees/k3f9/.git",
            "~/.config/git/config",
        ] {
            assert!(covers("denyWrite", path), "{path} is not writable");
        }
    }

    #[test]
    fn wf_and_the_other_items_are_not_writable() {
        for path in [
            "~/.config/workforce/config.toml",
            "~/.local/share/workforce/w1/store.redb",
            "~/.local/share/workforce/w1/worktrees/other/src/main.rs",
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

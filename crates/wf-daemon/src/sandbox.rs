//! Runs an agent in Fence, so that it can only write in its own directory and where it has to.
//!
//! An agent works in a repository of its own, which it can write whole; the principal's
//! repository is not writable at all, and the daemon copies the commits an agent submits into it.
//! So the profile has no rule about the parts of a `.git`.
//!
//! The profile is Fence's `code` template (Fence 0.1.67) with these changes, written out here
//! because a profile that extends a template can only add to its lists:
//! - nothing in the principal's home is writable: not `~/.config` and `~/.local/share`, where wf
//!   keeps its config, keys, stores and the repositories of other items, and not the caches of
//!   package managers or the directories of the harnesses either, as each of them holds code the
//!   principal runs later, outside the sandbox. The harness wf starts keeps its state in a
//!   directory of wf's instead ([`DataDir::harness_home`]); anything more is the principal's to
//!   allow, with `allow_write` in the machine config;
//! - no deny on writing `**/*.key`, which kept `wf init` and the test suites from writing keys in
//!   their temporary directories; the keys wf keeps are denied where they are instead;
//! - the paths of wf that would let an agent act outside its sandbox are denied;
//! - key files are denied below the agent's directory and temporary files, not everywhere: Fence
//!   writes every deny after every allow, so a deny of `**/*.pem` would keep cargo from unpacking
//!   a crate that ships one as a test fixture into a `CARGO_HOME` the principal allows.

use std::path::{Path, PathBuf};

use serde_json::json;
use wf_harness::{AcpAgentConfig, KnownHarness};

use crate::data_dir::DataDir;
use crate::machine_config::{EnvironmentConfig, FenceConfig};

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
    /// The harness wf starts, which keeps its state in its directory in `data_dir`; none for a
    /// check, which starts no harness, and for a harness wf does not know.
    pub harness: Option<KnownHarness>,
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
    // Fence allows writes to "." of the directory it runs in, so it has to start there.
    let mut wrapped = AcpAgentConfig::new("sh")
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
        .envs(agent.environment());
    if let Some(harness) = access.harness {
        let home = access.data_dir.harness_home(harness);
        // Made here, as Fence leaves out a path to write to that does not exist.
        std::fs::create_dir_all(&home)?;
        for (name, value) in harness_variables(harness, &home) {
            wrapped = wrapped.env(name, value);
        }
    }
    Ok(wrapped)
}

/// The variables that have `harness` keep its config, state and sessions in `home`. The machine
/// config may set them otherwise, such as to the principal's own directory of the harness, which
/// it then has to allow writing to.
fn harness_variables(harness: KnownHarness, home: &Path) -> Vec<(&'static str, String)> {
    let below = |name: &str| home.join(name).to_string_lossy().into_owned();
    let home = home.to_string_lossy().into_owned();
    match harness {
        KnownHarness::ClaudeCode => {
            let mut variables = vec![("CLAUDE_CONFIG_DIR", home)];
            // With its config directory moved, Claude Code looks for its login in a keychain item
            // named after the directory; empty, this keeps it to the item of the principal's login.
            // Not documented by Claude Code: should it go, the login error says what else to do.
            if cfg!(target_os = "macos") {
                variables.push(("CLAUDE_SECURESTORAGE_CONFIG_DIR", String::new()));
            }
            variables
        }
        KnownHarness::Codex => vec![("CODEX_HOME", home)],
        KnownHarness::GeminiCli => vec![("GEMINI_CLI_HOME", home)],
        // `wf` in the session finds the data directory through `WF_DATA_DIR`.
        KnownHarness::Opencode => vec![
            ("XDG_DATA_HOME", below("data")),
            ("XDG_STATE_HOME", below("state")),
            ("XDG_CACHE_HOME", below("cache")),
        ],
    }
}

/// What the principal can do when `harness`, run in Fence with its state in `home`, says that it
/// needs a login; none when `environment` sets or inherits the variable of its directory, so that
/// it keeps its state where the principal chose.
pub(crate) fn login_help(
    harness: KnownHarness,
    home: &Path,
    environment: &EnvironmentConfig,
) -> Option<String> {
    let (variable, _) = harness_variables(harness, home)[0];
    if environment.set.contains_key(variable)
        || environment.inherit.iter().any(|name| name == variable)
    {
        return None;
    }
    let home = home.display();
    let ways = match harness {
        KnownHarness::ClaudeCode => {
            "give agents a token from `claude setup-token` as `CLAUDE_CODE_OAUTH_TOKEN`, or \
             `ANTHROPIC_API_KEY`, with `[environment] inherit`; or set `CLAUDE_CONFIG_DIR` to \
             `~/.claude` with `[environment] set` and add `~/.claude` to `[sandbox] allow_write`"
        }
        KnownHarness::Codex => {
            "give agents `CODEX_API_KEY` or `OPENAI_API_KEY` with `[environment] inherit`; or set \
             `CODEX_HOME` to `~/.codex` with `[environment] set` and add `~/.codex` to \
             `[sandbox] allow_write`"
        }
        KnownHarness::GeminiCli => {
            "give agents `GEMINI_API_KEY` with `[environment] inherit`; or set `GEMINI_CLI_HOME` to \
             `~` with `[environment] set` and add `~/.gemini` to `[sandbox] allow_write`"
        }
        KnownHarness::Opencode => {
            "give agents the API key of the provider with `[environment] inherit`; or set \
             `XDG_DATA_HOME` to `~/.local/share` with `[environment] set` and add \
             `~/.local/share/opencode` to `[sandbox] allow_write`"
        }
    };
    Some(format!(
        "in Fence, {} keeps its state in {home} and does not find the principal's login there: {ways}",
        harness.name()
    ))
}

/// The sandbox profile Fence runs the agent with.
fn profile(config: &FenceConfig, access: &Access<'_>) -> serde_json::Value {
    let paths = |paths: &[&str]| paths.iter().map(PathBuf::from).collect::<Vec<_>>();
    let own: Vec<PathBuf> = [
        paths(ALLOW_WRITE),
        vec![access.directory.to_path_buf(), access.socket.to_path_buf()],
    ]
    .concat();

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
    // runs git, should the principal allow writing to a directory above it. Fence already denies
    // `~/.gitconfig`.
    let git_config = [PathBuf::from("~/.config/git")];

    // Fence writes every deny after every allow, so key files are denied only below the agent's
    // own directory and temporary files. What the principal allows, and the harness's directory of
    // wf's, which holds no keys of the principal, stay writable whole.
    let key_files: Vec<PathBuf> = own
        .iter()
        .chain(&paths(FENCE_ALLOW_WRITE))
        .flat_map(|root| key_files_below(root))
        .collect();
    let allow_write: Vec<PathBuf> = own
        .into_iter()
        .chain(config.allow_write.iter().cloned())
        .chain(
            access
                .harness
                .map(|harness| access.data_dir.harness_home(harness)),
        )
        .collect();
    let deny_read: Vec<PathBuf> = [paths(DENY_READ), secrets.clone()].concat();
    let deny_write: Vec<PathBuf> = [key_files, secrets, git_config.to_vec()].concat();
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

/// What every agent writes besides its directory: the directory Fence starts in, which is its
/// directory, and temporary files.
const ALLOW_WRITE: &[&str] = &[".", "/tmp"];

/// Directories Fence allows writing to itself, whatever the profile says, besides devices and its
/// temporary directory in `/tmp`.
const FENCE_ALLOW_WRITE: &[&str] = &["~/.npm/_logs", "~/.fence/debug"];

/// Files that hold keys or secrets, by name.
const KEY_FILES: &[&str] = &[".env", ".env.*", "*.pem", "*.p12", "*.pfx"];

/// Globs of the key files anywhere below `root`, a path or a glob that Fence allows writing to.
fn key_files_below(root: &Path) -> impl Iterator<Item = PathBuf> {
    let root = root.to_string_lossy();
    let root = PathBuf::from(root.strip_suffix("/**").unwrap_or(&root));
    KEY_FILES.iter().map(move |file| root.join("**").join(file))
}

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
            harness: Some(KnownHarness::ClaudeCode),
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
        let home = data_dir.harness_home(KnownHarness::ClaudeCode);
        assert_eq!(
            wrapped.environment()["CLAUDE_CONFIG_DIR"],
            home.to_str().unwrap()
        );
        assert!(home.is_dir());
        assert!(!wrapped.environment().contains_key("CARGO_HOME"));

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
    fn nothing_in_the_principals_home_is_writable() {
        for path in [
            "~/.cache/pre-commit/repo/hooks.py",
            "~/.cache/uv/x",
            "~/.npm/_cacache/index-v5/x",
            "~/.npm/_npx/x/node_modules/a/index.js",
            "~/.bun/bin/claude",
            "~/.bun/install/cache/x",
            "~/.local/state/x",
            "~/.zcompdump",
            "~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/serde-1.0.0/build.rs",
            "~/.cargo/bin/cargo",
            "~/.claude.json",
            "~/.claude/settings.json",
            "~/.claude/.credentials.json",
            "~/.claude/hooks/notify.sh",
            "~/.codex/config.toml",
            "~/.codex/auth.json",
            "~/.gemini/settings.json",
            "~/.config/opencode/opencode.json",
            "~/.local/share/opencode/bin/x",
            "~/.amp/bin/amp",
            "~/.cursor/mcp.json",
        ] {
            assert!(!covers("allowWrite", path), "{path} is not writable");
        }
    }

    #[test]
    fn the_harness_keeps_its_state_in_its_directory_of_wfs() {
        for path in [
            "/home/ada/.local/share/workforce/harness/claude-code/.claude.json",
            "/home/ada/.local/share/workforce/harness/claude-code/settings.json",
            "/home/ada/.local/share/workforce/harness/claude-code/projects/-work-repo/s.jsonl",
            "/home/ada/.local/share/workforce/harness/claude-code/key.pem",
        ] {
            assert!(covers("allowWrite", path), "{path} is writable");
            assert!(!covers("denyWrite", path), "{path} is not denied");
        }
        assert!(!covers(
            "allowWrite",
            "/home/ada/.local/share/workforce/harness/codex/config.toml"
        ));
    }

    #[test]
    fn every_harness_is_given_its_directory() {
        let home = Path::new("/data/harness/x");
        for harness in KnownHarness::ALL {
            let variables = harness_variables(harness, home);
            assert!(!variables.is_empty(), "{harness:?}");
            for (name, value) in variables {
                assert!(
                    value.is_empty() || Path::new(&value).starts_with(home),
                    "{harness:?} {name}={value}"
                );
            }
        }
    }

    #[test]
    fn the_login_help_names_where_the_harness_keeps_its_state_and_how_to_move_it() {
        let home = Path::new("/data/harness/x");
        for harness in KnownHarness::ALL {
            let help = login_help(harness, home, &EnvironmentConfig::default()).unwrap();
            let (variable, _) = harness_variables(harness, home)[0].clone();
            for part in [variable, "allow_write", "inherit", "/data/harness/x"] {
                assert!(help.contains(part), "{harness:?} names {part}: {help}");
            }
        }
    }

    #[test]
    fn there_is_no_login_help_when_the_config_moves_the_harness_directory() {
        let home = Path::new("/data/harness/x");
        for harness in KnownHarness::ALL {
            let (variable, _) = harness_variables(harness, home)[0];
            let set = EnvironmentConfig {
                set: [(variable.to_string(), "/home/ada/.harness".to_string())].into(),
                ..EnvironmentConfig::default()
            };
            let inherited = EnvironmentConfig {
                inherit: vec![variable.to_string()],
                ..EnvironmentConfig::default()
            };
            assert_eq!(login_help(harness, home, &set), None, "{harness:?}");
            assert_eq!(login_help(harness, home, &inherited), None, "{harness:?}");
        }
    }

    #[test]
    fn a_check_has_no_harness_directory() {
        let access = Access {
            harness: None,
            ..access()
        };
        let profile = profile(&config(), &access);
        let written = serde_json::to_string(&profile["filesystem"]["allowWrite"]).unwrap();
        assert!(!written.contains("/harness"), "{written}");
    }

    #[test]
    fn key_files_are_denied_in_the_agents_directory_and_temporary_files() {
        for path in [
            "/home/ada/.local/share/workforce/w1/repos/k3f9/.env",
            "/home/ada/.local/share/workforce/w1/repos/k3f9/deploy/key.pem",
            "/home/ada/.local/share/workforce/w1/repos/k3f9/.env.local",
            "/tmp/cert.p12",
            "/tmp/x/cert.pfx",
            "./key.pem",
            "~/.npm/_logs/key.pem",
            "~/.fence/debug/.env",
        ] {
            assert!(covers("denyWrite", path), "{path} is not writable");
        }
        for path in [
            "/opt/cache/registry/src/x/ed25519-3.0.0/tests/examples/pkcs8-v1.pem",
            "/opt/cache/registry/src/x/dotenvy-0.15.7/.env",
        ] {
            assert!(covers("allowWrite", path), "{path} is writable");
            assert!(!covers("denyWrite", path), "{path} is not denied");
        }
    }

    /// Runs `script` as the agent, in Fence with the profile `wrap` writes, and says whether it
    /// succeeded.
    fn in_fence(
        config: &FenceConfig,
        access: &Access<'_>,
        profile_file: &Path,
        script: &str,
    ) -> bool {
        let agent = AcpAgentConfig::new("sh").arg("-c").arg(script);
        let wrapped = wrap(config, access, &agent, profile_file).unwrap();
        std::process::Command::new(wrapped.command())
            .args(wrapped.arguments())
            .envs(wrapped.environment())
            .status()
            .unwrap()
            .success()
    }

    #[test]
    fn in_fence_the_harness_writes_its_directory_and_cargo_unpacks_key_files_where_allowed() {
        if wf_core::testing::skip_in_fence("Fence in Fence denies what the outer one denies") {
            return;
        }
        if !fence_installed() {
            eprintln!("skipped: fence is not installed");
            return;
        }
        // Canonical, as Fence matches the paths the kernel sees, such as `/private/var/...`.
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let fixture = root.join("fixture");
        std::fs::create_dir_all(fixture.join("ed25519-3.0.0/tests/examples")).unwrap();
        std::fs::write(fixture.join("ed25519-3.0.0/Cargo.toml"), "").unwrap();
        std::fs::write(
            fixture.join("ed25519-3.0.0/tests/examples/pkcs8-v1.pem"),
            "-----BEGIN PRIVATE KEY-----\n",
        )
        .unwrap();
        let crate_file = root.join("ed25519-3.0.0.crate");
        let packed = std::process::Command::new("tar")
            .arg("-czf")
            .arg(&crate_file)
            .arg("-C")
            .arg(&fixture)
            .arg("ed25519-3.0.0")
            .status()
            .unwrap();
        assert!(packed.success());
        let repository = root.join("k3f9");
        std::fs::create_dir(&repository).unwrap();
        let data_dir = DataDir::new(root.join("data"));
        let access = Access {
            directory: &repository,
            data_dir: &data_dir,
            ..access()
        };
        let profile_file = root.join("sandbox/session.json");
        // A `CARGO_HOME` the principal set up for agents, as the guide shows.
        let cargo_home = root.join("cargo");
        std::fs::create_dir(&cargo_home).unwrap();
        let config = FenceConfig {
            allow_write: vec![cargo_home.clone()],
            ..FenceConfig::default()
        };

        assert!(in_fence(
            &config,
            &access,
            &profile_file,
            r#"echo '{}' > "$CLAUDE_CONFIG_DIR/settings.json""#
        ));
        assert!(
            data_dir
                .harness_home(KnownHarness::ClaudeCode)
                .join("settings.json")
                .is_file()
        );

        // As cargo unpacks a crate it downloaded into its registry.
        let unpack = format!(
            "mkdir -p '{registry}' && tar -xzf '{}' -C '{registry}'",
            crate_file.display(),
            registry = cargo_home.join("registry/src/index").display()
        );
        assert!(in_fence(&config, &access, &profile_file, &unpack));
        assert!(
            cargo_home
                .join("registry/src/index/ed25519-3.0.0/tests/examples/pkcs8-v1.pem")
                .is_file()
        );

        assert!(!in_fence(
            &config,
            &access,
            &profile_file,
            "echo key > key.pem"
        ));
        assert!(!repository.join("key.pem").exists());
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
            "/home/ada/.local/share/workforce/bin/wf",
            "/home/ada/.local/share/workforce/keys/device.key",
            "/home/ada/.local/share/workforce/daemon.pid",
            "/home/ada/.local/share/workforce/daemon.log",
            "/home/ada/.local/share/workforce/new-file",
            "/home/ada/.local/share/workforce/cargo/registry/src/x/build.rs",
            "/home/ada/.local/share/workforce/harness/opencode/data/opencode/bin/x",
            "/home/ada/.local/share/workforce/w1/store.redb",
            "/home/ada/.local/share/workforce/w1/sandbox/s1.json",
            "/home/ada/.local/share/workforce/w1/repos/other/src/main.rs",
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

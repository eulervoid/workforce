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

use crate::data_dir::DataDir;
use crate::machine_config::FenceConfig;

/// What the agent is let to touch besides its worktree, and what of wf it must not.
pub struct Access<'a> {
    pub worktree: &'a Path,
    /// The repository of the worktree: commits write their objects and refs there.
    pub repository: &'a Path,
    /// The branch the agent commits to, the only branch it may move. `None` where the agent has no
    /// branch, as in checks.
    pub branch: Option<&'a str>,
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

/// `agent`, started by Fence in the worktree. The sandbox profile Fence reads is written to `profile_file`.
pub(crate) fn wrap(
    config: &FenceConfig,
    access: &Access<'_>,
    agent: &AcpAgentConfig,
    profile_file: &Path,
) -> std::io::Result<AcpAgentConfig> {
    if let Some(parent) = profile_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let admin_directory = admin_directory(access.worktree)?;
    std::fs::write(
        profile_file,
        serde_json::to_vec_pretty(&profile(config, access, &admin_directory))?,
    )?;
    // Fence allows writes to "." of the directory it runs in, so it has to start in the worktree.
    Ok(AcpAgentConfig::new("sh")
        .arg("-c")
        .arg(r#"cd "$1" && shift && exec "$@""#)
        .arg("sh")
        .arg(access.worktree.to_string_lossy())
        .arg("fence")
        .arg("--settings")
        .arg(profile_file.to_string_lossy())
        .arg("--")
        .arg(agent.command().to_string_lossy())
        .args(agent.arguments())
        .envs(agent.environment()))
}

/// The admin directory git keeps for `worktree` in `.git/worktrees`, read from the `gitdir:` line
/// of the worktree's `.git` file. Git appends a number to the name when the directory name is
/// taken, so the name of the worktree does not say.
fn admin_directory(worktree: &Path) -> std::io::Result<PathBuf> {
    let file = worktree.join(".git");
    let contents = std::fs::read_to_string(&file)?;
    parse_gitdir(&contents).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} has no `gitdir:` line", file.display()),
        )
    })
}

fn parse_gitdir(contents: &str) -> Option<PathBuf> {
    contents
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))
        .map(|path| PathBuf::from(path.trim()))
}

/// The sandbox profile Fence runs the agent with.
fn profile(config: &FenceConfig, access: &Access<'_>, admin_directory: &Path) -> serde_json::Value {
    let git_directory = access.repository.join(".git");
    let writable: Vec<PathBuf> = [access.worktree.to_path_buf(), access.socket.to_path_buf()]
        .into_iter()
        .chain(git_writable(access, &git_directory, admin_directory))
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
    // With these an agent has git run its code, outside the sandbox, the next time the daemon or
    // the principal runs git. Fence already denies `~/.gitconfig`.
    let git_config = [
        git_directory.join("config"),
        git_directory.join("hooks"),
        git_directory.join("worktrees").join("*").join("commondir"),
        git_directory
            .join("worktrees")
            .join("*")
            .join("config.worktree"),
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

/// What of the repository's `.git` the agent writes, so that it can commit, rebase and fetch but
/// not move any branch but its own, nor the target, nor touch the admin directory of another
/// worktree. Everything else (`HEAD`, `packed-refs`, `config`, `hooks`, `refs/heads` of others)
/// stays closed.
fn git_writable(access: &Access<'_>, git_directory: &Path, admin_directory: &Path) -> Vec<PathBuf> {
    let mut paths = vec![
        git_directory.join("objects"),
        git_directory.join("refs/remotes"),
        // Git appends a reflog entry for every remote-tracking ref a fetch updates.
        git_directory.join("logs/refs/remotes"),
        git_directory.join("refs/tags"),
        git_directory.join("FETCH_HEAD"),
        admin_directory.to_path_buf(),
        // Git locks `packed-refs` to delete a reference, also for the pseudorefs it removes after
        // every commit and rebase (`ORIG_HEAD`, `AUTO_MERGE`, ...), and prints an error when it
        // cannot. It writes nothing there unless the reference is packed, and the file itself
        // stays closed: renaming the lock onto it is denied.
        git_directory.join("packed-refs.lock"),
    ];
    if let Some(branch) = access.branch {
        for kind in ["refs/heads", "logs/refs/heads"] {
            let reference = git_directory.join(kind).join(branch);
            // Git writes a reference through `<name>.lock`.
            let mut lock = reference.clone().into_os_string();
            lock.push(".lock");
            paths.push(reference);
            paths.push(lock.into());
        }
    }
    paths
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
    const ADMIN: &str = "/work/repo/.git/worktrees/k3f9";
    const WORKTREE: &str = "/home/ada/.local/share/workforce/w1/worktrees/k3f9";

    fn access() -> Access<'static> {
        Access {
            worktree: Path::new(WORKTREE),
            repository: Path::new("/work/repo"),
            branch: Some("wf/k3f9"),
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
        serde_json::from_value(
            profile(&config(), &access(), Path::new(ADMIN))["filesystem"][list].clone(),
        )
        .unwrap()
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
        let profile_file = directory.path().join("sandbox/session.json");
        let agent = AcpAgentConfig::new("claude-agent-acp")
            .arg("--flag")
            .env("WF_SESSION", "s1");
        let worktree = directory.path().join("k3f9");
        std::fs::create_dir(&worktree).unwrap();
        std::fs::write(worktree.join(".git"), format!("gitdir: {ADMIN}\n")).unwrap();
        let access = Access {
            worktree: &worktree,
            ..access()
        };
        let wrapped = wrap(&config(), &access, &agent, &profile_file).unwrap();

        assert_eq!(wrapped.command(), Path::new("sh"));
        assert_eq!(
            wrapped.arguments()[3..],
            [
                worktree.to_str().unwrap(),
                "fence",
                "--settings",
                profile_file.to_str().unwrap(),
                "--",
                "claude-agent-acp",
                "--flag"
            ]
        );
        assert_eq!(wrapped.environment()["WF_SESSION"], "s1");

        let written: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&profile_file).unwrap()).unwrap();
        assert_eq!(written, profile(&config(), &access, Path::new(ADMIN)));
    }

    #[test]
    fn the_agent_writes_its_worktree_commits_and_talks_to_the_daemon() {
        for path in [
            "/home/ada/.local/share/workforce/w1/worktrees/k3f9/src/main.rs",
            "/work/repo/.git/objects/ab/cdef",
            "/work/repo/.git/refs/heads/wf/k3f9",
            "/work/repo/.git/refs/heads/wf/k3f9.lock",
            "/work/repo/.git/logs/refs/heads/wf/k3f9",
            "/work/repo/.git/refs/remotes/origin/main",
            "/work/repo/.git/logs/refs/remotes/origin/main",
            "/work/repo/.git/worktrees/k3f9/index",
            "/work/repo/.git/worktrees/k3f9/HEAD",
            "/work/repo/.git/packed-refs.lock",
            "/run/wf/daemon.sock",
            "/opt/cache/x",
        ] {
            assert!(covers("allowWrite", path), "{path} is writable");
            assert!(!covers("denyWrite", path), "{path} is not denied");
        }
        let profile = profile(&config(), &access(), Path::new(ADMIN));
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
    fn git_cannot_be_made_to_run_code() {
        for path in [
            "/work/repo/.git/config",
            "/work/repo/.git/hooks/pre-commit",
            "/work/repo/.git/worktrees/k3f9/commondir",
            "/work/repo/.git/worktrees/k3f9/config.worktree",
            "/home/ada/.local/share/workforce/w1/worktrees/k3f9/.git",
            "~/.config/git/config",
        ] {
            assert!(covers("denyWrite", path), "{path} is not writable");
        }
    }

    #[test]
    fn the_target_and_other_worktrees_cannot_be_moved() {
        for path in [
            "/work/repo/.git/refs/heads/main",
            "/work/repo/.git/refs/heads/main.lock",
            "/work/repo/.git/refs/heads/wf/other",
            "/work/repo/.git/logs/refs/heads/main",
            "/work/repo/.git/packed-refs",
            "/work/repo/.git/HEAD",
            "/work/repo/.git/worktrees/other/HEAD",
            "/work/repo/.git/worktrees/other/index",
        ] {
            assert!(!covers("allowWrite", path), "{path} is not writable");
        }
    }

    #[test]
    fn the_admin_directory_is_the_one_the_gitdir_line_names() {
        let file = "gitdir: /work/repo/.git/worktrees/k3f92\n";
        let admin = parse_gitdir(file).unwrap();
        assert_eq!(admin, Path::new("/work/repo/.git/worktrees/k3f92"));
        let profile = profile(&config(), &access(), &admin);
        let allowed = profile["filesystem"]["allowWrite"].to_string();
        assert!(allowed.contains("/work/repo/.git/worktrees/k3f92"));
        assert!(!allowed.contains("/work/repo/.git/worktrees/k3f9\""));
        assert_eq!(parse_gitdir("not a worktree"), None);
    }

    #[test]
    fn without_a_branch_no_branch_is_writable() {
        let access = Access {
            branch: None,
            ..access()
        };
        let profile = profile(&config(), &access, Path::new(ADMIN));
        let allowed = profile["filesystem"]["allowWrite"].to_string();
        assert!(!allowed.contains("refs/heads"));
        assert!(allowed.contains("/work/repo/.git/objects"));
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

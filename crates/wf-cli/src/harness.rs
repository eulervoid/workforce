//! Detects agent harnesses whose ACP command is installed on `PATH`, and asks them for the models
//! they offer. There is no probe of the login yet.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use serde::{Deserialize, Serialize};
use wf_core::name::HarnessName;
use wf_harness::KnownHarness;

use crate::commands::emit;

/// Whether this machine can start `harness`: wf knows it by name.
pub fn is_startable(harness: &str) -> bool {
    KnownHarness::from_name(harness).is_some()
}

/// What `wf agent add` and `edit` make of a harness name.
///
/// A name this machine cannot start is refused unless `allow_unknown_here` says that another
/// member's machine may know it, in which case the returned warning is what to tell the principal.
pub fn check_startable(
    harness: &HarnessName,
    allow_unknown_here: bool,
) -> anyhow::Result<Option<String>> {
    if is_startable(harness.as_str()) {
        return Ok(None);
    }
    let closest = KnownHarness::ALL
        .into_iter()
        .max_by(|left, right| {
            strsim::jaro_winkler(harness.as_str(), left.name())
                .total_cmp(&strsim::jaro_winkler(harness.as_str(), right.name()))
        })
        .expect("wf knows harnesses")
        .name();
    let known = KnownHarness::ALL
        .iter()
        .map(|known| format!("`{}`", known.name()))
        .collect::<Vec<_>>()
        .join(", ");
    if allow_unknown_here {
        return Ok(Some(format!(
            "this machine cannot start the harness `{harness}`; wf knows {known}"
        )));
    }
    bail!(
        "this machine cannot start the harness `{harness}`; did you mean `{closest}`? wf knows {known}; \
         `--harness-unknown-here` defines it anyway, for a machine that does know it"
    )
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

pub fn detect(path_variable: &std::ffi::OsStr) -> Vec<&'static str> {
    let directories: Vec<PathBuf> = std::env::split_paths(path_variable).collect();
    KnownHarness::ALL
        .into_iter()
        .filter(|harness| {
            let agent = harness.command();
            directories
                .iter()
                .any(|directory| is_executable(&directory.join(agent.command())))
        })
        .map(KnownHarness::name)
        .collect()
}

/// How long a harness may take to start and offer its models.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// A model a harness offers, as `wf harness models` lists it.
#[derive(Serialize, Deserialize)]
pub struct ModelView {
    /// What `--model` takes.
    pub value: String,
    pub name: String,
    /// The harness uses it when no model is chosen.
    pub is_default: bool,
}

/// The name of a model without the provider that its value already names, as opencode repeats
/// it: `anthropic/Claude Haiku 4.5` for `anthropic/claude-haiku-4-5`.
fn without_provider<'a>(value: &str, name: &'a str) -> &'a str {
    value
        .split_once('/')
        .and_then(|(provider, _)| name.strip_prefix(provider)?.strip_prefix('/'))
        .unwrap_or(name)
}

/// `wf harness models`: starts the harness, opens a session in an empty directory to read the
/// models it offers, and stops it.
pub fn models(json: bool, harness: &HarnessName) -> anyhow::Result<()> {
    let agent = KnownHarness::from_name(harness.as_str())
        .ok_or_else(|| anyhow!("wf does not know how to start the harness {harness}"))?
        .command();
    let directory = tempfile::tempdir()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let models = runtime
        .block_on(async {
            tokio::time::timeout(
                PROBE_TIMEOUT,
                wf_harness::models(agent, directory.path().to_path_buf()),
            )
            .await
        })
        .map_err(|_| anyhow!("{harness} did not offer its models within {PROBE_TIMEOUT:?}"))?
        .with_context(|| format!("asking {harness} for its models"))?;
    let views: Vec<ModelView> = models
        .into_iter()
        .map(|model| ModelView {
            name: without_provider(&model.value, &model.name).to_string(),
            value: model.value,
            is_default: model.is_default,
        })
        .collect();
    emit(json, &views, |models| {
        if models.is_empty() {
            return format!("{harness} offers no models to choose from\n");
        }
        let width = models
            .iter()
            .map(|model| model.value.len())
            .max()
            .unwrap_or(0);
        models
            .iter()
            .map(|model| {
                let default = if model.is_default { "  (default)" } else { "" };
                format!("{:width$}  {}{default}\n", model.value, model.name)
            })
            .collect()
    })
}

/// The models `harness` offers, read by running `wf harness models` in a child process. The
/// probe runs apart because the harness it starts is killed when it ends, and the SIGCHLD of
/// that would interrupt a prompt reading the terminal; a `wait` on the child is not disturbed.
pub fn models_of(harness: &str) -> anyhow::Result<Vec<ModelView>> {
    let output = std::process::Command::new(std::env::current_exe()?)
        .args(["--json", "harness", "models", harness])
        .stdin(std::process::Stdio::null())
        .output()?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        bail!("{}", error.trim().trim_start_matches("error: "));
    }
    #[derive(Deserialize)]
    struct Output {
        data: Vec<ModelView>,
    }
    let output: Output = serde_json::from_slice(&output.stdout)?;
    Ok(output.data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_drops_the_provider_its_value_names() {
        assert_eq!(
            without_provider("anthropic/claude-haiku-4-5", "anthropic/Claude Haiku 4.5"),
            "Claude Haiku 4.5"
        );
        assert_eq!(without_provider("haiku", "Haiku 5.5"), "Haiku 5.5");
        assert_eq!(
            without_provider("openrouter/qwen/qwen3", "Qwen: Qwen3"),
            "Qwen: Qwen3"
        );
    }
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn finds_executables_on_the_path() {
        let directory = tempfile::tempdir().unwrap();
        for (name, mode) in [
            ("codex-acp", 0o755),
            ("opencode", 0o644),
            ("unrelated", 0o755),
        ] {
            let path = directory.path().join(name);
            std::fs::write(&path, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        let found = detect(directory.path().as_os_str());
        assert_eq!(found, vec!["codex"]);
    }
}

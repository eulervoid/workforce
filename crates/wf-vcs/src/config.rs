//! The git config of commands that run in a sandbox, and the git variables wf sets for them.

use std::path::{Path, PathBuf};

/// The variable that names the file git reads as its global config.
pub const GLOBAL_CONFIG_VARIABLE: &str = "GIT_CONFIG_GLOBAL";

/// Writes the global git config a command runs with to `path`, creating its directory: the
/// machine's, with signing off. A command must not sign with the principal's key, and could not
/// reach it in the sandbox anyway; a repository that turns signing on in its own config, as
/// tests of signing do, still signs. Git reads it when [`GLOBAL_CONFIG_VARIABLE`] names `path`.
pub fn write_unsigned_global_config(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, unsigned_config(&global_configs()))
}

/// Whether wf sets the git variable `name` itself for a session, so that the principal's config
/// of the environment must not.
pub fn is_set_by_wf(name: &str) -> bool {
    matches!(
        name,
        "GIT_AUTHOR_NAME" | "GIT_COMMITTER_NAME" | crate::CONFIG_COUNT
    ) || [crate::CONFIG_KEY, crate::CONFIG_VALUE]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

fn unsigned_config(global: &[PathBuf]) -> String {
    let mut config = String::new();
    for path in global {
        let path = path
            .to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        config.push_str(&format!("[include]\n\tpath = \"{path}\"\n"));
    }
    config.push_str("[commit]\n\tgpgsign = false\n[tag]\n\tgpgsign = false\n");
    config
}

/// The files git reads as the global config of this process's environment, in its order.
fn global_configs() -> Vec<PathBuf> {
    if let Some(path) = std::env::var_os(GLOBAL_CONFIG_VARIABLE) {
        return vec![PathBuf::from(path)];
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|home| home.join(".config")));
    [
        xdg.map(|config| config.join("git").join("config")),
        home.map(|home| home.join(".gitconfig")),
    ]
    .into_iter()
    .flatten()
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_git_config_of_a_command_is_the_global_one_without_signing() {
        let directory = tempfile::tempdir().unwrap();
        let global = directory.path().join("my \"git\" config");
        std::fs::write(
            &global,
            "[user]\n\tname = Ada\n[commit]\n\tgpgsign = true\n[tag]\n\tgpgsign = true\n",
        )
        .unwrap();
        let missing = directory.path().join("missing");
        let config = directory.path().join("command.gitconfig");
        std::fs::write(&config, unsigned_config(&[missing, global])).unwrap();
        let get = |key: &str| {
            let output = std::process::Command::new("git")
                .args(["config", "--get", key])
                .current_dir(directory.path())
                .env(GLOBAL_CONFIG_VARIABLE, &config)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env_remove(crate::CONFIG_COUNT)
                .output()
                .unwrap();
            String::from_utf8(output.stdout).unwrap().trim().to_string()
        };
        assert_eq!(get("user.name"), "Ada");
        assert_eq!(get("commit.gpgsign"), "false");
        assert_eq!(get("tag.gpgsign"), "false");
    }

    #[test]
    fn the_variables_of_signing_are_set_by_wf() {
        assert!(is_set_by_wf("GIT_CONFIG_COUNT"));
        assert!(is_set_by_wf("GIT_CONFIG_KEY_0"));
        assert!(is_set_by_wf("GIT_CONFIG_VALUE_12"));
        assert!(is_set_by_wf("GIT_AUTHOR_NAME"));
        assert!(!is_set_by_wf("GIT_CONFIG_GLOBAL"));
    }
}

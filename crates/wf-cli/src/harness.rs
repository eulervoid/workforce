//! Detects agent harnesses whose ACP command is installed on `PATH`. There is no probe of the
//! login yet.

use std::path::{Path, PathBuf};

/// The harnesses wf knows how to start, in the order they are offered.
const KNOWN: &[&str] = &["claude-code", "opencode", "codex", "gemini-cli"];

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

pub fn detect(path_variable: &std::ffi::OsStr) -> Vec<&'static str> {
    let directories: Vec<PathBuf> = std::env::split_paths(path_variable).collect();
    KNOWN
        .iter()
        .filter(|harness| {
            let Some(agent) = wf_harness::command_for(harness) else {
                return false;
            };
            directories
                .iter()
                .any(|directory| is_executable(&directory.join(agent.command())))
        })
        .copied()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
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

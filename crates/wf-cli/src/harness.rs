//! Detects agent harnesses installed on `PATH`. There is no ACP probe yet.

use std::path::{Path, PathBuf};

/// `(binary, harness name)`
const KNOWN: &[(&str, &str)] = &[
    ("claude", "claude-code"),
    ("opencode", "opencode"),
    ("codex", "codex"),
    ("gemini", "gemini-cli"),
];

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

pub fn detect(path_variable: &std::ffi::OsStr) -> Vec<&'static str> {
    let directories: Vec<PathBuf> = std::env::split_paths(path_variable).collect();
    KNOWN
        .iter()
        .filter(|(binary, _)| {
            directories
                .iter()
                .any(|directory| is_executable(&directory.join(binary)))
        })
        .map(|(_, harness)| *harness)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn finds_executables_on_the_path() {
        let directory = tempfile::tempdir().unwrap();
        for (name, mode) in [("codex", 0o755), ("opencode", 0o644), ("unrelated", 0o755)] {
            let path = directory.path().join(name);
            std::fs::write(&path, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        let found = detect(directory.path().as_os_str());
        assert_eq!(found, vec!["codex"]);
    }
}

//! What the tests that run the `wf` binary share.

#![allow(dead_code)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

pub struct Sandbox {
    pub root: TempDir,
    pub repository: PathBuf,
    pub data: PathBuf,
    pub bin: PathBuf,
}

impl Sandbox {
    pub fn new() -> Self {
        let root = TempDir::new().unwrap();
        let repository = root.path().join("repo");
        let data = root.path().join("data");
        let bin = root.path().join("bin");
        std::fs::create_dir_all(&repository).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        for harness in ["claude-agent-acp", "opencode"] {
            let path = bin.join(harness);
            std::fs::write(&path, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // Commands that show or check out work run git.
        let git = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|directory| directory.join("git"))
            .find(|path| path.is_file())
            .expect("git is installed");
        std::os::unix::fs::symlink(git, bin.join("git")).unwrap();
        Self {
            root,
            repository,
            data,
            bin,
        }
    }

    pub fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_wf"));
        command
            .args(args)
            .current_dir(&self.repository)
            .env("XDG_DATA_HOME", &self.data)
            .env("XDG_RUNTIME_DIR", self.root.path().join("run"))
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("PATH", &self.bin)
            .env("USER", "ada");
        command
    }

    pub fn run(&self, args: &[&str]) -> String {
        let output = self.command(args).output().unwrap();
        assert!(
            output.status.success(),
            "wf {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    pub fn fail(&self, args: &[&str]) -> String {
        let output = self.command(args).output().unwrap();
        assert!(!output.status.success(), "wf {args:?} should fail");
        String::from_utf8(output.stderr).unwrap()
    }

    pub fn json(&self, args: &[&str]) -> Value {
        let mut all = args.to_vec();
        all.push("--json");
        let value: Value = serde_json::from_str(&self.run(&all)).unwrap();
        assert_eq!(value["schema"], wf_cli::view::SCHEMA_VERSION);
        value["data"].clone()
    }

    pub fn add(&self, args: &[&str]) -> String {
        let mut all = vec!["item", "add"];
        all.extend_from_slice(args);
        self.json(&all)["id"].as_str().unwrap().to_string()
    }
}

pub fn short(id: &str) -> &str {
    &id[..6]
}

/// The comments in the activity of an item `wf item show --json` printed, oldest first: comments
/// of their own and the reasons given with moves and verdicts, each with the text of its entry.
pub fn comments(shown: &Value) -> Value {
    Value::Array(
        shown["activity"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["comment"].is_object())
            .map(|entry| {
                let mut comment = entry["comment"].clone();
                comment["text"] = entry["text"].clone();
                comment
            })
            .collect(),
    )
}

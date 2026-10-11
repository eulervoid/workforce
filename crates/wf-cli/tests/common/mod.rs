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
        for harness in ["claude", "opencode"] {
            let path = bin.join(harness);
            std::fs::write(&path, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
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
            .env("PATH", &self.bin)
            .env("USER", "joshua");
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

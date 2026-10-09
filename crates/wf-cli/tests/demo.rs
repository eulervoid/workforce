//! Runs the real `wf` binary through the demo script of the M0 plan.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

struct Sandbox {
    _root: TempDir,
    repository: PathBuf,
    data: PathBuf,
    bin: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
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
            _root: root,
            repository,
            data,
            bin,
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_wf"));
        command
            .args(args)
            .current_dir(&self.repository)
            .env("XDG_DATA_HOME", &self.data)
            .env("PATH", &self.bin)
            .env("USER", "joshua");
        command
    }

    fn run(&self, args: &[&str]) -> String {
        let output = self.command(args).output().unwrap();
        assert!(
            output.status.success(),
            "wf {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn fail(&self, args: &[&str]) -> String {
        let output = self.command(args).output().unwrap();
        assert!(!output.status.success(), "wf {args:?} should fail");
        String::from_utf8(output.stderr).unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut all = args.to_vec();
        all.push("--json");
        let value: Value = serde_json::from_str(&self.run(&all)).unwrap();
        assert_eq!(value["schema"], 1);
        value["data"].clone()
    }

    fn add(&self, args: &[&str]) -> String {
        let mut all = vec!["item", "add"];
        all.extend_from_slice(args);
        self.json(&all)["id"].as_str().unwrap().to_string()
    }
}

fn short(id: &str) -> &str {
    &id[..6]
}

fn titles(items: &Value) -> Vec<String> {
    items
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["title"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn the_demo_script_works_end_to_end() {
    let wf = Sandbox::new();

    let init = wf.json(&["init", "--name", "demo"]);
    assert_eq!(init["handle"], "joshua");
    assert_eq!(init["agents"].as_array().unwrap().len(), 2);
    assert!(wf.repository.join(".workforce.toml").is_file());
    assert!(
        wf.fail(&["init", "--name", "again"])
            .contains("already belongs")
    );

    let agents = wf.run(&["agent", "list"]);
    assert!(agents.contains("@joshua/coder") && agents.contains("@joshua/coder-opencode"));

    let task = wf.add(&["Implement feed sync", "--priority", "P1"]);
    let question = wf.add(&["Which hash for event ids?", "--type", "question"]);

    let link = wf.run(&[
        "item",
        "link",
        short(&task),
        "--depends-on",
        short(&question),
    ]);
    assert!(link.contains("now depends on"));
    let list = wf.run(&["item", "list"]);
    assert!(
        list.contains("Blocked by") && list.contains("question, open"),
        "{list}"
    );
    assert_eq!(
        titles(&wf.json(&["item", "ready"])),
        ["Which hash for event ids?"]
    );

    wf.run(&["item", "move", short(&question), "answered"]);
    assert_eq!(
        titles(&wf.json(&["item", "ready"])),
        ["Implement feed sync"]
    );

    wf.run(&[
        "comment",
        short(&task),
        &format!("see #{}, @coder please keep it small", short(&question)),
    ]);
    let shown = wf.json(&["item", "show", short(&task)]);
    assert_eq!(shown["references"][0]["id"], question);
    assert_eq!(shown["mentions"][0], "@joshua/coder");
    assert_eq!(
        shown["notes"][0]["text"],
        format!("see #{}, @coder please keep it small", short(&question))
    );
    assert_eq!(
        wf.json(&["item", "show", short(&question)])["referenced_by"][0]["item"],
        task
    );
    let mentions = wf.run(&["mentions", "@coder"]);
    assert!(mentions.contains("Implement feed sync"), "{mentions}");

    wf.run(&["item", "move", short(&task), "done"]);
    wf.run(&["item", "archive", short(&task)]);
    assert_eq!(
        titles(&wf.json(&["item", "list"])),
        ["Which hash for event ids?"]
    );
    let with_archived = wf.json(&["item", "list", "--archived"]);
    assert_eq!(titles(&with_archived).len(), 2);
    let stub = wf.run(&["item", "show", short(&task)]);
    assert!(stub.contains("archived"), "{stub}");

    let log = wf.run(&["log"]);
    assert!(log.contains("events verified"), "{log}");
    let events = wf.json(&["log"])["events"].as_array().unwrap().len();
    assert_eq!(events, 1 + 2 + 2 + 1 + 1 + 1 + 1 + 1);
    assert!(
        wf.json(&["log"])["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|event| event["verified"] == true)
    );

    let before = wf.json(&["item", "list", "--archived"]);
    let status = wf.run(&["status", "--refold"]);
    assert!(status.contains("refolded"), "{status}");
    assert_eq!(wf.json(&["item", "list", "--archived"]), before);
    assert_eq!(wf.json(&["status"])["events"], events);
}

#[test]
fn invalid_requests_fail_with_the_specific_reason() {
    let wf = Sandbox::new();
    wf.run(&["init", "--name", "demo", "--no-agents"]);
    let task = wf.add(&["a task"]);
    let decision = wf.add(&["a decision", "--type", "decision"]);

    assert!(
        wf.fail(&["item", "move", short(&task), "answered"])
            .contains("no state `answered`")
    );
    assert!(
        wf.fail(&["item", "archive", short(&task)])
            .contains("done or cancelled")
    );
    assert!(
        wf.fail(&["item", "add", "x", "--type", "nonsense"])
            .contains("no item type")
    );
    assert!(
        wf.fail(&["item", "add", "x", "--type", "decision", "--priority", "P1"])
            .contains("Priority")
    );
    assert!(
        wf.fail(&[
            "item",
            "link",
            short(&decision),
            "--depends-on",
            short(&task)
        ])
        .contains("cannot depend")
    );
    assert!(
        wf.fail(&["item", "link", short(&task), "--depends-on", short(&task)])
            .contains("itself")
    );
    assert!(
        wf.fail(&["item", "show", "zzzzzz"])
            .contains("matches nothing")
    );
    assert!(wf.fail(&["item", "show", "ab"]).contains("too short"));
    assert!(
        wf.fail(&["mentions", "@ghost"])
            .contains("does not name anyone")
    );
}

#[test]
fn item_edit_by_flags_and_by_editor() {
    let wf = Sandbox::new();
    wf.run(&["init", "--name", "demo", "--no-agents"]);
    let task = wf.add(&["Old title", "--label", "keep", "--label", "drop"]);

    wf.run(&[
        "item",
        "edit",
        short(&task),
        "--title",
        "New title",
        "--priority",
        "P0",
        "--add-label",
        "fresh",
        "--remove-label",
        "drop",
    ]);
    let shown = wf.json(&["item", "show", short(&task)]);
    assert_eq!(shown["title"], "New title");
    assert_eq!(shown["priority"], "P0");
    assert_eq!(shown["labels"], serde_json::json!(["fresh", "keep"]));

    let editor = wf.bin.join("fake-editor");
    std::fs::write(
        &editor,
        "#!/bin/sh\n/usr/bin/sed -i.bak -e 's/^title: .*/title: Edited in editor/' -e 's/^priority: .*/priority: null/' \"$1\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o755)).unwrap();
    let output = wf
        .command(&["item", "edit", short(&task)])
        .env("EDITOR", &editor)
        .env("VISUAL", &editor)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let shown = wf.json(&["item", "show", short(&task), "--history"]);
    assert_eq!(shown["title"], "Edited in editor");
    assert_eq!(shown["priority"], Value::Null);
    let history = shown["history"].as_array().unwrap();
    assert!(history.iter().any(|entry| {
        entry["summary"]
            .as_str()
            .unwrap()
            .contains("title = \"New title\"")
    }));

    assert!(
        wf.run(&["item", "edit", short(&task), "--title", "Edited in editor"])
            .contains("nothing changed")
    );
}

#[test]
fn agents_and_policy() {
    let wf = Sandbox::new();
    wf.run(&["init", "--name", "demo", "--no-agents"]);
    wf.run(&[
        "agent",
        "add",
        "reviewer",
        "--harness",
        "codex",
        "--capability",
        "rust",
        "--lane",
        "default",
    ]);
    assert!(
        wf.fail(&["agent", "add", "reviewer", "--harness", "codex"])
            .contains("already exists")
    );
    wf.run(&["agent", "edit", "reviewer", "--model", "big"]);
    let agents = wf.json(&["agent", "list"]);
    assert_eq!(agents[0]["id"], "@joshua/reviewer");
    assert_eq!(agents[0]["model"], "big");
    assert_eq!(agents[0]["capabilities"], serde_json::json!(["rust"]));
    assert!(
        wf.fail(&["agent", "edit", "ghost", "--model", "x"])
            .contains("no agent")
    );

    let policy = wf.run(&["policy", "show"]);
    assert!(policy.contains("[types.task]"));
    let custom = format!(
        "{policy}\n[types.bug]\nlane = \"default\"\ninitial = \"new\"\nstates = [{{ name = \"new\", category = \"open\" }}, {{ name = \"fixed\", category = \"done\" }}]\n"
    );
    let file = wf.repository.join("policy.toml");
    std::fs::write(&file, &custom).unwrap();
    wf.run(&["policy", "apply", file.to_str().unwrap()]);
    wf.add(&["crash", "--type", "bug"]);

    std::fs::write(&file, policy.replace("\"answered\"", "\"replied\"")).unwrap();
    assert!(
        wf.fail(&["policy", "apply", file.to_str().unwrap()])
            .contains("cannot be changed")
    );
}

#[test]
fn commands_outside_a_workforce_explain_what_to_do() {
    let wf = Sandbox::new();
    assert!(wf.fail(&["item", "list"]).contains("wf init"));
}

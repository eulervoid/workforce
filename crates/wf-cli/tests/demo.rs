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
            .env("XDG_RUNTIME_DIR", self._root.path().join("run"))
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

    let task = wf.add(&["--title", "Implement feed sync", "--priority", "P1"]);
    let question = wf.add(&["--title", "Which hash for event ids?", "--type", "question"]);

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
    // genesis, device profile, two agents, then the items
    assert_eq!(events, 1 + 1 + 2 + 2 + 1 + 1 + 1 + 1 + 1);
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
    let task = wf.add(&["--title", "a task"]);
    let decision = wf.add(&["--title", "a decision", "--type", "decision"]);

    assert!(
        wf.fail(&["item", "move", short(&task), "answered"])
            .contains("no state `answered`")
    );
    assert!(
        wf.fail(&["item", "archive", short(&task)])
            .contains("done or cancelled")
    );
    assert!(
        wf.fail(&["item", "add", "--title", "x", "--type", "nonsense"])
            .contains("no item type")
    );
    assert!(
        wf.fail(&[
            "item",
            "add",
            "--title",
            "x",
            "--type",
            "decision",
            "--priority",
            "P1"
        ])
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
    let task = wf.add(&["--title", "Old title", "--label", "keep", "--label", "drop"]);

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
        "#!/bin/sh\n/usr/bin/sed -i.bak -e 's/^title = .*/title = \"Edited in editor\"/' \"$1\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o755)).unwrap();
    let output = wf
        .command(&["item", "edit", short(&task), "--edit"])
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
    assert_eq!(shown["priority"], "P0");
    let history = shown["history"].as_array().unwrap();
    assert!(history.iter().any(|entry| {
        entry["summary"]
            .as_str()
            .unwrap()
            .contains("title = \"New title\"")
    }));

    let remover = wf.bin.join("priority-remover");
    std::fs::write(
        &remover,
        "#!/bin/sh\n/usr/bin/sed -i.bak -e '/^priority = /d' \"$1\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&remover, std::fs::Permissions::from_mode(0o755)).unwrap();
    let output = wf
        .command(&["item", "edit", short(&task), "--edit"])
        .env("EDITOR", &remover)
        .env("VISUAL", &remover)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("needs a priority"));

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
    wf.add(&["--title", "crash", "--type", "bug"]);

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

#[test]
fn item_add_opens_the_editor_on_request_and_asks_for_a_title_otherwise() {
    let wf = Sandbox::new();
    wf.run(&["init", "--name", "demo", "--no-agents"]);
    assert!(
        wf.fail(&["item", "add"])
            .contains("missing --title; run at a terminal")
    );

    let editor = wf.bin.join("fake-editor");
    std::fs::write(
        &editor,
        "#!/bin/sh\n/usr/bin/sed -i.bak -e 's/^title = .*/title = \"From the editor\"/' -e 's/^type = .*/type = \"question\"/' \"$1\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o755)).unwrap();
    let output = wf
        .command(&["item", "add", "--edit", "--priority", "P1", "--json"])
        .env("EDITOR", &editor)
        .env("VISUAL", &editor)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let created: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(created["data"]["title"], "From the editor");
    assert_eq!(created["data"]["type"], "question");
    assert_eq!(created["data"]["priority"], "P1");
}

#[test]
fn comments_need_text_or_the_editor() {
    let wf = Sandbox::new();
    wf.run(&["init", "--name", "demo", "--no-agents"]);
    let task = wf.add(&["--title", "a task"]);
    assert!(
        wf.fail(&["comment", short(&task)])
            .contains("the comment is empty")
    );

    let editor = wf.bin.join("fake-editor");
    std::fs::write(
        &editor,
        "#!/bin/sh\necho 'written in the editor' >> \"$1\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o755)).unwrap();
    let output = wf
        .command(&["comment", short(&task), "--edit", "start:"])
        .env("EDITOR", &editor)
        .env("VISUAL", &editor)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let shown = wf.json(&["item", "show", short(&task)]);
    assert!(shown.to_string().contains("written in the editor"));
}

#[test]
fn missing_items_and_states_are_errors_without_a_terminal() {
    let wf = Sandbox::new();
    wf.run(&["init", "--name", "demo", "--no-agents"]);
    let task = wf.add(&["--title", "a task"]);
    for args in [
        &["item", "show"][..],
        &["item", "edit"],
        &["item", "move"],
        &["item", "archive"],
        &["item", "unarchive"],
        &["item", "link", "--depends-on", short(&task)],
        &["comment"],
    ] {
        assert!(
            wf.fail(args).contains("missing item; pass an item id"),
            "{args:?}"
        );
    }
    assert!(
        wf.fail(&["item", "move", short(&task)])
            .contains("missing state")
    );
    assert!(
        wf.fail(&["item", "link", short(&task)])
            .contains("missing link")
    );
}

#[test]
fn invite_device_prints_a_code_that_names_this_machine() {
    let sandbox = Sandbox::new();
    sandbox.run(&["init", "--name", "demo", "--no-agents"]);
    let invite = sandbox.json(&["invite", "device", "--expires", "1h"]);
    let code = invite["code"].as_str().unwrap();
    assert!(code.starts_with("wfi1"), "{code}");
    let log = sandbox.run(&["log"]);
    assert!(log.contains("InviteCreated"), "{log}");

    let parsed: wf_core::invite::InviteCode = code.parse().unwrap();
    let status = sandbox.json(&["status"]);
    assert_eq!(parsed.workforce.to_string(), status["id"].as_str().unwrap());
    assert_eq!(parsed.bootstrap.len(), 1);
}

#[test]
fn join_refuses_bad_codes_and_missing_names() {
    let sandbox = Sandbox::new();
    let error = sandbox.fail(&["join", "not-a-code", "--name", "vm"]);
    assert!(error.contains("invite code"), "{error}");
    sandbox.run(&["init", "--name", "demo", "--no-agents"]);
    let code = sandbox.json(&["invite", "device"])["code"]
        .as_str()
        .unwrap()
        .to_string();
    let error = sandbox.fail(&["join", &code]);
    assert!(error.contains("--name"), "{error}");
}

#[test]
fn devices_and_members_are_listed_and_editable() {
    let sandbox = Sandbox::new();
    sandbox.run(&[
        "init",
        "--name",
        "demo",
        "--no-agents",
        "--device-name",
        "laptop",
    ]);

    let devices = sandbox.json(&["device", "list"]);
    let devices = devices.as_array().unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0]["name"], "laptop");
    assert_eq!(devices[0]["this_device"], true);
    assert_eq!(devices[0]["principal"], "@joshua");

    sandbox.run(&[
        "device",
        "edit",
        "--name",
        "work-laptop",
        "--max-sessions",
        "3",
        "--agent",
        "coder=2",
    ]);
    let devices = sandbox.json(&["device", "list"]);
    assert_eq!(devices[0]["name"], "work-laptop");
    assert_eq!(devices[0]["max_sessions"], 3);
    assert_eq!(devices[0]["agents"]["coder"], 2);
    let text = sandbox.run(&["device", "list"]);
    assert!(text.contains("work-laptop (this)"), "{text}");

    let members = sandbox.json(&["members"]);
    assert_eq!(members[0]["handle"], "@joshua");
    assert_eq!(members[0]["role"], "owner");
    assert_eq!(members[0]["devices"][0], "work-laptop");

    let error = sandbox.fail(&["device", "edit"]);
    assert!(error.contains("nothing to change"), "{error}");
}

struct RunningDaemon {
    child: std::process::Child,
}

impl Sandbox {
    #[allow(clippy::zombie_processes)]
    fn start_daemon(&self) -> RunningDaemon {
        let child = self
            .command(&["daemon", "--no-network"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let socket = self._root.path().join("run/workforce/daemon.sock");
        for _ in 0..200 {
            if socket.exists() && std::os::unix::net::UnixStream::connect(&socket).is_ok() {
                return RunningDaemon { child };
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("the daemon did not start");
    }
}

impl RunningDaemon {
    fn stop(self) {
        drop(self);
    }
}

impl Drop for RunningDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn the_cli_gives_the_same_output_through_the_daemon_as_through_the_store() {
    let sandbox = Sandbox::new();
    sandbox.run(&[
        "init",
        "--name",
        "demo",
        "--no-agents",
        "--device-name",
        "laptop",
    ]);
    sandbox.add(&["--title", "Written before the daemon", "--priority", "P1"]);

    let daemon = sandbox.start_daemon();
    let through_daemon =
        sandbox.add(&["--title", "Written through the daemon", "--priority", "P2"]);
    let list = sandbox.json(&["item", "list"]);
    let status = sandbox.json(&["status"]);
    let item = sandbox.json(&["item", "show", short(&through_daemon)]);
    let log = sandbox.json(&["log"]);
    let devices = sandbox.json(&["device", "list"]);
    daemon.stop();

    assert_eq!(sandbox.json(&["item", "list"]), list);
    assert_eq!(sandbox.json(&["status"]), status);
    assert_eq!(
        sandbox.json(&["item", "show", short(&through_daemon)]),
        item
    );
    assert_eq!(sandbox.json(&["log"]), log);
    assert_eq!(sandbox.json(&["device", "list"]), devices);
    assert_eq!(list.as_array().unwrap().len(), 2);
}

#[test]
fn a_second_daemon_is_refused() {
    let sandbox = Sandbox::new();
    sandbox.run(&["init", "--name", "demo", "--no-agents"]);
    let daemon = sandbox.start_daemon();
    let error = sandbox.fail(&["daemon", "--no-network"]);
    assert!(error.contains("already listening"), "{error}");
    daemon.stop();
}

mod forking {
    use super::*;
    use wf_core::event::{DeviceBound, DeviceProfile, EventBody, InviteCreated, InviteKind};
    use wf_core::identity::{Actor, DeviceSigner, InviteSigner, PrincipalSigner, Signed};
    use wf_store::{RedbStore, Replica};

    fn read_key(path: &std::path::Path) -> [u8; 32] {
        let text = std::fs::read_to_string(path).unwrap();
        let bytes = data_encoding::HEXLOWER
            .decode(text.trim().as_bytes())
            .unwrap();
        bytes.try_into().unwrap()
    }

    /// Binds a second device named `name` by writing the events a join would, straight into the
    /// store of the sandbox.
    fn bind_second_device(sandbox: &Sandbox, name: &str) -> DeviceSigner {
        let id = sandbox.json(&["status"])["id"]
            .as_str()
            .unwrap()
            .to_string();
        let keys = sandbox.data.join("workforce/keys");
        let principal = PrincipalSigner::from_bytes(&read_key(
            &sandbox
                .data
                .join("workforce")
                .join(&id)
                .join("principal.key"),
        ))
        .unwrap();
        let device = DeviceSigner::from_bytes(&read_key(&keys.join("device.key"))).unwrap();
        let second = DeviceSigner::generate();
        let store = sandbox.data.join("workforce").join(&id).join("store.redb");
        let mut replica =
            Replica::open(RedbStore::open(store).unwrap(), id.parse().unwrap()).unwrap();
        let workforce = replica.state().workforce_id().unwrap();
        let actor = Actor::Human(principal.key());
        let secret = InviteSigner::generate();
        let invite = replica
            .author(
                &device,
                actor.clone(),
                wf_cli::workspace::now_ms(),
                EventBody::InviteCreated(
                    Signed::sign(
                        &principal,
                        Some(workforce),
                        InviteCreated {
                            kind: InviteKind::Device,
                            invite: secret.key(),
                            expires_ms: u64::MAX / 2,
                        },
                    )
                    .unwrap(),
                ),
            )
            .unwrap();
        let proof = secret.prove(workforce, invite.id(), second.key()).unwrap();
        replica
            .author(
                &second,
                actor.clone(),
                wf_cli::workspace::now_ms(),
                EventBody::DeviceBound(DeviceBound {
                    invite: invite.id(),
                    proof,
                }),
            )
            .unwrap();
        replica
            .author(
                &second,
                actor,
                wf_cli::workspace::now_ms(),
                EventBody::DeviceProfile(DeviceProfile {
                    name: name.into(),
                    hardware: "test".into(),
                    agents: Default::default(),
                    max_sessions: 1,
                }),
            )
            .unwrap();
        second
    }

    fn workforce_file(sandbox: &Sandbox) -> String {
        std::fs::read_to_string(sandbox.repository.join(".workforce.toml")).unwrap()
    }

    #[test]
    fn a_fork_without_a_lost_device_keeps_every_item_and_is_announced() {
        let sandbox = Sandbox::new();
        sandbox.run(&[
            "init",
            "--name",
            "demo",
            "--no-agents",
            "--device-name",
            "laptop",
        ]);
        sandbox.add(&["--title", "Try sync", "--priority", "P1"]);
        let parent_file = workforce_file(&sandbox);
        let parent = sandbox.json(&["status"])["id"]
            .as_str()
            .unwrap()
            .to_string();

        let fork = sandbox.json(&["fork", "--name", "demo v2"]);
        let child = fork["workforce"].as_str().unwrap().to_string();
        assert_ne!(child, parent);
        assert_eq!(fork["announced"], true);
        assert!(workforce_file(&sandbox).contains(&child));

        let status = sandbox.json(&["status"]);
        assert_eq!(status["name"], "demo v2");
        assert_eq!(status["forked_from"], parent.as_str());
        let items = sandbox.json(&["item", "list"]);
        assert_eq!(items.as_array().unwrap().len(), 1);
        assert_eq!(items[0]["title"], "Try sync");
        let list = sandbox.json(&["fork", "list"]);
        assert_eq!(list["parent"]["workforce"], parent.as_str());

        // Writing on in the fork continues the history.
        sandbox.add(&["--title", "After the fork", "--priority", "P2"]);
        assert_eq!(sandbox.json(&["item", "list"]).as_array().unwrap().len(), 2);

        // The parent knows about the fork, and stays as it was.
        std::fs::write(sandbox.repository.join(".workforce.toml"), parent_file).unwrap();
        let status = sandbox.run(&["status"]);
        assert!(
            status.contains(&format!("wf fork accept {child}")),
            "{status}"
        );
        assert_eq!(sandbox.json(&["item", "list"]).as_array().unwrap().len(), 1);
        let list = sandbox.json(&["fork", "list"]);
        assert_eq!(list["forks"][0]["workforce"], child.as_str());
    }

    #[test]
    fn the_daemon_founds_the_fork_and_the_history_keeps_its_edits() {
        let sandbox = Sandbox::new();
        sandbox.run(&["init", "--name", "demo", "--no-agents"]);
        let item = sandbox.add(&["--title", "Original", "--priority", "P1"]);
        sandbox.run(&["item", "edit", short(&item), "--title", "Renamed"]);
        sandbox.run(&["comment", short(&item), "A note"]);

        let daemon = sandbox.start_daemon();
        let fork = sandbox.json(&["fork"]);
        let child = fork["workforce"].as_str().unwrap().to_string();
        let through_daemon = sandbox.json(&["item", "show", short(&item)]);
        assert_eq!(through_daemon["title"], "Renamed");
        assert_eq!(through_daemon["notes"].as_array().unwrap().len(), 1);
        sandbox.add(&["--title", "After the fork", "--priority", "P2"]);
        let log = sandbox.json(&["log"]);
        let unverified = log["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["verified"] != true)
            .count();
        assert_eq!(unverified, 0, "{log}");
        daemon.stop();
        assert_eq!(sandbox.json(&["log"]), log, "the same through the store");

        // The same through the store of the fork, which the daemon had created.
        assert_eq!(sandbox.json(&["status"])["id"], child.as_str());
        let from_store = sandbox.json(&["item", "show", short(&item)]);
        assert_eq!(from_store["title"], "Renamed");
        assert_eq!(sandbox.json(&["item", "list"]).as_array().unwrap().len(), 2);
    }

    #[test]
    fn excluding_a_device_leaves_it_out_but_keeps_its_items() {
        let sandbox = Sandbox::new();
        sandbox.run(&[
            "init",
            "--name",
            "demo",
            "--no-agents",
            "--device-name",
            "laptop",
        ]);
        sandbox.add(&["--title", "From the laptop", "--priority", "P2"]);
        bind_second_device(&sandbox, "vm");
        assert_eq!(
            sandbox.json(&["device", "list"]).as_array().unwrap().len(),
            2
        );

        let error = sandbox.fail(&["fork", "--exclude", "laptop"]);
        assert!(error.contains("cannot leave out"), "{error}");
        let error = sandbox.fail(&["fork", "--exclude", "nonsense"]);
        assert!(error.contains("no device"), "{error}");

        let fork = sandbox.json(&["fork", "--exclude", "vm"]);
        assert_eq!(fork["excluded_devices"].as_array().unwrap().len(), 1);
        let devices = sandbox.json(&["device", "list"]);
        assert_eq!(devices.as_array().unwrap().len(), 1);
        assert_eq!(devices[0]["name"], "laptop");
        assert_eq!(sandbox.json(&["item", "list"]).as_array().unwrap().len(), 1);
    }

    #[test]
    fn a_new_principal_gives_the_fork_its_own_key_and_leaves_the_parent_alone() {
        let sandbox = Sandbox::new();
        sandbox.run(&[
            "init",
            "--name",
            "demo",
            "--no-agents",
            "--device-name",
            "laptop",
        ]);
        sandbox.add(&["--title", "Written with the old key", "--priority", "P2"]);
        let parent = sandbox.json(&["status"])["id"]
            .as_str()
            .unwrap()
            .to_string();
        let key_of = |id: &str| {
            std::fs::read_to_string(
                sandbox
                    .data
                    .join("workforce")
                    .join(id)
                    .join("principal.key"),
            )
            .unwrap()
        };
        let old_key = key_of(&parent);

        let fork = sandbox.json(&["fork", "--new-principal"]);
        assert_eq!(fork["announced"], false);
        let child = fork["workforce"].as_str().unwrap().to_string();
        assert_ne!(old_key, key_of(&child));
        assert_eq!(old_key, key_of(&parent), "the parent keeps its key");

        let item = sandbox.json(&["item", "list"]);
        let id = item[0]["id"].as_str().unwrap().to_string();
        let shown = sandbox.run(&["item", "show", short(&id)]);
        assert!(shown.contains("@joshua"), "{shown}");
        let members = sandbox.json(&["members"]);
        assert_eq!(members[0]["handle"], "@joshua");
        // The new key works for what needs a principal.
        sandbox.run(&["invite", "device"]);

        // And the other workforce on this machine still has its own.
        std::fs::write(
            sandbox.repository.join(".workforce.toml"),
            format!("id = \"{parent}\"\n"),
        )
        .unwrap();
        sandbox.run(&["invite", "device"]);
    }

    #[test]
    fn a_fork_without_a_new_principal_keeps_the_key_of_its_parent() {
        let sandbox = Sandbox::new();
        sandbox.run(&["init", "--name", "demo", "--no-agents"]);
        let parent = sandbox.json(&["status"])["id"]
            .as_str()
            .unwrap()
            .to_string();
        let child = sandbox.json(&["fork"])["workforce"]
            .as_str()
            .unwrap()
            .to_string();
        let key_of = |id: &str| {
            std::fs::read_to_string(
                sandbox
                    .data
                    .join("workforce")
                    .join(id)
                    .join("principal.key"),
            )
            .unwrap()
        };
        assert_eq!(key_of(&parent), key_of(&child));
    }
}

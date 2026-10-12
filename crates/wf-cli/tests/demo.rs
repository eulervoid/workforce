//! Runs the real `wf` binary through the demo script of the M0 plan.

use std::os::unix::fs::PermissionsExt;

use serde_json::Value;

mod common;

use common::{Sandbox, comments, short};

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
    let first = wf.add(&["--title", "Pick a hash for event ids"]);

    let link = wf.run(&["item", "link", short(&task), "--depends-on", short(&first)]);
    assert!(link.contains("now depends on"));
    let list = wf.run(&["item", "list"]);
    assert!(
        list.contains("BLOCKING") && list.contains("(task:open)"),
        "{list}"
    );
    assert_eq!(
        titles(&wf.json(&["item", "list", "--ready"])),
        ["Pick a hash for event ids"]
    );
    let ready = wf.run(&["item", "list", "-r"]);
    assert!(ready.contains("BLOCKING"), "{ready}");
    assert_eq!(
        titles(&wf.json(&["item", "list", "--blocked"])),
        ["Implement feed sync"]
    );
    assert!(titles(&wf.json(&["item", "list", "--stranded"])).is_empty());
    assert_eq!(
        titles(&wf.json(&["item", "list", "--ready", "--blocked"])).len(),
        2
    );

    wf.run(&["item", "move", short(&first), "dropped"]);
    assert_eq!(
        titles(&wf.json(&["item", "list", "--stranded"])),
        ["Implement feed sync"]
    );
    assert!(titles(&wf.json(&["item", "list", "--blocked"])).is_empty());
    assert!(wf.run(&["item", "list", "--stranded"]).contains('⚠'));
    let by_readiness = wf.json(&["item", "list", "--group-by", "readiness"]);
    let groups: Vec<&str> = by_readiness
        .as_array()
        .unwrap()
        .iter()
        .map(|group| group["group"].as_str().unwrap())
        .collect();
    assert_eq!(groups, ["stranded", "not_open"]);
    assert_eq!(titles(&by_readiness[0]["items"]), ["Implement feed sync"]);
    let by_state = wf.run(&["item", "list", "--group-by", "state"]);
    assert!(
        by_state.contains("open (1)") && by_state.contains("dropped (1)"),
        "{by_state}"
    );
    wf.run(&["item", "move", short(&first), "open"]);

    wf.run(&["item", "move", short(&first), "done"]);
    assert_eq!(
        titles(&wf.json(&["item", "list", "--ready"])),
        ["Implement feed sync"]
    );

    wf.run(&[
        "comment",
        short(&task),
        &format!("see #{}, @coder please keep it small", short(&first)),
    ]);
    let shown = wf.json(&["item", "show", short(&task)]);
    assert_eq!(shown["references"][0]["id"], first);
    assert_eq!(shown["mentions"][0], "@joshua/coder");
    assert_eq!(
        comments(&shown)[0]["text"],
        format!("see #{}, @coder please keep it small", short(&first))
    );
    assert_eq!(
        wf.json(&["item", "show", short(&first)])["referenced_by"][0]["item"],
        task
    );
    let mentions = wf.run(&["mentions", "@coder"]);
    assert!(mentions.contains("Implement feed sync"), "{mentions}");

    wf.run(&["item", "move", short(&task), "done"]);
    wf.run(&["item", "archive", short(&task)]);
    assert_eq!(
        titles(&wf.json(&["item", "list"])),
        ["Pick a hash for event ids"]
    );
    let with_archived = wf.json(&["item", "list", "--archived"]);
    assert_eq!(titles(&with_archived).len(), 2);
    let stub = wf.run(&["item", "show", short(&task)]);
    assert!(stub.contains("archived"), "{stub}");

    let log = wf.run(&["log"]);
    assert!(log.contains("events verified"), "{log}");
    let events = wf.json(&["log"])["events"].as_array().unwrap().len();
    // genesis, device profile, two agents, then the items
    assert_eq!(events, 1 + 1 + 2 + 2 + 1 + 1 + 1 + 1 + 1 + 2);
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
    let shown = wf.json(&["item", "show", short(&task)]);
    assert_eq!(shown["title"], "Edited in editor");
    assert_eq!(shown["priority"], "P0");
    let activity = shown["activity"].as_array().unwrap();
    assert!(activity.iter().any(|entry| {
        entry["summary"]
            .as_str()
            .unwrap()
            .contains("renamed it to \"New title\"")
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
    assert_eq!(agents[0]["start"], "manual");
    assert_eq!(agents[0]["permissions"], "allow");
    wf.run(&[
        "agent",
        "edit",
        "reviewer",
        "--start",
        "auto",
        "--park-timeout",
        "30m",
        "--max-duration",
        "1h",
    ]);
    let agents = wf.json(&["agent", "list"]);
    assert_eq!(agents[0]["model"], "big");
    assert_eq!(agents[0]["start"], "auto");
    assert_eq!(agents[0]["permissions"], "allow");
    assert_eq!(agents[0]["park_timeout"], "30m");
    assert_eq!(agents[0]["max_duration"], "1h");
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

    std::fs::write(&file, policy.replace("\"ratified\"", "\"accepted\"")).unwrap();
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
        "#!/bin/sh\n/usr/bin/sed -i.bak -e 's/^title = .*/title = \"From the editor\"/' -e 's/^type = .*/type = \"decision\"/' -e '/^priority = /d' \"$1\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o755)).unwrap();
    let output = wf
        .command(&["item", "add", "--edit", "--json"])
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
    assert_eq!(created["data"]["type"], "decision");
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
fn blockers_and_change_requests_stay_open_until_a_comment_resolves_them() {
    let wf = Sandbox::new();
    wf.run(&["init", "--name", "demo", "--no-agents"]);
    let task = wf.add(&["--title", "a task"]);
    let comment = |args: &[&str]| {
        let mut all = vec!["comment", short(&task)];
        all.extend_from_slice(args);
        wf.json(&all)["event"].as_str().unwrap().to_string()
    };
    let blocker = comment(&["Which crate?", "--block"]);
    comment(&["Handle empty input", "--request-change"]);
    comment(&["Also negative numbers", "--request-change"]);
    comment(&["Just a remark"]);

    let open = |shown: &Value| -> Vec<String> {
        shown["open"]
            .as_array()
            .unwrap()
            .iter()
            .map(|comment| comment["kind"].as_str().unwrap().to_string())
            .collect()
    };
    let shown = wf.json(&["item", "show", short(&task)]);
    assert_eq!(
        open(&shown),
        ["blocker", "change_request", "change_request"],
        "{shown}"
    );
    let printed = wf.run(&["item", "show", short(&task)]);
    assert!(printed.contains("3 open:"), "{printed}");
    assert!(printed.contains("⧗ blocker"), "{printed}");

    comment(&["Use humantime", "--resolve", &blocker[..4]]);
    assert_eq!(
        open(&wf.json(&["item", "show", short(&task)])),
        ["change_request", "change_request"]
    );
    assert!(
        wf.fail(&["comment", short(&task), "again", "--resolve", &blocker[..4]])
            .contains("resolved already")
    );
    assert!(
        wf.fail(&["comment", short(&task), "--resolve-blockers"])
            .contains("no open blockers")
    );

    comment(&["--resolve-change-requests"]);
    let shown = wf.json(&["item", "show", short(&task)]);
    assert!(open(&shown).is_empty(), "{shown}");
    let resolving = comments(&shown).as_array().unwrap().last().unwrap().clone();
    assert_eq!(
        resolving["resolves"].as_array().unwrap().len(),
        2,
        "{resolving}"
    );
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
        let socket = self.root.path().join("run/workforce/daemon.sock");
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
        assert_eq!(comments(&through_daemon).as_array().unwrap().len(), 1);
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

#[test]
fn steps_are_checked_and_unchecked_by_number() {
    let wf = Sandbox::new();
    wf.run(&["init", "--name", "demo", "--no-agents"]);
    let task = wf.add(&[
        "--title",
        "Build it",
        "--body",
        "## Plan\n\n- [ ] write parser\n- [ ] add tests\n- [ ] ship",
    ]);
    let steps = |wf: &Sandbox| -> Vec<bool> {
        wf.json(&["item", "show", short(&task)])["body"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|part| part["kind"] == "step")
            .map(|part| part["done"].as_bool().unwrap())
            .collect()
    };
    assert_eq!(steps(&wf), [false, false, false]);

    let output = wf.run(&["item", "check", short(&task), "1", "3"]);
    assert!(output.contains("checked [1] write parser, [3] ship (2/3 done)"));
    assert_eq!(steps(&wf), [true, false, true]);

    let output = wf.run(&["item", "check", short(&task), "1"]);
    assert!(output.contains("nothing changed: [1] write parser already checked"));

    let output = wf.run(&["item", "check", short(&task), "3", "--remove"]);
    assert!(output.contains("unchecked [3] ship (1/3 done)"));
    assert_eq!(steps(&wf), [true, false, false]);

    let shown = wf.run(&["item", "show", short(&task)]);
    assert!(shown.contains("- [x] 1. write parser") && shown.contains("- [ ] 2. add tests"));
    assert!(shown.contains("steps 1/3"), "{shown}");
    let listed = wf.run(&["item", "list"]);
    assert!(listed.contains("1/3"), "{listed}");
    let json = wf.json(&["item", "list"]);
    assert_eq!(json[0]["steps"]["done"], 1);
    assert_eq!(json[0]["steps"]["total"], 3);

    assert!(
        wf.fail(&["item", "check", short(&task), "4"])
            .contains("has 3 steps, there is no step 4")
    );
    assert!(
        wf.fail(&[
            "item",
            "edit",
            short(&task),
            "--body",
            "- [ ] same\n- [ ] same"
        ])
        .contains("appears twice")
    );
}

mod work {
    use super::*;
    use wf_cli::keys::DataDir;
    use wf_cli::workspace::Workspace;
    use wf_core::event::{EventBody, SessionStarted};
    use wf_core::name::AgentName;
    use wf_core::session::SessionRole;

    /// Starts a session of `coder` on the task the way the daemon will, and returns its id.
    fn start_session(wf: &Sandbox, task: &str) -> String {
        let mut workspace = Workspace::open(
            DataDir::new(wf.data.join("workforce")),
            &wf.repository,
            false,
        )
        .unwrap();
        let item = workspace.resolve_item(task).unwrap();
        let started = workspace
            .author(EventBody::SessionStarted(SessionStarted {
                agent: AgentName::new("coder").unwrap(),
                item,
                role: SessionRole::Work,
            }))
            .unwrap();
        wf_core::id::SessionId::from(started.id()).to_string()
    }

    fn as_session(wf: &Sandbox, session: &str, args: &[&str]) -> std::process::Output {
        wf.command(args)
            .env("WF_SESSION", session)
            .output()
            .unwrap()
    }

    #[test]
    fn an_agent_comments_on_its_item_and_on_its_steps() {
        let wf = Sandbox::new();
        wf.run(&["init", "--name", "demo", "--no-agents"]);
        wf.run(&["agent", "add", "coder", "--harness", "opencode"]);
        let task = wf.add(&[
            "--title",
            "Build it",
            "--body",
            "- [ ] pick storage\n- [ ] write it",
        ]);
        let session = start_session(&wf, short(&task));

        let output = as_session(&wf, &session, &["comment", short(&task), "halfway there"]);
        assert!(output.status.success());
        let output = as_session(
            &wf,
            &session,
            &[
                "comment",
                short(&task),
                "chose redb over sqlite",
                "--step",
                "1",
                "--done",
            ],
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );

        let shown = wf.json(&["item", "show", short(&task)]);
        let steps: Vec<bool> = shown["body"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|part| part["kind"] == "step")
            .map(|part| part["done"].as_bool().unwrap())
            .collect();
        assert_eq!(steps, [true, false]);
        let comments = comments(&shown);
        let reports: Vec<&Value> = comments
            .as_array()
            .unwrap()
            .iter()
            .filter(|note| note["session"].is_string())
            .collect();
        assert_eq!(reports.len(), 2);
        let on_step = reports
            .iter()
            .find(|note| note["step"].is_object())
            .expect("a report on a step");
        assert_eq!(on_step["text"], "chose redb over sqlite");
        assert_eq!(on_step["step"]["number"], 1);
        assert!(
            on_step["session"]
                .as_str()
                .unwrap()
                .starts_with("coder work")
        );

        let text = wf.run(&["item", "show", short(&task)]);
        assert!(text.contains("on step 1"), "{text}");

        wf.run(&[
            "item",
            "edit",
            short(&task),
            "--body",
            "- [ ] pick a database",
        ]);
        let text = wf.run(&["item", "show", short(&task)]);
        assert!(text.contains("chose redb over sqlite"), "{text}");
        assert!(text.contains("a step since edited"), "{text}");
    }

    #[test]
    fn a_comment_names_a_step_that_exists() {
        let wf = Sandbox::new();
        wf.run(&["init", "--name", "demo", "--no-agents"]);
        wf.run(&["agent", "add", "coder", "--harness", "opencode"]);
        let task = wf.add(&["--title", "Build it", "--body", "- [ ] only step"]);
        let session = start_session(&wf, short(&task));
        let output = as_session(
            &wf,
            &session,
            &["comment", short(&task), "x", "--step", "2"],
        );
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("has 1 steps, there is no step 2")
        );
        let output = as_session(&wf, &session, &["comment", short(&task), "x", "--done"]);
        assert!(!output.status.success(), "--done needs --step");
    }
}

#[test]
fn items_name_a_review_level_from_the_policy() {
    let wf = Sandbox::new();
    wf.run(&["init", "--name", "demo", "--no-agents"]);
    let task = wf.add(&["--title", "Careful", "--review", "standard"]);
    let review = |wf: &Sandbox| wf.json(&["item", "show", short(&task)])["review"].clone();
    assert_eq!(review(&wf), "standard");
    assert!(
        wf.run(&["item", "show", short(&task)])
            .contains("review=standard")
    );

    wf.run(&["item", "edit", short(&task), "--review", "critical"]);
    assert_eq!(review(&wf), "critical");
    wf.run(&["item", "edit", short(&task), "--review", "default"]);
    assert_eq!(review(&wf), "principal", "the task type's default applies");
    wf.run(&["item", "edit", short(&task), "--review", "none"]);
    assert_eq!(review(&wf), "none");

    let message = wf.fail(&["item", "add", "--title", "x", "--review", "ghost"]);
    assert!(message.contains("no review level `ghost`"), "{message}");
    let message = wf.fail(&["item", "edit", short(&task), "--review", "ghost"]);
    assert!(message.contains("no review level `ghost`"), "{message}");
    let message = wf.fail(&[
        "item", "add", "--type", "distill", "--title", "x", "--review", "light",
    ]);
    assert!(message.contains("no review state"), "{message}");
}

#[test]
fn permissions_test_runs_the_deciders_of_the_machine_config() {
    let wf = Sandbox::new();
    let allowed = wf.json(&["permissions", "test", "--command", "wf item list | head"]);
    assert_eq!(allowed["permission"], "allow");
    assert_eq!(allowed["by"], "rules");

    let config = wf.root.path().join("config/workforce");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("config.toml"),
        r#"
[[permissions.decider]]
kind = "rules"

[[permissions.decider]]
kind = "hook"
command = ["/bin/sh", "-c", "echo '{\"permission\": \"deny\", \"reason\": \"never rm\"}'"]
"#,
    )
    .unwrap();
    let denied = wf.run(&["permissions", "test", "--command", "rm -rf /"]);
    assert_eq!(
        denied,
        "passed by rules: no rule covers it\ndenied by hook /bin/sh: never rm\n"
    );

    std::fs::write(
        config.join("config.toml"),
        "[[permissions.decider]]\nkind = \"shell\"\n",
    )
    .unwrap();
    let broken = wf.fail(&["permissions", "test", "--command", "ls"]);
    assert!(broken.contains("config.toml"), "{broken}");
}

#[test]
fn an_agent_that_starts_automatically_is_hosted_here_and_status_flags_one_that_is_not() {
    let wf = Sandbox::new();
    wf.json(&["init", "--name", "demo", "--no-agents"]);
    let added = wf.run(&[
        "agent",
        "add",
        "worker",
        "--harness",
        "claude-code",
        "--start",
        "auto",
    ]);
    assert!(added.contains("this device runs it"), "{added}");
    let devices = wf.json(&["device", "list"]);
    assert_eq!(devices[0]["agents"]["worker"], 1, "{devices}");
    let status = wf.json(&["status"]);
    assert!(status["flagged"].as_array().unwrap().is_empty(), "{status}");

    let manual = wf.run(&["agent", "add", "helper", "--harness", "claude-code"]);
    assert!(!manual.contains("runs it"), "{manual}");

    // Hosting something else replaces the list, which leaves the worker with no device.
    wf.run(&["device", "edit", "--agent", "helper=1"]);
    let flagged = wf.json(&["status"])["flagged"].to_string();
    assert!(flagged.contains("no device hosts it"), "{flagged}");
}

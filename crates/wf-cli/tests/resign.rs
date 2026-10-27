//! `wf import --resign`: the export of a fork by event version 1 becomes one plain workforce of
//! the current version. Delete with `wf_core::resign` once no log of version 1 is left.

mod common;

use std::path::{Path, PathBuf};

use common::{Sandbox, comments, short};
use serde_json::Value;
use wf_core::id::{ItemId, SessionId};
use wf_core::identity::{Actor, DeviceSigner, PrincipalSigner};
use wf_core::session::EndReason;
use wf_store::{RedbStore, Replica};

fn read_key(path: &Path) -> [u8; 32] {
    let text = std::fs::read_to_string(path).unwrap();
    let bytes = data_encoding::HEXLOWER
        .decode(text.trim().as_bytes())
        .unwrap();
    bytes.try_into().unwrap()
}

fn workforce_id(sandbox: &Sandbox) -> String {
    sandbox.json(&["status"])["id"]
        .as_str()
        .unwrap()
        .to_string()
}

fn workforce_directory(sandbox: &Sandbox, id: &str) -> PathBuf {
    sandbox.data.join("workforce").join(id)
}

/// Starts and ends a session of `agent` on `item` by writing the events the daemon would,
/// straight into the store of the sandbox.
fn ended_session(sandbox: &Sandbox, agent: &str, item: ItemId) -> SessionId {
    let id = workforce_id(sandbox);
    let directory = workforce_directory(sandbox, &id);
    let principal =
        PrincipalSigner::from_bytes(&read_key(&directory.join("principal.key"))).unwrap();
    let device =
        DeviceSigner::from_bytes(&read_key(&sandbox.data.join("workforce/keys/device.key")))
            .unwrap();
    let mut replica = Replica::open(
        RedbStore::open(directory.join("store.redb")).unwrap(),
        id.parse().unwrap(),
    )
    .unwrap();
    let actor = Actor::Principal(principal.key());
    let started = replica
        .author(
            &device,
            actor.clone(),
            wf_store::now_ms(),
            wf_core::testing::start_session(agent, item),
        )
        .unwrap();
    let session = SessionId::from(started.id());
    replica
        .author(
            &device,
            actor,
            wf_store::now_ms(),
            wf_core::testing::end_session(session, EndReason::Released),
        )
        .unwrap();
    session
}

/// What an export by version 1 showed where the current one differs: the version, the kind of
/// a policy set, an item created without a state, and the old names of states.
fn as_version_one(export: &str, created_without_state: &str) -> String {
    let mut text = String::new();
    for line in export.lines() {
        let mut event: Value = serde_json::from_str(line).unwrap();
        event["content"]["v"] = 1.into();
        if event["kind"] == "PolicySet" {
            event["kind"] = "BoundPolicySet".into();
        }
        let kind = event["kind"].clone();
        let body = &mut event["content"]["body"];
        if kind == "ItemCreated" && body["title"] == created_without_state {
            body.as_object_mut().unwrap().remove("state");
        }
        if kind == "ItemMoved" && body["to"] == "done" {
            body["to"] = "ratified".into();
        }
        text.push_str(&serde_json::to_string(&event).unwrap());
        text.push('\n');
    }
    text
}

fn item_by_title<'a>(list: &'a Value, title: &str) -> &'a Value {
    list.as_array()
        .unwrap()
        .iter()
        .find(|item| item["title"] == title)
        .unwrap_or_else(|| panic!("no item {title} in {list}"))
}

/// Title, type, state and priority of every item, in the order of their titles.
fn summaries(list: &Value) -> Vec<(String, String, String, String)> {
    let mut summaries: Vec<_> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            (
                item["title"].as_str().unwrap().to_string(),
                item["type"].as_str().unwrap().to_string(),
                item["state"].as_str().unwrap().to_string(),
                item["priority"].to_string(),
            )
        })
        .collect();
    summaries.sort();
    summaries
}

#[test]
fn the_export_of_a_fork_is_signed_again_as_one_workforce_with_its_local_data() {
    let sandbox = Sandbox::new();
    wf_vcs::testing::init_repository(&sandbox.repository);
    sandbox.run(&["init", "--name", "demo", "--device-name", "laptop"]);
    sandbox.run(&["agent", "add", "coder", "--harness", "opencode"]);
    let first = sandbox.add(&["--title", "First", "--priority", "P1"]);
    let second = sandbox.add(&[
        "--title",
        "Second",
        "--body",
        &format!("after #{}\n\n- [ ] one step", short(&first)),
    ]);
    sandbox.run(&["item", "done", short(&second)]);
    let session = ended_session(&sandbox, "coder", first.parse().unwrap());

    sandbox.run(&["fork", "--name", "demo v2"]);
    let fork = workforce_id(&sandbox);
    sandbox.run(&[
        "comment",
        short(&first),
        &format!("see #{}", short(&second)),
    ]);
    let comment = comments(&sandbox.json(&["item", "show", short(&first)]))[0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    sandbox.add(&[
        "--title",
        "Third",
        "--body",
        &format!("like #{}", short(&first)),
    ]);
    let policy = sandbox.run(&["policy", "show"]);
    let file = sandbox.root.path().join("policy.toml");
    std::fs::write(&file, format!("{policy}\n# rewritten\n")).unwrap();
    sandbox.run(&["policy", "apply", file.to_str().unwrap()]);
    let items = sandbox.json(&["item", "list"]);

    // What this machine keeps of the fork by id.
    let old_directory = workforce_directory(&sandbox, &fork);
    let first_name = &first[..12];
    let bare = wf_vcs::GitCli::open(&old_directory.join("git")).unwrap();
    let main = bare.branch_head("main").unwrap();
    let first_item: ItemId = first.parse().unwrap();
    bare.create_agent_repository(
        &old_directory.join("repos").join(first_name),
        &wf_vcs::item_branch(first_item),
        main,
    )
    .unwrap();
    bare.ensure_item_branch(first_item, main).unwrap();
    std::fs::create_dir_all(
        old_directory
            .join("repos")
            .join(format!("{first_name}-review-x")),
    )
    .unwrap();
    let transcripts = old_directory.join("transcripts");
    std::fs::create_dir_all(&transcripts).unwrap();
    std::fs::write(transcripts.join(format!("{session}.jsonl")), "said\n").unwrap();
    std::fs::write(
        transcripts.join(format!("{session}.agent-session.json")),
        serde_json::json!({ "id": "agent-1", "prompted_ms": 1, "model": "m", "told": [comment] })
            .to_string(),
    )
    .unwrap();
    std::fs::write(transcripts.join("stray.jsonl"), "").unwrap();
    std::fs::create_dir_all(old_directory.join("check-logs")).unwrap();
    std::fs::write(old_directory.join("check-logs").join("tree.log"), "ok\n").unwrap();

    let export = as_version_one(&sandbox.run(&["export"]), "First");
    let file = sandbox.root.path().join("export.jsonl");
    std::fs::write(&file, export).unwrap();
    let resigned = sandbox.json(&["import", "--resign", file.to_str().unwrap()]);

    let new = resigned["workforce"].as_str().unwrap().to_string();
    assert_eq!(resigned["old_workforce"], fork.as_str());
    assert_eq!(workforce_id(&sandbox), new, "{resigned}");
    assert_eq!(resigned["dropped_events"]["Genesis"], 2, "{resigned}");
    assert_eq!(
        resigned["dropped_events"]["BoundPolicySet"], 1,
        "{resigned}"
    );

    // One plain workforce, with the items the fork had.
    let status = sandbox.json(&["status"]);
    assert_eq!(status["name"], "demo v2");
    assert_eq!(status["forked_from"], Value::Null, "{status}");
    let resigned_items = sandbox.json(&["item", "list"]);
    assert_eq!(summaries(&resigned_items), summaries(&items));
    assert!(
        sandbox.run(&["policy", "show"]).contains("# rewritten"),
        "the latest policy is the policy of the new genesis"
    );

    // References name the new ids.
    let new_first = item_by_title(&resigned_items, "First")["id"]
        .as_str()
        .unwrap()
        .to_string();
    let new_second = item_by_title(&resigned_items, "Second")["id"]
        .as_str()
        .unwrap()
        .to_string();
    let shown = sandbox.json(&["item", "show", short(&new_second)]);
    assert_eq!(
        shown["body"][0]["text"],
        format!("after #{}", short(&new_first))
    );
    assert_eq!(shown["body"][1]["text"], "one step");
    let shown = comments(&sandbox.json(&["item", "show", short(&new_first)]));
    assert_eq!(shown[0]["text"], format!("see #{}", short(&new_second)));
    let new_comment = shown[0]["id"].as_str().unwrap();

    // The map of ids beside the store.
    let new_directory = workforce_directory(&sandbox, &new);
    let map = std::fs::read_to_string(new_directory.join("resigned-ids.tsv")).unwrap();
    assert!(
        map.contains(&format!("{fork}\t{new}\tWorkforce\n")),
        "{map}"
    );
    assert!(
        map.contains(&format!("{first}\t{new_first}\tItemCreated\n")),
        "{map}"
    );
    let new_session = map
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{session}\t")))
        .and_then(|rest| rest.strip_suffix("\tSessionStarted"))
        .unwrap_or_else(|| panic!("no session in {map}"));

    // The local data, moved under the new ids.
    let new_first_name = &new_first[..12];
    let repository = new_directory.join("repos").join(new_first_name);
    assert_eq!(
        wf_vcs::agent_branch_head(
            &repository,
            &wf_vcs::item_branch(new_first.parse().unwrap())
        ),
        Some(main)
    );
    let new_bare = wf_vcs::GitCli::open(&new_directory.join("git")).unwrap();
    assert_eq!(
        new_bare
            .branch_head(&wf_vcs::item_branch(new_first.parse().unwrap()))
            .unwrap(),
        main
    );
    let new_transcripts = new_directory.join("transcripts");
    assert_eq!(
        std::fs::read_to_string(new_transcripts.join(format!("{new_session}.jsonl"))).unwrap(),
        "said\n"
    );
    let record: Value = serde_json::from_str(
        &std::fs::read_to_string(new_transcripts.join(format!("{new_session}.agent-session.json")))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(record["told"], serde_json::json!([new_comment]));
    assert_eq!(
        std::fs::read_to_string(new_directory.join("check-logs/tree.log")).unwrap(),
        "ok\n"
    );
    let dropped = resigned["dropped"].to_string();
    assert!(
        dropped.contains(&format!("repos/{first_name}-review-x")),
        "{dropped}"
    );
    assert!(dropped.contains("transcripts/stray.jsonl"), "{dropped}");

    // The old directory is the backup, and a second import does not overwrite the first.
    assert!(old_directory.join("store.redb").is_file());
    assert!(old_directory.join("repos").join(first_name).is_dir());
    let again = sandbox.fail(&["import", "--resign", file.to_str().unwrap()]);
    assert!(again.contains("exists already"), "{again}");
}

//! A scripted ACP agent for tests: answers one prompt with a message, a tool call and a
//! permission request, and reports the permission outcome in its last message. Its sessions
//! offer two models. `FAKE_AGENT_REOPENS` set to `resume` or `load` offers that way back into a
//! session; a load first replays an earlier message. `FAKE_AGENT_DESCRIPTORS` names a file it
//! writes the descriptors it holds to when it starts, one per line. `FAKE_AGENT_SILENT` set makes
//! it never answer a prompt, nor say anything while it does not. `FAKE_AGENT_HANG` set makes it
//! wait on a prompt until it is cancelled, and then end the turn as cancelled.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

fn send(message: Value) {
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{message}").unwrap();
    stdout.flush().unwrap();
}

fn update(update: Value) {
    send(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {"sessionId": "fake", "update": update},
    }));
}

fn chunk(text: &str) {
    update(json!({
        "sessionUpdate": "agent_message_chunk",
        "content": {"type": "text", "text": text},
    }));
}

/// The options of a session: its model, set to `model`.
fn config_options(model: &str) -> Value {
    json!([{
        "id": "model", "name": "Model", "category": "model",
        "type": "select", "currentValue": model,
        "options": [
            {"value": "fast", "name": "Fast"},
            {"value": "deep", "name": "Deep"},
        ],
    }])
}

/// The descriptors this process holds. They are looked up one by one, as reading `/dev/fd`
/// would hold one more, and by duplicating each: on macOS `stat` on `/dev/fd/N` now and then
/// fails with EBADF for a pipe that is open.
fn descriptors() -> Vec<String> {
    (0..4096)
        .filter(|&descriptor| {
            // SAFETY: the descriptor is only duplicated, and the duplicate closed right away.
            unsafe { std::os::fd::BorrowedFd::borrow_raw(descriptor) }
                .try_clone_to_owned()
                .is_ok()
        })
        .map(|descriptor| descriptor.to_string())
        .collect()
}

fn main() {
    if let Ok(path) = std::env::var("FAKE_AGENT_DESCRIPTORS") {
        std::fs::write(path, descriptors().join("\n")).unwrap();
    }
    let reopens = std::env::var("FAKE_AGENT_REOPENS").unwrap_or_default();
    let silent = std::env::var_os("FAKE_AGENT_SILENT").is_some();
    let hang = std::env::var_os("FAKE_AGENT_HANG").is_some();
    let mut lines = std::io::stdin().lock().lines();
    while let Some(Ok(line)) = lines.next() {
        let request: Value = serde_json::from_str(&line).unwrap();
        let id = request["id"].clone();
        match request["method"].as_str() {
            Some("initialize") => send(json!({
                "jsonrpc": "2.0", "id": id,
                "result": {"protocolVersion": 1, "agentCapabilities": {
                    "loadSession": reopens == "load",
                    "sessionCapabilities": if reopens == "resume" { json!({"resume": {}}) } else { json!({}) },
                }},
            })),
            Some("session/resume") => send(json!({"jsonrpc": "2.0", "id": id, "result": {}})),
            Some("session/load") => {
                chunk("said before. ");
                send(json!({"jsonrpc": "2.0", "id": id, "result": {}}));
            }
            Some("session/new") => send(json!({
                "jsonrpc": "2.0", "id": id,
                "result": {"sessionId": "fake", "configOptions": config_options("fast")},
            })),
            Some("session/set_config_option") => send(json!({
                "jsonrpc": "2.0", "id": id,
                "result": {"configOptions": config_options(request["params"]["value"].as_str().unwrap())},
            })),
            Some("session/prompt") if silent => {}
            Some("session/prompt") if hang => {
                for line in lines.by_ref() {
                    let message: Value = serde_json::from_str(&line.unwrap()).unwrap();
                    if message["method"] == "session/cancel" {
                        break;
                    }
                }
                send(json!({
                    "jsonrpc": "2.0", "id": id, "result": {"stopReason": "cancelled"},
                }));
            }
            Some("session/prompt") => {
                eprintln!("fake agent got a prompt");
                chunk("Working on it. ");
                update(json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": "call-1",
                    "title": "Edit file",
                    "status": "pending",
                }));
                send(json!({
                    "jsonrpc": "2.0", "id": 100, "method": "session/request_permission",
                    "params": {
                        "sessionId": "fake",
                        "toolCall": {"toolCallId": "call-1", "title": "Edit file"},
                        "options": [
                            {"optionId": "yes", "name": "Yes", "kind": "allow_once"},
                            {"optionId": "no", "name": "No", "kind": "reject_once"},
                        ],
                    },
                }));
                let reply: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
                let chosen = reply["result"]["outcome"]["optionId"]
                    .as_str()
                    .unwrap_or("cancelled");
                chunk(&format!("permission: {chosen}"));
                send(json!({
                    "jsonrpc": "2.0", "id": id, "result": {"stopReason": "end_turn"},
                }));
            }
            _ => {}
        }
    }
}

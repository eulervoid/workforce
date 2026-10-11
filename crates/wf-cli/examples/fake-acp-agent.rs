//! A scripted ACP agent for the tests of the runner.
//!
//! `FAKE_AGENT_SCRIPT` names a file with one operation per line. Each prompt runs the operations
//! up to the next `turn` and answers `end_turn`; the next prompt goes on from there. Without
//! operations left a prompt ends at once.
//!
//! - `say <text>`: an agent message.
//! - `sh <command>`: runs the command with `sh -c` in the directory of the session.
//! - `wf <args>`: runs `$WF_BIN` with the arguments; double quotes group words, and `{item}` is
//!   the item named in the prompt, as in `You are working on item #k3f9ab`.
//! - `permission`: asks for a permission and says what the answer was.
//! - `hang`: does nothing until the turn is cancelled, then answers `cancelled`.
//! - `crash`: exits with an error.
//! - `turn`: ends the turn.

use std::io::{BufRead, Write};
use std::process::Command;

use serde_json::{Value, json};

fn send(message: Value) {
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{message}").unwrap();
    stdout.flush().unwrap();
}

fn say(text: &str) {
    send(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {"sessionId": "fake", "update": {
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": text},
        }},
    }));
}

/// Splits on whitespace, keeping what is inside double quotes together.
fn words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut started = false;
    for character in line.chars() {
        match character {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            character if character.is_whitespace() && !quoted => {
                if started {
                    words.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            character => {
                current.push(character);
                started = true;
            }
        }
    }
    if started {
        words.push(current);
    }
    words
}

fn main() {
    let script = std::env::var("FAKE_AGENT_SCRIPT")
        .ok()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default();
    let mut operations = script
        .lines()
        .map(str::to_string)
        .collect::<Vec<_>>()
        .into_iter();
    let mut lines = std::io::stdin().lock().lines();
    let mut directory = std::env::current_dir().unwrap();
    let mut item = String::new();
    while let Some(Ok(line)) = lines.next() {
        let request: Value = serde_json::from_str(&line).unwrap();
        let id = request["id"].clone();
        match request["method"].as_str() {
            Some("initialize") => send(json!({
                "jsonrpc": "2.0", "id": id,
                "result": {"protocolVersion": 1, "agentCapabilities": {}},
            })),
            Some("session/new") => {
                if let Some(cwd) = request["params"]["cwd"].as_str() {
                    directory = cwd.into();
                }
                send(json!({"jsonrpc": "2.0", "id": id, "result": {"sessionId": "fake"}}));
            }
            Some("session/prompt") => {
                let prompt = request["params"]["prompt"].to_string();
                if let Some((_, rest)) = prompt.split_once("item #") {
                    item = rest
                        .chars()
                        .take_while(char::is_ascii_alphanumeric)
                        .collect();
                }
                let mut stop = "end_turn";
                for operation in operations.by_ref() {
                    let (name, rest) = operation.split_once(' ').unwrap_or((&operation, ""));
                    match name {
                        "say" => say(rest),
                        "sh" => {
                            let status = Command::new("sh")
                                .args(["-c", rest])
                                .current_dir(&directory)
                                .status()
                                .unwrap();
                            if !status.success() {
                                say(&format!("sh failed: {rest}"));
                            }
                        }
                        "wf" => {
                            let output = Command::new(std::env::var("WF_BIN").unwrap())
                                .args(words(&rest.replace("{item}", &item)))
                                .current_dir(&directory)
                                .output()
                                .unwrap();
                            say(&format!(
                                "wf {rest}: {} {}",
                                String::from_utf8_lossy(&output.stdout).trim(),
                                String::from_utf8_lossy(&output.stderr).trim()
                            ));
                        }
                        "permission" => {
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
                            let reply: Value =
                                serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
                            let chosen = reply["result"]["outcome"]["optionId"]
                                .as_str()
                                .unwrap_or("cancelled");
                            say(&format!("permission: {chosen}"));
                        }
                        "hang" => {
                            while let Some(Ok(line)) = lines.next() {
                                let message: Value = serde_json::from_str(&line).unwrap();
                                if message["method"] == "session/cancel" {
                                    break;
                                }
                            }
                            stop = "cancelled";
                            break;
                        }
                        "crash" => std::process::exit(1),
                        "turn" => break,
                        other => panic!("unknown operation {other}"),
                    }
                }
                send(json!({"jsonrpc": "2.0", "id": id, "result": {"stopReason": stop}}));
            }
            _ => {}
        }
    }
}

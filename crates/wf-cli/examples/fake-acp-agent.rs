//! A scripted ACP agent for the tests of the runner.
//!
//! `FAKE_AGENT_SCRIPT` names a file with one operation per line. Each prompt runs the operations
//! up to the next `turn` and answers `end_turn`; the next prompt goes on from there. Without
//! operations left a prompt ends at once.
//!
//! - `say <text>`: an agent message.
//! - `sh <command>`: runs the command with `sh -c` in the directory of the session.
//! - `wf <args>`: runs `$WF_BIN` with the arguments; double quotes group words.
//!
//! In `sh` and `wf` lines, `{item}` is the item named in the prompt, as in `item #k3f9ab`.
//! - `permission`: asks for a permission and says what the answer was. A cancel while it waits
//!   ends the turn once the request is answered, and so does a request answered `cancelled`.
//! - `permission <command>`: asks to run the command, with `{item}` replaced.
//! - `hang`: does nothing until the turn is cancelled, then answers `cancelled`.
//! - `crash`: exits with an error.
//! - `login`: answers the prompt `Authentication required`, as a harness whose login lapsed.
//! - `turn`: ends the turn.
//! - `unstartable`, as the first line: exits before it answers `initialize`, as a harness that is
//!   not installed.
//!
//! New sessions offer two models, `fast` and `deep`; `fast` is the default.
//!
//! Each session gets an id of its own and keeps how far it got in the script in
//! `<script>.<session>`. `FAKE_AGENT_REOPENS` set to `resume` or `load` offers that way back into a
//! session, which goes on in the script from there.

use std::io::{BufRead, Write};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};

fn send(message: Value) {
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{message}").unwrap();
    stdout.flush().unwrap();
}

/// An agent message of its own, in one chunk.
fn say(text: &str) {
    static MESSAGES: AtomicUsize = AtomicUsize::new(0);
    let message = MESSAGES.fetch_add(1, Ordering::Relaxed);
    send(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {"sessionId": "fake", "update": {
            "sessionUpdate": "agent_message_chunk",
            "messageId": format!("message-{message}"),
            "content": {"type": "text", "text": text},
        }},
    }));
}

/// The options of a new session: its model, set to `model`.
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

/// Where a session keeps how many operations of the script it has run.
fn progress_file(script: &str, session: &str) -> String {
    format!("{script}.{session}")
}

fn main() {
    let script_path = std::env::var("FAKE_AGENT_SCRIPT").unwrap_or_default();
    let script = std::fs::read_to_string(&script_path).unwrap_or_default();
    let reopens = std::env::var("FAKE_AGENT_REOPENS").unwrap_or_default();
    let all: Vec<String> = script.lines().map(str::to_string).collect();
    if all
        .first()
        .is_some_and(|operation| operation == "unstartable")
    {
        std::process::exit(127);
    }
    let mut operations = all.clone().into_iter();
    let mut session = String::new();
    let mut done = 0;
    let mut lines = std::io::stdin().lock().lines();
    let mut directory = std::env::current_dir().unwrap();
    let mut item = String::new();
    while let Some(Ok(line)) = lines.next() {
        let request: Value = serde_json::from_str(&line).unwrap();
        let id = request["id"].clone();
        match request["method"].as_str() {
            Some("initialize") => send(json!({
                "jsonrpc": "2.0", "id": id,
                "result": {"protocolVersion": 1, "agentCapabilities": {
                    "loadSession": reopens == "load",
                    "sessionCapabilities":
                        if reopens == "resume" { json!({"resume": {}}) } else { json!({}) },
                }},
            })),
            Some("session/new") => {
                if let Some(cwd) = request["params"]["cwd"].as_str() {
                    directory = cwd.into();
                }
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos();
                session = format!("fake-{}-{nanos}", std::process::id());
                send(json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {"sessionId": session, "configOptions": config_options("fast")},
                }));
            }
            Some("session/set_config_option") => {
                let model = request["params"]["value"].as_str().unwrap();
                send(json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {"configOptions": config_options(model)},
                }));
            }
            Some(method @ ("session/resume" | "session/load")) => {
                if let Some(cwd) = request["params"]["cwd"].as_str() {
                    directory = cwd.into();
                }
                session = request["params"]["sessionId"].as_str().unwrap().to_string();
                let Ok(progress) = std::fs::read_to_string(progress_file(&script_path, &session))
                else {
                    send(json!({
                        "jsonrpc": "2.0", "id": id,
                        "error": {"code": -32002, "message": "no such session"},
                    }));
                    continue;
                };
                done = progress.trim().parse().unwrap();
                operations = all.clone().into_iter();
                operations.by_ref().take(done).for_each(drop);
                if method == "session/load" {
                    say("what was said before");
                }
                send(json!({"jsonrpc": "2.0", "id": id, "result": {}}));
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
                let mut logged_out = false;
                for operation in operations.by_ref() {
                    done += 1;
                    std::fs::write(progress_file(&script_path, &session), done.to_string())
                        .unwrap();
                    let (name, rest) = operation.split_once(' ').unwrap_or((&operation, ""));
                    match name {
                        "say" => say(rest),
                        "sh" => {
                            let status = Command::new("sh")
                                .args(["-c", &rest.replace("{item}", &item)])
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
                            let tool_call = if rest.is_empty() {
                                json!({"toolCallId": "call-1", "title": "Edit file"})
                            } else {
                                let command = rest.replace("{item}", &item);
                                json!({
                                    "toolCallId": "call-1", "title": command, "kind": "execute",
                                    "rawInput": {"command": command},
                                })
                            };
                            send(json!({
                                "jsonrpc": "2.0", "id": 100, "method": "session/request_permission",
                                "params": {
                                    "sessionId": "fake",
                                    "toolCall": tool_call,
                                    "options": [
                                        {"optionId": "yes", "name": "Yes", "kind": "allow_once"},
                                        {"optionId": "no", "name": "No", "kind": "reject_once"},
                                    ],
                                },
                            }));
                            // As ACP asks, a cancel does not end the wait: the client answers
                            // the request, `cancelled`, and the turn then ends cancelled.
                            let mut cancelled = false;
                            let reply = loop {
                                let message: Value =
                                    serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
                                if message["method"] == "session/cancel" {
                                    cancelled = true;
                                } else if message["id"] == 100 {
                                    break message;
                                }
                            };
                            let chosen = match &reply["result"]["outcome"] {
                                outcome if outcome["outcome"] == "cancelled" => "cancelled",
                                outcome => outcome["optionId"].as_str().unwrap_or("cancelled"),
                            };
                            say(&format!("permission: {chosen}"));
                            if cancelled || chosen == "cancelled" {
                                stop = "cancelled";
                                break;
                            }
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
                        "login" => {
                            logged_out = true;
                            break;
                        }
                        "turn" => break,
                        other => panic!("unknown operation {other}"),
                    }
                }
                if logged_out {
                    send(json!({
                        "jsonrpc": "2.0", "id": id,
                        "error": {"code": -32000, "message": "Authentication required"},
                    }));
                } else {
                    send(json!({"jsonrpc": "2.0", "id": id, "result": {"stopReason": stop}}));
                }
            }
            _ => {}
        }
    }
}

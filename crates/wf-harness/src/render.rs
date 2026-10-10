use std::collections::HashMap;

use serde_json::Value;

use crate::transcript::{Direction, TranscriptEntry};

/// Turns transcript entries into text for people. Message and thought chunks are joined into
/// running text; everything else gets a line of its own.
#[derive(Debug, Default)]
pub struct Renderer {
    mid_line: bool,
    tool_titles: HashMap<String, String>,
}

impl Renderer {
    pub fn render(&mut self, entry: &TranscriptEntry) -> String {
        match entry.direction {
            Direction::AgentLog => self.line(format!("log   {}", text_of(&entry.message))),
            Direction::ToAgent => match method(&entry.message) {
                Some("session/prompt") => {
                    let prompt = entry.message["params"]["prompt"][0]["text"]
                        .as_str()
                        .unwrap_or("");
                    self.line(format!("prompt {prompt}"))
                }
                _ => String::new(),
            },
            Direction::FromAgent => self.render_agent_message(&entry.message),
        }
    }

    fn render_agent_message(&mut self, message: &Value) -> String {
        match method(message) {
            Some("session/update") => self.update(&message["params"]["update"]),
            Some("session/request_permission") => {
                let title = message["params"]["toolCall"]["title"]
                    .as_str()
                    .unwrap_or("?");
                self.line(format!("ask   permission for {title}"))
            }
            _ => {
                if let Some(reason) = message["result"]["stopReason"].as_str() {
                    self.line(format!("end   {reason}"))
                } else if let Some(error) = message["error"]["message"].as_str() {
                    self.line(format!("error {error}"))
                } else {
                    String::new()
                }
            }
        }
    }

    fn update(&mut self, update: &Value) -> String {
        let content_text = update["content"]["text"].as_str().unwrap_or("");
        match update["sessionUpdate"].as_str() {
            Some("agent_message_chunk") => self.chunk(content_text),
            Some("agent_thought_chunk") => self.chunk(content_text),
            Some("tool_call") => {
                let title = update["title"].as_str().unwrap_or("?");
                let status = update["status"].as_str().unwrap_or("pending");
                if let Some(id) = update["toolCallId"].as_str() {
                    self.tool_titles.insert(id.to_owned(), title.to_owned());
                }
                self.line(format!("tool  {title} ({status})"))
            }
            Some("tool_call_update") => match update["status"].as_str() {
                Some(status) => {
                    let id = update["toolCallId"].as_str().unwrap_or("?");
                    let title = self.tool_titles.get(id).map_or(id, String::as_str);
                    let line = format!("tool  {title} ({status})");
                    self.line(line)
                }
                None => String::new(),
            },
            Some("plan") => {
                let entries = update["entries"].as_array().map_or(0, Vec::len);
                self.line(format!("plan  {entries} entries"))
            }
            _ => String::new(),
        }
    }

    fn chunk(&mut self, text: &str) -> String {
        self.mid_line = !text.ends_with('\n');
        text.to_owned()
    }

    fn line(&mut self, text: String) -> String {
        let separator = if self.mid_line { "\n" } else { "" };
        self.mid_line = false;
        format!("{separator}{text}\n")
    }
}

fn method(message: &Value) -> Option<&str> {
    message["method"].as_str()
}

fn text_of(value: &Value) -> &str {
    value.as_str().unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::Timestamp;

    fn entry(direction: Direction, message: Value) -> TranscriptEntry {
        TranscriptEntry {
            at: Timestamp::UNIX_EPOCH,
            direction,
            message,
        }
    }

    fn update(update: Value) -> TranscriptEntry {
        entry(
            Direction::FromAgent,
            serde_json::json!({"method": "session/update", "params": {"update": update}}),
        )
    }

    #[test]
    fn chunks_join_and_other_updates_start_a_new_line() {
        let mut renderer = Renderer::default();
        let chunk = |text: &str| {
            update(serde_json::json!({
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": text}
            }))
        };
        let tool = update(serde_json::json!({
            "sessionUpdate": "tool_call", "title": "Read file", "status": "pending"
        }));
        let output: String = [chunk("Hel"), chunk("lo"), tool]
            .iter()
            .map(|entry| renderer.render(entry))
            .collect();
        assert_eq!(output, "Hello\ntool  Read file (pending)\n");
        let finished = update(serde_json::json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "call-1", "status": "completed"
        }));
        let started = update(serde_json::json!({
            "sessionUpdate": "tool_call", "toolCallId": "call-1", "title": "Read file"
        }));
        renderer.render(&started);
        assert_eq!(renderer.render(&finished), "tool  Read file (completed)\n");
    }

    #[test]
    fn prompts_and_stop_reasons_are_shown() {
        let mut renderer = Renderer::default();
        let prompt = entry(
            Direction::ToAgent,
            serde_json::json!({"method": "session/prompt", "params": {"prompt": [{"type": "text", "text": "hi"}]}}),
        );
        let end = entry(
            Direction::FromAgent,
            serde_json::json!({"id": 3, "result": {"stopReason": "end_turn"}}),
        );
        assert_eq!(renderer.render(&prompt), "prompt hi\n");
        assert_eq!(renderer.render(&end), "end   end_turn\n");
        let failure = entry(
            Direction::FromAgent,
            serde_json::json!({"id": 4, "error": {"code": -32000, "message": "no login"}}),
        );
        assert_eq!(renderer.render(&failure), "error no login\n");
    }
}

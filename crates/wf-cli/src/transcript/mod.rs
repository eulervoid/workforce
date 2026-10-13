//! ACP transcripts, read as turns: what wf told the agent, then what the agent did, closed by a
//! footer.
//!
//! [`Transcript`] folds the entries of a transcript file into blocks that no longer change, and an
//! open tail that still does: the paragraph the agent is writing and the tool calls that have not
//! finished. [`Layout`] draws a block or the tail for a width; the append-only [`Printer`] of `wf
//! session watch` and the dashboard both draw with it.

mod layout;
mod printer;

use std::collections::HashSet;

use jiff::{SignedDuration, Timestamp};
use serde_json::Value;
use wf_harness::{Decision, Direction, Permission, TranscriptEntry};

pub use layout::{Detail, Format, Layout, terminal_width};
pub use printer::{Printer, Session};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Thinking {
    Message,
    Thought,
}

/// Who wrote a prompt: the first one is the brief, wf sends the later ones to keep the session
/// going.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prompter {
    Brief,
    Wf,
}

/// Agent text that is not a whole paragraph yet: markdown can span chunks.
#[derive(Clone, Debug)]
pub struct Prose {
    pub kind: Thinking,
    /// The agent's id for the message the text is from, when it gives one.
    message: Option<String>,
    pub text: String,
    /// When the last chunk came.
    pub at: Timestamp,
    /// Counts the paragraphs of the transcript, so a reader that showed part of an open one knows
    /// it again once it is finished.
    pub paragraph: usize,
}

#[derive(Clone, Debug)]
pub enum Asked {
    Not,
    Waiting,
    Answered(Decision),
}

/// How a call ended.
#[derive(Clone, Debug)]
pub enum Outcome {
    /// By the time of its last update.
    Completed(Timestamp),
    Failed(Timestamp),
    /// Its permission request was denied, so it never ran.
    Denied,
    /// The turn or the session ended while it was still open.
    Unfinished,
}

#[derive(Clone, Debug)]
pub struct ToolCall {
    pub id: String,
    /// The tool's own name when the agent gives one, such as `Bash`, otherwise its kind.
    pub name: String,
    pub title: String,
    /// When the call started running: when it was made, or when its permission request was
    /// answered.
    pub since: Timestamp,
    pub content: Vec<Value>,
    pub asked: Asked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Clone, Debug)]
pub struct PlanEntry {
    pub status: PlanStatus,
    pub content: String,
}

/// Why the agent ended its turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    Cancelled,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    /// A reason ACP does not name.
    Other(String),
}

impl StopReason {
    fn parse(reason: &str) -> Self {
        match reason {
            "end_turn" => Self::EndTurn,
            "cancelled" => Self::Cancelled,
            "max_tokens" => Self::MaxTokens,
            "max_turn_requests" => Self::MaxTurnRequests,
            "refusal" => Self::Refusal,
            other => Self::Other(other.to_owned()),
        }
    }
}

/// A part of the transcript that no longer changes.
#[derive(Clone, Debug)]
pub enum Block {
    Prompt {
        by: Prompter,
        at: Timestamp,
        text: String,
    },
    /// The agent's part of a turn begins.
    Answer {
        at: Timestamp,
    },
    /// A paragraph of the agent's text.
    Prose {
        kind: Thinking,
        text: String,
        paragraph: usize,
    },
    Call {
        call: ToolCall,
        outcome: Outcome,
    },
    /// A line the harness wrote on its own, such as on stderr.
    Log(String),
    Plan(Vec<PlanEntry>),
    /// An error reply; when it answers the prompt, it ends the turn, after `elapsed`.
    Error {
        message: String,
        elapsed: Option<SignedDuration>,
    },
    /// The turn ended, after `elapsed`.
    Footer {
        reason: StopReason,
        elapsed: Option<SignedDuration>,
    },
}

enum Turn {
    Closed,
    /// wf prompted the agent with the request `request`, which has not been answered yet.
    Prompted {
        at: Timestamp,
        request: Value,
    },
    /// The agent's part of the turn is open.
    Answering {
        since: Timestamp,
        request: Value,
    },
}

/// What a session is doing at the end of its transcript so far.
#[derive(Clone, Debug)]
pub enum Now<'a> {
    /// The most recent call that is still open.
    Calling(&'a ToolCall),
    /// The last line of what the agent is writing.
    Writing(&'a str),
    /// Waiting for the agent: its turn is open, but nothing is happening in it.
    Thinking,
    /// The turn is over; the agent waits for wf.
    Idle,
}

pub struct Transcript {
    blocks: Vec<Block>,
    prose: Option<Prose>,
    paragraphs: usize,
    turn: Turn,
    prompts: usize,
    /// Open calls, in the order they started.
    calls: Vec<ToolCall>,
    /// Calls of this turn that were denied: what the agent says of them later is left out.
    denied: HashSet<String>,
}

impl Default for Transcript {
    fn default() -> Self {
        Self::new()
    }
}

impl Transcript {
    pub fn new() -> Self {
        Self {
            blocks: Vec::new(),
            prose: None,
            paragraphs: 0,
            turn: Turn::Closed,
            prompts: 0,
            calls: Vec::new(),
            denied: HashSet::new(),
        }
    }

    /// The blocks that no longer change, in order. New ones are only ever appended.
    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    /// The paragraph the agent is writing.
    pub fn open_prose(&self) -> Option<&Prose> {
        self.prose.as_ref()
    }

    /// The calls that have not finished, in the order they started.
    pub fn open_calls(&self) -> &[ToolCall] {
        &self.calls
    }

    pub fn now(&self) -> Now<'_> {
        if let Some(call) = self.calls.last() {
            return Now::Calling(call);
        }
        if let Some(line) = self.prose.as_ref().and_then(|prose| {
            prose
                .text
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
        }) {
            return Now::Writing(line.trim());
        }
        match self.turn {
            Turn::Closed => Now::Idle,
            Turn::Prompted { .. } | Turn::Answering { .. } => Now::Thinking,
        }
    }

    /// Takes in the next entry of the transcript file.
    pub fn push(&mut self, entry: &TranscriptEntry) {
        let message = &entry.message;
        let method = message["method"].as_str();
        if entry.direction == Direction::FromAgent && method == Some("session/update") {
            let update = &message["params"]["update"];
            let thinking = match update["sessionUpdate"].as_str() {
                Some("agent_message_chunk") => Some(Thinking::Message),
                Some("agent_thought_chunk") => Some(Thinking::Thought),
                _ => None,
            };
            if let Some(kind) = thinking {
                let text = update["content"]["text"].as_str().unwrap_or("");
                self.chunk(entry.at, kind, update["messageId"].as_str(), text);
                return;
            }
        }
        match entry.direction {
            Direction::AgentLog => {
                let text = message.as_str().unwrap_or("");
                self.blocks.push(Block::Log(text.to_owned()));
            }
            Direction::ToAgent => {
                if method == Some("session/prompt") {
                    self.prompt(entry.at, message);
                }
            }
            Direction::Wf => {
                if let Ok(decision) = serde_json::from_value(message["decision"].clone()) {
                    let call = message["toolCallId"].as_str().unwrap_or("?");
                    self.decided(entry.at, call, decision);
                }
            }
            Direction::FromAgent => match method {
                Some("session/update") => self.update(entry.at, &message["params"]["update"]),
                Some("session/request_permission") => {
                    let call = self.call(entry.at, &message["params"]["toolCall"]);
                    call.asked = Asked::Waiting;
                    let id = call.id.clone();
                    self.denied.remove(&id);
                }
                Some(_) => {}
                None => {
                    if let Some(reason) = message["result"]["stopReason"].as_str() {
                        let elapsed = self.close_turn(entry.at);
                        self.blocks.push(Block::Footer {
                            reason: StopReason::parse(reason),
                            elapsed,
                        });
                    } else if let Some(error) = message["error"]["message"].as_str() {
                        self.error(entry.at, &message["id"], error);
                    }
                }
            },
        }
    }

    /// The session ended: what is still open will not finish.
    pub fn end(&mut self) {
        self.unfinished_calls();
    }

    /// Ends the paragraph being written.
    fn close_prose(&mut self) {
        let Some(prose) = self.prose.take() else {
            return;
        };
        if prose.text.trim().is_empty() {
            return;
        }
        self.blocks.push(Block::Prose {
            kind: prose.kind,
            text: prose.text,
            paragraph: prose.paragraph,
        });
    }

    fn open_prose_with(&mut self, kind: Thinking, message: Option<String>, at: Timestamp) {
        self.paragraphs += 1;
        self.prose = Some(Prose {
            kind,
            message,
            text: String::new(),
            at,
            paragraph: self.paragraphs,
        });
    }

    /// Opens the agent's part of the turn, unless it is open already.
    fn answering(&mut self, at: Timestamp) {
        let (since, request) = match std::mem::replace(&mut self.turn, Turn::Closed) {
            turn @ Turn::Answering { .. } => {
                self.turn = turn;
                return;
            }
            Turn::Prompted { at, request } => (at, request),
            Turn::Closed => (at, Value::Null),
        };
        self.turn = Turn::Answering { since, request };
        self.blocks.push(Block::Answer { at });
    }

    /// A chunk of the agent's text. A chunk of another message, or of thoughts after a message,
    /// starts a paragraph of its own.
    fn chunk(&mut self, at: Timestamp, kind: Thinking, message: Option<&str>, text: &str) {
        if self
            .prose
            .as_ref()
            .is_some_and(|prose| prose.kind != kind || prose.message.as_deref() != message)
        {
            self.close_prose();
        }
        self.answering(at);
        if self.prose.is_none() {
            self.open_prose_with(kind, message.map(str::to_owned), at);
        }
        let prose = self.prose.as_mut().expect("it was just opened");
        prose.text.push_str(text);
        prose.at = at;
        while let Some(boundary) = self
            .prose
            .as_ref()
            .and_then(|prose| layout::paragraph_boundary(&prose.text))
        {
            let prose = self.prose.as_mut().expect("a boundary was found in it");
            let tail = prose.text.split_off(boundary);
            let message = prose.message.clone();
            self.close_prose();
            self.open_prose_with(kind, message, at);
            self.prose.as_mut().expect("it was just opened").text = tail;
        }
    }

    /// wf decided the permission request for the call `call_id`, right before answering it. A
    /// call that was denied is done with.
    fn decided(&mut self, at: Timestamp, call_id: &str, decision: Decision) {
        let Some(index) = self.calls.iter().position(|call| call.id == call_id) else {
            return;
        };
        let call = &mut self.calls[index];
        call.since = at;
        let permission = decision.permission;
        call.asked = Asked::Answered(decision);
        match permission {
            Permission::Allow => return,
            Permission::Deny => {}
        }
        let call = self.calls.remove(index);
        self.denied.insert(call.id.clone());
        self.close_prose();
        self.answering(at);
        self.blocks.push(Block::Call {
            call,
            outcome: Outcome::Denied,
        });
    }

    fn prompt(&mut self, at: Timestamp, message: &Value) {
        let text = message["params"]["prompt"]
            .as_array()
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|block| block["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        let by = if self.prompts == 0 {
            Prompter::Brief
        } else {
            Prompter::Wf
        };
        self.prompts += 1;
        self.close_prose();
        self.turn = Turn::Prompted {
            at,
            request: message["id"].clone(),
        };
        self.blocks.push(Block::Prompt { by, at, text });
    }

    /// The calls left open, which end with the turn or the session.
    fn unfinished_calls(&mut self) {
        self.close_prose();
        self.denied.clear();
        for call in std::mem::take(&mut self.calls) {
            self.answering(call.since);
            self.blocks.push(Block::Call {
                call,
                outcome: Outcome::Unfinished,
            });
        }
    }

    /// Closes the turn: the calls it left open, then how long it took.
    fn close_turn(&mut self, at: Timestamp) -> Option<SignedDuration> {
        self.unfinished_calls();
        match std::mem::replace(&mut self.turn, Turn::Closed) {
            Turn::Prompted { at: since, .. } | Turn::Answering { since, .. } => {
                Some(at.duration_since(since))
            }
            Turn::Closed => None,
        }
    }

    /// An error reply: to the prompt of the turn, it ends the turn; to anything else, it is shown
    /// where it comes.
    fn error(&mut self, at: Timestamp, request: &Value, error: &str) {
        let ends_turn = match &self.turn {
            Turn::Prompted {
                request: prompt, ..
            }
            | Turn::Answering {
                request: prompt, ..
            } => prompt == request,
            Turn::Closed => false,
        };
        let elapsed = if ends_turn {
            self.close_turn(at)
        } else {
            self.close_prose();
            self.answering(at);
            None
        };
        self.blocks.push(Block::Error {
            message: error.to_owned(),
            elapsed,
        });
    }

    fn update(&mut self, at: Timestamp, update: &Value) {
        match update["sessionUpdate"].as_str() {
            Some("tool_call" | "tool_call_update") => self.tool(at, update),
            Some("plan") => {
                self.close_prose();
                self.answering(at);
                let entries = update["entries"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|entry| PlanEntry {
                        status: match entry["status"].as_str() {
                            Some("completed") => PlanStatus::Completed,
                            Some("in_progress") => PlanStatus::InProgress,
                            _ => PlanStatus::Pending,
                        },
                        content: entry["content"].as_str().unwrap_or("?").to_owned(),
                    })
                    .collect();
                self.blocks.push(Block::Plan(entries));
            }
            _ => {}
        }
    }

    /// The open call `fields` is about, taking in what they say of it.
    fn call(&mut self, at: Timestamp, fields: &Value) -> &mut ToolCall {
        let id = fields["toolCallId"].as_str().unwrap_or("?");
        let index = match self.calls.iter().position(|call| call.id == id) {
            Some(index) => index,
            None => {
                self.calls.push(ToolCall {
                    id: id.to_owned(),
                    name: String::new(),
                    title: String::new(),
                    since: at,
                    content: Vec::new(),
                    asked: Asked::Not,
                });
                self.calls.len() - 1
            }
        };
        let call = &mut self.calls[index];
        if let Some(name) = fields["name"].as_str() {
            call.name = name.to_owned();
        } else if call.name.is_empty()
            && let Some(kind) = fields["kind"].as_str()
        {
            call.name = layout::kind_name(kind).to_owned();
        }
        if let Some(title) = fields["title"].as_str() {
            call.title = title.to_owned();
        }
        if let Some(content) = fields["content"].as_array()
            && !content.is_empty()
        {
            call.content = content.clone();
        }
        call
    }

    fn tool(&mut self, at: Timestamp, update: &Value) {
        if update["toolCallId"]
            .as_str()
            .is_some_and(|id| self.denied.contains(id))
        {
            return;
        }
        self.close_prose();
        self.answering(at);
        let outcome = match update["status"].as_str() {
            Some("completed") => Some(Outcome::Completed(at)),
            Some("failed") => Some(Outcome::Failed(at)),
            _ => None,
        };
        let id = self.call(at, update).id.clone();
        let Some(outcome) = outcome else {
            return;
        };
        let index = self
            .calls
            .iter()
            .position(|call| call.id == id)
            .expect("the call was just taken in");
        let call = self.calls.remove(index);
        self.blocks.push(Block::Call { call, outcome });
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn entry(seconds: i64, direction: Direction, message: Value) -> TranscriptEntry {
        TranscriptEntry {
            at: Timestamp::from_second(seconds).unwrap(),
            direction,
            message,
        }
    }

    fn update(seconds: i64, update: Value) -> TranscriptEntry {
        entry(
            seconds,
            Direction::FromAgent,
            json!({"method": "session/update", "params": {"update": update}}),
        )
    }

    #[test]
    fn now_is_the_latest_open_call_then_the_text_being_written() {
        let mut transcript = Transcript::new();
        assert!(matches!(transcript.now(), Now::Idle));
        transcript.push(&entry(
            0,
            Direction::ToAgent,
            json!({"id": 1, "method": "session/prompt", "params": {"prompt": [{"type": "text", "text": "Do it"}]}}),
        ));
        assert!(matches!(transcript.now(), Now::Thinking));
        transcript.push(&update(
            1,
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Reading.\n\nNow the tests"}}),
        ));
        assert!(matches!(transcript.now(), Now::Writing("Now the tests")));
        transcript.push(&update(
            2,
            json!({"sessionUpdate": "tool_call", "toolCallId": "1", "name": "Bash", "title": "cargo test"}),
        ));
        assert!(matches!(transcript.now(), Now::Calling(call) if call.title == "cargo test"));
        transcript.push(&update(
            3,
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "1", "status": "completed"}),
        ));
        assert!(matches!(transcript.now(), Now::Thinking));
        transcript.push(&entry(
            4,
            Direction::FromAgent,
            json!({"id": 1, "result": {"stopReason": "end_turn"}}),
        ));
        assert!(matches!(transcript.now(), Now::Idle));
    }

    #[test]
    fn blocks_are_only_appended() {
        let mut transcript = Transcript::new();
        transcript.push(&update(
            0,
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "One.\n\nTwo"}}),
        ));
        assert!(matches!(
            transcript.blocks(),
            [Block::Answer { .. }, Block::Prose { text, .. }] if text == "One.\n\n"
        ));
        assert_eq!(transcript.open_prose().unwrap().text, "Two");
        transcript.end();
        assert!(matches!(
            transcript.blocks(),
            [_, _, Block::Prose { text, .. }] if text == "Two"
        ));
    }
}

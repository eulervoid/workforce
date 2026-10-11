use std::io::{self, Write};
use std::sync::Mutex;

use agent_client_protocol::LineDirection;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Which side of the connection a transcript line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    ToAgent,
    FromAgent,
    AgentLog,
    /// Written by wf itself, not crossing the connection: who answered a permission request.
    Wf,
}

impl From<LineDirection> for Direction {
    fn from(direction: LineDirection) -> Self {
        match direction {
            LineDirection::Stdin => Direction::ToAgent,
            LineDirection::Stdout => Direction::FromAgent,
            LineDirection::Stderr => Direction::AgentLog,
        }
    }
}

/// One line of a transcript file. `message` is the JSON-RPC message as sent or received, or a
/// JSON string for lines that are not JSON, such as the agent's stderr.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptEntry {
    pub at: Timestamp,
    pub direction: Direction,
    pub message: Value,
}

impl TranscriptEntry {
    /// `{"decision": {"permission": "allow", "by": "rules", "reason": ...}}`.
    pub fn decision(at: Timestamp, decision: &crate::permission::Decision) -> Self {
        TranscriptEntry {
            at,
            direction: Direction::Wf,
            message: serde_json::json!({ "decision": decision }),
        }
    }

    pub fn from_line(at: Timestamp, direction: Direction, line: &str) -> Self {
        let message = match direction {
            Direction::AgentLog | Direction::Wf => Value::String(line.to_owned()),
            Direction::ToAgent | Direction::FromAgent => {
                serde_json::from_str(line).unwrap_or_else(|_| Value::String(line.to_owned()))
            }
        };
        TranscriptEntry {
            at,
            direction,
            message,
        }
    }
}

/// Appends entries to a writer, one JSON object per line, flushed so that a reader can follow.
/// The first write failure is kept and the entries after it are dropped.
pub(crate) struct TranscriptWriter<W> {
    state: Mutex<WriterState<W>>,
}

struct WriterState<W> {
    writer: W,
    failure: Option<io::Error>,
}

impl<W: Write> TranscriptWriter<W> {
    pub(crate) fn new(writer: W) -> Self {
        TranscriptWriter {
            state: Mutex::new(WriterState {
                writer,
                failure: None,
            }),
        }
    }

    pub(crate) fn record(&self, entry: &TranscriptEntry) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.failure.is_some() {
            return;
        }
        let result = serde_json::to_vec(entry)
            .map_err(io::Error::from)
            .and_then(|mut line| {
                line.push(b'\n');
                state.writer.write_all(&line)
            })
            .and_then(|()| state.writer.flush());
        if let Err(error) = result {
            state.failure = Some(error);
        }
    }

    pub(crate) fn finish(&self) -> io::Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        match state.failure.take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_lines_are_kept_as_json_and_other_lines_as_strings() {
        let at = Timestamp::UNIX_EPOCH;
        let request = TranscriptEntry::from_line(at, Direction::ToAgent, r#"{"id":1}"#);
        assert_eq!(request.message["id"], 1);
        let noise = TranscriptEntry::from_line(at, Direction::FromAgent, "starting up");
        assert_eq!(noise.message, Value::String("starting up".into()));
        let log = TranscriptEntry::from_line(at, Direction::AgentLog, r#"{"id":1}"#);
        assert_eq!(log.message, Value::String(r#"{"id":1}"#.into()));
    }

    #[test]
    fn entries_round_trip_through_lines() {
        let buffer = Vec::new();
        let writer = TranscriptWriter::new(buffer);
        let entry = TranscriptEntry::from_line(Timestamp::UNIX_EPOCH, Direction::ToAgent, "{}");
        writer.record(&entry);
        writer.finish().unwrap();
        let bytes = writer.state.into_inner().unwrap().writer;
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.ends_with('\n'));
        assert_eq!(
            serde_json::from_str::<TranscriptEntry>(text.trim()).unwrap(),
            entry
        );
    }
}

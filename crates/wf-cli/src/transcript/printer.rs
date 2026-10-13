//! Append-only terminal presentation of a transcript for `wf session watch`: no cursor movement,
//! so it works on a pipe as well.
//!
//! Each tool call is one block, printed once it is finished, that holds the call, the answer to
//! its permission request and its result. A call still open when the reader catches up shows as a
//! running line first.

use std::collections::HashMap;

use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};
use wf_harness::TranscriptEntry;

use super::layout::Spacing;
use super::{Asked, Block, Detail, Format, Layout, Transcript};

/// How long agent text may wait for the rest of its paragraph before it is printed anyway.
const STALL: SignedDuration = SignedDuration::from_secs(1);

/// Whether the session of the transcript still runs, when the reader reaches its end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Session {
    Live,
    Ended,
}

/// The running line of an open call that went out last, so each state is shown once.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shown {
    WaitingForPermission,
    Running,
}

pub struct Printer {
    transcript: Transcript,
    layout: Layout,
    /// How many blocks of the transcript are printed.
    printed: usize,
    last: Spacing,
    /// The open paragraph that was printed in part because it waited too long, and how many bytes
    /// of it.
    printed_prose: Option<(usize, usize)>,
    shown: HashMap<String, Shown>,
}

impl Printer {
    pub fn new(
        agent: &str,
        width: usize,
        format: Format,
        detail: Detail,
        time_zone: TimeZone,
    ) -> Self {
        Self {
            transcript: Transcript::new(),
            layout: Layout::new(agent, width, format, detail, time_zone),
            printed: 0,
            last: Spacing::Nothing,
            printed_prose: None,
            shown: HashMap::new(),
        }
    }

    /// Takes in the next entry, and prints what it finished.
    pub fn render(&mut self, entry: &TranscriptEntry) -> String {
        self.transcript.push(entry);
        self.new_blocks()
    }

    /// What there is to say once everything written so far is rendered, at `now`: agent text
    /// that waited too long for the rest of its paragraph, and the calls that are still open.
    /// Call it whenever the transcript has no more lines for now.
    pub fn caught_up(&mut self, now: Timestamp) -> String {
        let mut out = String::new();
        if self
            .transcript
            .open_prose()
            .is_some_and(|prose| now.duration_since(prose.at) >= STALL)
        {
            out.push_str(&self.open_prose());
        }
        out.push_str(&self.open_calls());
        out
    }

    /// Everything still held back, at the end of the transcript. The calls still open are running
    /// while the session is, and unfinished once it ended.
    pub fn finish(&mut self, session: Session) -> String {
        match session {
            Session::Live => {
                let mut out = self.open_prose();
                out.push_str(&self.open_calls());
                out
            }
            Session::Ended => {
                self.transcript.end();
                self.new_blocks()
            }
        }
    }

    fn new_blocks(&mut self) -> String {
        let mut out = String::new();
        for block in &self.transcript.blocks()[self.printed..] {
            let text = match block {
                Block::Prose {
                    kind,
                    text,
                    paragraph,
                } => match self.printed_prose.take() {
                    Some((printed, bytes)) if printed == *paragraph => {
                        self.layout.prose(*kind, &text[bytes..])
                    }
                    other => {
                        self.printed_prose = other;
                        self.layout.block(block)
                    }
                },
                Block::Call { call, .. } => {
                    self.shown.remove(&call.id);
                    self.layout.block(block)
                }
                Block::Prompt { .. }
                | Block::Answer { .. }
                | Block::Log(_)
                | Block::Plan(_)
                | Block::Error { .. }
                | Block::Footer { .. } => self.layout.block(block),
            };
            if text.is_empty() {
                continue;
            }
            out.push_str(self.last.before(block.opens()));
            out.push_str(&text);
            self.last = block.leaves();
        }
        self.printed = self.transcript.blocks().len();
        out
    }

    /// The part of the open paragraph not printed yet, whole or not.
    fn open_prose(&mut self) -> String {
        let Some(prose) = self.transcript.open_prose() else {
            return String::new();
        };
        let from = match self.printed_prose {
            Some((paragraph, bytes)) if paragraph == prose.paragraph => bytes,
            Some(_) | None => 0,
        };
        self.printed_prose = Some((prose.paragraph, prose.text.len()));
        let text = self.layout.prose(prose.kind, &prose.text[from..]);
        if text.is_empty() {
            return text;
        }
        let mut out = self.last.before(Spacing::Prose).to_owned();
        out.push_str(&text);
        self.last = Spacing::Prose;
        out
    }

    /// A running line for each open call whose state has not been shown yet.
    fn open_calls(&mut self) -> String {
        let mut out = String::new();
        for call in self.transcript.open_calls() {
            let state = match call.asked {
                Asked::Waiting => Shown::WaitingForPermission,
                Asked::Not | Asked::Answered(_) => Shown::Running,
            };
            if self.shown.insert(call.id.clone(), state) == Some(state) {
                continue;
            }
            out.push_str(self.last.before(Spacing::Tool));
            out.push_str(&self.layout.running(call));
            self.last = Spacing::Tool;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use jiff::tz::TimeZone;
    use serde_json::Value;
    use wf_harness::{Decider, Decision, Direction, Permission, TranscriptEntry};

    use super::*;

    fn width_of(text: &str) -> usize {
        textwrap::core::display_width(text)
    }

    fn plain(text: &str) -> String {
        anstream::adapter::strip_str(text).to_string()
    }

    fn renderer(format: Format, detail: Detail, width: usize) -> Printer {
        Printer::new("coder", width, format, detail, TimeZone::UTC)
    }

    fn at(seconds: i64) -> Timestamp {
        Timestamp::from_second(seconds).unwrap()
    }

    fn entry(seconds: i64, direction: Direction, message: Value) -> TranscriptEntry {
        TranscriptEntry {
            at: at(seconds),
            direction,
            message,
        }
    }

    fn update(seconds: i64, update: Value) -> TranscriptEntry {
        entry(
            seconds,
            Direction::FromAgent,
            serde_json::json!({"method": "session/update", "params": {"update": update}}),
        )
    }

    fn render_all(renderer: &mut Printer, entries: &[TranscriptEntry]) -> String {
        let mut out: String = entries
            .iter()
            .map(|entry| plain(&renderer.render(entry)))
            .collect();
        out.push_str(&plain(&renderer.finish(Session::Live)));
        out
    }

    #[test]
    fn chunk_boundaries_do_not_break_markdown_or_fences() {
        let mut renderer = renderer(Format::Styled, Detail::Abridged, 80);
        let output: String = [
            "**Hel",
            "lo**\n\n```",
            "rust\n\nlet value = 1;\n",
            "```\n\n",
        ]
        .into_iter()
        .map(|text| renderer.render(&said(0, "a", text)))
        .collect();
        assert!(output.contains("Hello"));
        assert!(!output.contains("**"));
        assert!(output.contains("let value = 1;"));
        assert!(
            renderer
                .transcript
                .open_prose()
                .is_none_or(|prose| prose.text.is_empty())
        );
    }

    #[test]
    fn a_call_its_permission_and_its_result_are_one_block() {
        let long = "x".repeat(100);
        let entries = [
            update(
                0,
                serde_json::json!({"sessionUpdate": "tool_call", "toolCallId": "1", "name": "Bash", "title": "Terminal", "kind": "execute", "status": "pending"}),
            ),
            update(
                1,
                serde_json::json!({"sessionUpdate": "tool_call_update", "toolCallId": "1", "title": format!("echo {long}")}),
            ),
            entry(
                1,
                Direction::FromAgent,
                serde_json::json!({"id": 7, "method": "session/request_permission", "params": {"toolCall": {"toolCallId": "1"}}}),
            ),
            decision(2, "1", Permission::Allow, Decider::Principal, None),
            entry(
                2,
                Direction::ToAgent,
                serde_json::json!({"id": 7, "result": {"outcome": {"outcome": "selected", "optionId": "allow-once"}}}),
            ),
            update(
                4,
                serde_json::json!({"sessionUpdate": "tool_call_update", "toolCallId": "1", "status": "completed",
                "content": [{"type": "content", "content": {"type": "text", "text": format!("```console\n{long}\nsecond\n```")}}]}),
            ),
        ];
        let output = render_all(
            &mut renderer(Format::Styled, Detail::Abridged, 60),
            &entries,
        );
        assert_eq!(output.matches("Bash").count(), 1, "{output}");
        assert!(output.lines().all(|line| width_of(line) <= 60), "{output}");
        let header = output.lines().find(|line| line.contains("Bash")).unwrap();
        assert!(header.starts_with("  ✓ Bash  echo xxx"), "{header}");
        assert!(
            header.contains("allowed by you") && header.ends_with("2.0s"),
            "{header}"
        );
        assert!(
            output.contains("      second") && !output.contains("```"),
            "{output}"
        );
    }

    #[test]
    fn an_open_call_shows_as_running_once_caught_up() {
        let mut renderer = renderer(Format::Raw, Detail::Abridged, 60);
        let start = update(
            0,
            serde_json::json!({"sessionUpdate": "tool_call", "toolCallId": "1", "name": "Bash", "title": "sleep 9"}),
        );
        assert_eq!(renderer.render(&start), "\ncoder  00:00\n".trim_start());
        assert!(renderer.caught_up(at(0)).starts_with("  … Bash  sleep 9"));
        assert_eq!(renderer.caught_up(at(0)), "");
        let ask = entry(
            1,
            Direction::FromAgent,
            serde_json::json!({"id": 1, "method": "session/request_permission", "params": {"toolCall": {"toolCallId": "1"}}}),
        );
        renderer.render(&ask);
        assert!(
            renderer
                .caught_up(at(1))
                .trim_end()
                .ends_with("waiting for permission")
        );
    }

    #[test]
    fn unweighed_allows_and_harness_logs_show_only_with_full() {
        let entries = |by: Decider| {
            [
                entry(
                    0,
                    Direction::AgentLog,
                    serde_json::json!("[session/create] x"),
                ),
                update(
                    0,
                    serde_json::json!({"sessionUpdate": "tool_call", "toolCallId": "1", "kind": "read", "title": "Read a.rs"}),
                ),
                entry(
                    0,
                    Direction::FromAgent,
                    serde_json::json!({"id": 1, "method": "session/request_permission", "params": {"toolCall": {"toolCallId": "1"}}}),
                ),
                decision(0, "1", Permission::Allow, by, None),
                entry(
                    0,
                    Direction::ToAgent,
                    serde_json::json!({"id": 1, "result": {}}),
                ),
                update(
                    0,
                    serde_json::json!({"sessionUpdate": "tool_call_update", "toolCallId": "1", "status": "completed"}),
                ),
            ]
        };
        let quiet = render_all(
            &mut renderer(Format::Raw, Detail::Abridged, 80),
            &entries(Decider::FixedPermissions),
        );
        assert!(
            !quiet.contains("allowed") && !quiet.contains("session/create"),
            "{quiet}"
        );
        assert!(quiet.contains("✓ Read  a.rs"), "{quiet}");
        let full = render_all(
            &mut renderer(Format::Raw, Detail::Full, 80),
            &entries(Decider::FixedPermissions),
        );
        assert!(
            full.contains("allowed by the agent's permissions")
                && full.contains("log [session/create] x"),
            "{full}"
        );
        let rules = render_all(
            &mut renderer(Format::Raw, Detail::Abridged, 80),
            &entries(Decider::Rules),
        );
        assert!(rules.contains("allowed by rules"), "{rules}");
    }

    #[test]
    fn long_output_keeps_its_head_and_tail() {
        let text: String = (0..30).map(|index| format!("line {index}\n")).collect();
        let entries = [update(
            0,
            serde_json::json!({"sessionUpdate": "tool_call_update", "toolCallId": "1", "status": "failed",
            "content": [{"type": "content", "content": {"type": "text", "text": text}}]}),
        )];
        let abridged = render_all(&mut renderer(Format::Raw, Detail::Abridged, 80), &entries);
        assert!(
            abridged.contains("| line 4\n    | … 20 lines hidden\n    | line 25\n"),
            "{abridged}"
        );
        assert!(
            abridged.contains("✗") && abridged.contains("failed  0.0s"),
            "{abridged}"
        );
        let full = render_all(&mut renderer(Format::Raw, Detail::Full, 80), &entries);
        assert!(
            full.contains("line 12") && !full.contains("hidden"),
            "{full}"
        );
    }

    #[test]
    fn raw_keeps_markdown_and_has_no_escape_codes() {
        let mut renderer = renderer(Format::Raw, Detail::Abridged, 80);
        let output = renderer.render(&said(0, "a", "**kept**"));
        let output = output + &renderer.finish(Session::Live);
        assert!(output.contains("  **kept**") && !output.contains('\x1b'));
    }

    #[test]
    fn edits_are_titled_by_their_path_with_a_count_of_changes() {
        let entries = [update(
            0,
            serde_json::json!({
                "sessionUpdate": "tool_call_update", "toolCallId": "edit-1", "kind": "edit", "status": "completed",
                "content": [{"type": "diff", "path": "src/main.rs", "oldText": "before\n", "newText": "after\n"}]
            }),
        )];
        let output = render_all(&mut renderer(Format::Raw, Detail::Abridged, 80), &entries);
        assert!(output.contains("✓ Edit  src/main.rs  +1 -1"), "{output}");
        assert!(
            output.contains("      -before") && output.contains("      +after"),
            "{output}"
        );
        assert!(!output.contains("+++"));
    }

    #[test]
    fn a_turn_ends_with_a_plain_footer_and_its_duration() {
        let entries = [
            entry(
                0,
                Direction::ToAgent,
                serde_json::json!({"id": "p", "method": "session/prompt", "params": {"prompt": [{"type": "text", "text": format!("# Do it\n\n{}", "more\n".repeat(11))}]}}),
            ),
            update(
                5,
                serde_json::json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Done."}}),
            ),
            entry(
                65,
                Direction::FromAgent,
                serde_json::json!({"id": "p", "result": {"stopReason": "end_turn"}}),
            ),
        ];
        let output = render_all(&mut renderer(Format::Raw, Detail::Abridged, 40), &entries);
        assert_eq!(
            output,
            "brief  00:00                    13 lines\n  Do it\n\ncoder  00:00\n  Done.\n\nfinished                           1m05s\n"
        );
    }

    fn prompt(seconds: i64, text: &str) -> TranscriptEntry {
        entry(
            seconds,
            Direction::ToAgent,
            serde_json::json!({"id": seconds, "method": "session/prompt", "params": {"prompt": [{"type": "text", "text": text}]}}),
        )
    }

    fn end_turn(seconds: i64) -> TranscriptEntry {
        entry(
            seconds,
            Direction::FromAgent,
            serde_json::json!({"id": seconds, "result": {"stopReason": "end_turn"}}),
        )
    }

    fn decision(
        seconds: i64,
        call: &str,
        permission: Permission,
        by: Decider,
        reason: Option<&str>,
    ) -> TranscriptEntry {
        let decision = Decision {
            permission,
            by,
            reason: reason.map(str::to_owned),
        };
        TranscriptEntry::decision(at(seconds), call, &decision)
    }

    fn asked(seconds: i64, request: i64, call: &str, title: &str) -> TranscriptEntry {
        entry(
            seconds,
            Direction::FromAgent,
            serde_json::json!({"id": request, "method": "session/request_permission", "params": {"toolCall": {"toolCallId": call, "title": title}}}),
        )
    }

    fn said(seconds: i64, message: &str, text: &str) -> TranscriptEntry {
        update(
            seconds,
            serde_json::json!({"sessionUpdate": "agent_message_chunk", "messageId": message, "content": {"type": "text", "text": text}}),
        )
    }

    #[test]
    fn every_prompt_opens_a_turn_and_short_ones_show_whole() {
        let entries = [
            prompt(0, &format!("# Brief\n\n{}", "line\n".repeat(20))),
            said(1, "a", "Blocked."),
            end_turn(2),
            prompt(
                60,
                "What you were waiting for is resolved.\n- @ada: Use humantime\nContinue.",
            ),
            said(61, "b", "Done."),
            end_turn(62),
        ];
        let output = render_all(&mut renderer(Format::Raw, Detail::Abridged, 60), &entries);
        assert!(
            output.contains("  Brief\n") && !output.contains("line\n"),
            "{output}"
        );
        assert!(
            output.contains("wf  00:01\n  What you were waiting for is resolved.\n  - @ada: Use humantime\n  Continue.\n"),
            "{output}"
        );
        assert_eq!(output.matches("finished").count(), 2, "{output}");
    }

    #[test]
    fn separate_messages_do_not_run_together() {
        let entries = [said(0, "a", "permission: no"), said(0, "b", "done")];
        let output = render_all(&mut renderer(Format::Raw, Detail::Abridged, 60), &entries);
        assert!(output.contains("  permission: no\n\n  done\n"), "{output}");
    }

    #[test]
    fn text_that_waits_for_the_rest_of_its_paragraph_shows_after_a_while() {
        let mut renderer = renderer(Format::Raw, Detail::Abridged, 60);
        renderer.render(&said(0, "a", "watching"));
        assert!(!renderer.caught_up(at(0)).contains("watching"));
        assert!(renderer.caught_up(at(1)).contains("  watching\n"));
        assert_eq!(renderer.finish(Session::Live), "");
    }

    #[test]
    fn a_paragraph_shown_in_part_is_not_shown_again_when_it_ends() {
        let mut renderer = renderer(Format::Raw, Detail::Abridged, 60);
        renderer.render(&said(0, "a", "first half"));
        assert!(renderer.caught_up(at(1)).contains("  first half\n"));
        let rest = renderer.render(&said(2, "a", " and the rest\n\n"));
        assert!(
            rest.contains("and the rest") && !rest.contains("first half"),
            "{rest}"
        );
    }

    #[test]
    fn a_decision_says_by_whom_and_why_and_a_denied_call_is_done() {
        let entries = [
            entry(
                0,
                Direction::FromAgent,
                serde_json::json!({"id": 100, "method": "session/request_permission", "params": {"toolCall": {"toolCallId": "call-1", "title": "Edit file"}}}),
            ),
            decision(
                0,
                "call-1",
                Permission::Deny,
                Decider::Hook {
                    program: "sh".to_owned(),
                },
                Some("no edits today"),
            ),
            entry(
                0,
                Direction::ToAgent,
                serde_json::json!({"id": 100, "result": {}}),
            ),
            update(
                1,
                serde_json::json!({"sessionUpdate": "tool_call_update", "toolCallId": "call-1", "status": "failed"}),
            ),
            said(1, "a", "permission: no"),
            end_turn(2),
        ];
        let output = render_all(&mut renderer(Format::Raw, Detail::Abridged, 60), &entries);
        assert!(
            output.contains("  ✗ Tool  Edit file\n    denied by hook sh: no edits today\n"),
            "{output}"
        );
        assert_eq!(output.matches("Edit file").count(), 1, "{output}");
    }

    #[test]
    fn calls_a_turn_leaves_open_end_with_it() {
        let entries = [
            entry(
                0,
                Direction::FromAgent,
                serde_json::json!({"id": 100, "method": "session/request_permission", "params": {"toolCall": {"toolCallId": "call-1", "title": "Edit file"}}}),
            ),
            decision(
                0,
                "call-1",
                Permission::Allow,
                Decider::Agent {
                    agent: "judge".to_owned(),
                },
                Some("edits serve the item"),
            ),
            entry(
                0,
                Direction::ToAgent,
                serde_json::json!({"id": 100, "result": {}}),
            ),
            end_turn(2),
        ];
        let output = render_all(&mut renderer(Format::Raw, Detail::Abridged, 60), &entries);
        assert!(output.contains("· Tool  Edit file"), "{output}");
        assert!(output.contains("unfinished"), "{output}");
        assert!(
            output.contains("    allowed by agent judge: edits serve the item\n"),
            "{output}"
        );
        assert!(!output.contains("running"), "{output}");
    }

    #[test]
    fn decisions_belong_to_the_call_they_name() {
        let entries = [
            asked(0, 100, "call-a", "Edit a"),
            asked(0, 101, "call-b", "Edit b"),
            decision(1, "call-b", Permission::Allow, Decider::Principal, None),
            decision(
                2,
                "call-a",
                Permission::Deny,
                Decider::Hook {
                    program: "sh".to_owned(),
                },
                Some("no edits today"),
            ),
            update(
                3,
                serde_json::json!({"sessionUpdate": "tool_call_update", "toolCallId": "call-b", "status": "completed"}),
            ),
        ];
        let output = render_all(&mut renderer(Format::Raw, Detail::Abridged, 60), &entries);
        assert!(
            output.contains("✗ Tool  Edit a\n    denied by hook sh: no edits today\n"),
            "{output}"
        );
        assert!(
            output.contains("✓ Tool  Edit b  allowed by you"),
            "{output}"
        );
    }

    #[test]
    fn an_error_reply_to_the_prompt_ends_its_turn() {
        let entries = [
            prompt(0, "Do it"),
            asked(1, 100, "call-1", "Edit file"),
            entry(
                2,
                Direction::FromAgent,
                serde_json::json!({"id": 0, "error": {"code": -32603, "message": "Internal error"}}),
            ),
            prompt(10, "Go on"),
            said(11, "a", "Done."),
            end_turn(12),
        ];
        let output = render_all(&mut renderer(Format::Raw, Detail::Abridged, 60), &entries);
        assert!(output.contains("· Tool  Edit file"), "{output}");
        assert!(output.contains("✗ Internal error"), "{output}");
        assert_eq!(output.matches("Edit file").count(), 1, "{output}");
    }

    #[test]
    fn calls_open_when_the_session_ended_are_unfinished() {
        let entries = [prompt(0, "Do it"), asked(1, 100, "call-1", "Edit file")];
        let mut ended = renderer(Format::Raw, Detail::Abridged, 60);
        for entry in &entries {
            ended.render(entry);
        }
        let output = ended.finish(Session::Ended);
        assert!(!output.contains("waiting for permission"), "{output}");
        assert!(
            output.contains("· Tool  Edit file") && output.contains("unfinished"),
            "{output}"
        );
    }

    #[test]
    fn prompts_keep_their_lines_when_styled() {
        let output = plain(&render_all(
            &mut renderer(Format::Styled, Detail::Abridged, 60),
            &[prompt(0, "Resolved.\n- @ada: Use humantime\nContinue.")],
        ));
        assert!(
            output.contains("  - @ada: Use humantime\n  Continue.\n"),
            "{output}"
        );
    }
}

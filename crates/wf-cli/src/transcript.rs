//! Append-only terminal presentation of ACP transcripts: no cursor movement, so it works on a
//! pipe as well. Everything is laid out for a fixed width, so long lines never wrap by themselves.

use std::collections::HashMap;

use anstyle::{AnsiColor, Style};
use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use serde_json::Value;
use similar::TextDiff;
use wf_harness::{Direction, TranscriptEntry};

const OUTPUT_LIMIT: usize = 20;
const PROMPT_LIMIT: usize = 8;
const DEFAULT_WIDTH: usize = 100;
const MAX_WIDTH: usize = 100;
const MIN_WIDTH: usize = 40;
const GUTTER: &str = "  │ ";

const DIM: Style = Style::new().dimmed();

fn color(color: AnsiColor) -> Style {
    Style::new().fg_color(Some(color.into()))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Speaker {
    Agent,
    Thought,
}

/// What came last, to decide where blank lines go: tool calls follow each other closely.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Block {
    Nothing,
    Message,
    Tool,
    Other,
}

#[derive(Default)]
struct ToolCall {
    title: String,
    kind: String,
    announced: bool,
}

pub struct Renderer {
    /// `None` in raw mode: no wrapping and no truncating of lines.
    width: Option<usize>,
    raw: bool,
    full: bool,
    pending: String,
    speaker: Option<Speaker>,
    last: Block,
    mid_line: bool,
    tools: HashMap<String, ToolCall>,
}

impl Renderer {
    pub fn new(raw: bool, full: bool) -> Self {
        let width = terminal_size::terminal_size()
            .map_or(DEFAULT_WIDTH, |(width, _)| usize::from(width.0))
            .clamp(MIN_WIDTH, MAX_WIDTH);
        Self {
            width: (!raw).then_some(width),
            raw,
            full,
            pending: String::new(),
            speaker: None,
            last: Block::Nothing,
            mid_line: false,
            tools: HashMap::new(),
        }
    }

    fn styled(&self, text: &str, style: Style) -> String {
        if self.raw {
            text.to_owned()
        } else {
            format!("{style}{text}{style:#}")
        }
    }

    /// The separation before a new block.
    fn begin(&mut self, block: Block) -> String {
        let mut out = String::new();
        if std::mem::take(&mut self.mid_line) {
            out.push('\n');
        }
        let tight = self.last == Block::Tool && block == Block::Tool;
        if self.last != Block::Nothing && !tight {
            out.push('\n');
        }
        self.last = block;
        out
    }

    /// Cuts or wraps `text` to the width left after `indent` columns.
    fn fit(&self, text: &str, indent: usize, wrap: bool) -> Vec<String> {
        let text = anstream::adapter::strip_str(text).to_string();
        let Some(width) = self.width else {
            return text.lines().map(str::to_owned).collect();
        };
        let room = width.saturating_sub(indent).max(10);
        text.lines()
            .flat_map(|line| {
                let line = line.trim_end();
                if line.chars().count() <= room {
                    vec![line.to_owned()]
                } else if wrap {
                    let options = textwrap::Options::new(room).break_words(true);
                    textwrap::wrap(line, options)
                        .into_iter()
                        .map(|part| part.into_owned())
                        .collect()
                } else {
                    let cut: String = line.chars().take(room - 1).collect();
                    vec![format!("{cut}…")]
                }
            })
            .collect()
    }

    /// Lines under a header, limited unless `--full`.
    fn indented(
        &self,
        lines: Vec<String>,
        limit: usize,
        prefix: &str,
        style: impl Fn(&str) -> Style,
    ) -> String {
        let shown = if self.full {
            lines.len()
        } else {
            limit.min(lines.len())
        };
        let mut out = String::new();
        for line in &lines[..shown] {
            out.push_str(&self.styled(prefix, DIM));
            out.push_str(&self.styled(line, style(line)));
            out.push('\n');
        }
        if lines.len() > shown {
            let more = format!("… {} more lines (use --full)", lines.len() - shown);
            out.push_str(&self.styled(prefix, DIM));
            out.push_str(&self.styled(&more, DIM));
            out.push('\n');
        }
        out
    }

    fn header(&self, label: &str, text: &str, label_style: Style) -> String {
        format!("{} {text}\n", self.styled(label, label_style))
    }

    pub fn finish(&mut self) -> String {
        let text = std::mem::take(&mut self.pending);
        if text.trim().is_empty() {
            return String::new();
        }
        let rendered = if self.raw {
            text
        } else {
            markdown(&text, self.width.unwrap_or(DEFAULT_WIDTH))
        };
        let rendered = match self.speaker {
            Some(Speaker::Thought) => {
                let style = Style::new().dimmed().italic();
                let rendered = rendered.replace("\x1b[0m", &format!("\x1b[0m{style}"));
                self.styled(&rendered, style)
            }
            Some(Speaker::Agent) | None => rendered,
        };
        format!("{}\n", rendered.trim_end())
    }

    fn chunk(&mut self, speaker: Speaker, text: &str) -> String {
        let mut out = String::new();
        if self.speaker != Some(speaker) {
            out.push_str(&self.finish());
            self.speaker = Some(speaker);
            out.push_str(&self.begin(Block::Message));
        }
        if self.raw {
            self.mid_line = !text.ends_with('\n');
            out.push_str(text);
            return out;
        }
        self.pending.push_str(text);
        // Only whole paragraphs are rendered. A fence can span chunks.
        while let Some(boundary) = paragraph_boundary(&self.pending) {
            let tail = self.pending.split_off(boundary);
            let paragraph = self.finish();
            if !paragraph.is_empty() {
                out.push_str(&paragraph);
                out.push('\n');
            }
            self.pending = tail;
        }
        out
    }

    pub fn render(&mut self, entry: &TranscriptEntry) -> String {
        let message = &entry.message;
        let method = message["method"].as_str();
        if entry.direction == Direction::FromAgent && method == Some("session/update") {
            let update = &message["params"]["update"];
            let speaker = match update["sessionUpdate"].as_str() {
                Some("agent_message_chunk") => Some(Speaker::Agent),
                Some("agent_thought_chunk") => Some(Speaker::Thought),
                _ => None,
            };
            if let Some(speaker) = speaker {
                return self.chunk(speaker, update["content"]["text"].as_str().unwrap_or(""));
            }
        }
        let (block, body) = match entry.direction {
            Direction::AgentLog if !self.full => (Block::Nothing, String::new()),
            Direction::AgentLog => (
                Block::Other,
                self.header("log", message.as_str().unwrap_or(""), DIM),
            ),
            Direction::ToAgent => match method {
                Some("session/prompt") => (Block::Other, self.prompt(message)),
                _ => (Block::Nothing, String::new()),
            },
            // An allow nobody weighed goes without saying; the request above says what was asked.
            Direction::Wf
                if !self.full
                    && message["decision"]["permission"] == "allow"
                    && message["decision"]["reason"].is_null() =>
            {
                (Block::Nothing, String::new())
            }
            Direction::Wf => (Block::Other, self.decision(&message["decision"])),
            Direction::FromAgent => match method {
                Some("session/update") => self.update(&message["params"]["update"]),
                Some("session/request_permission") => {
                    let title = message["params"]["toolCall"]["title"]
                        .as_str()
                        .unwrap_or("?");
                    let text = first_line(title);
                    (
                        Block::Other,
                        self.header(
                            "?",
                            &format!("permission for {text}"),
                            color(AnsiColor::Yellow),
                        ),
                    )
                }
                _ => {
                    if let Some(reason) = message["result"]["stopReason"].as_str() {
                        (Block::Other, self.header("─", reason, DIM))
                    } else if let Some(error) = message["error"]["message"].as_str() {
                        (Block::Other, self.header("✗", error, color(AnsiColor::Red)))
                    } else {
                        (Block::Nothing, String::new())
                    }
                }
            },
        };
        if body.is_empty() {
            return body;
        }
        let mut out = self.finish();
        self.speaker = None;
        out.push_str(&self.begin(block));
        out.push_str(&body);
        out
    }

    /// `✓ allowed by rules: ...` or `✗ denied by a person`.
    fn decision(&self, decision: &Value) -> String {
        let (label, verb, style) = match decision["permission"].as_str() {
            Some("allow") => ("✓", "allowed", color(AnsiColor::Green)),
            _ => ("✗", "denied", color(AnsiColor::Red)),
        };
        let by = decision["by"].as_str().unwrap_or("?");
        let text = match decision["reason"].as_str() {
            Some(reason) => format!("{verb} by {by}: {}", first_line(reason)),
            None => format!("{verb} by {by}"),
        };
        self.header(label, &text, style)
    }

    fn prompt(&self, message: &Value) -> String {
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
        let mut out = self.header(
            "prompt",
            "",
            Style::new().bold().fg_color(Some(AnsiColor::Cyan.into())),
        );
        out.truncate(out.trim_end().len());
        out.push('\n');
        let lines = self.fit(&text, 2, true);
        out.push_str(&self.indented(lines, PROMPT_LIMIT, "  ", |_| Style::new()));
        out
    }

    fn update(&mut self, update: &Value) -> (Block, String) {
        match update["sessionUpdate"].as_str() {
            Some(kind @ ("tool_call" | "tool_call_update")) => {
                (Block::Tool, self.tool(kind == "tool_call", update))
            }
            Some("plan") => {
                let mut out = self.header("plan", "", Style::new().bold());
                out.truncate(out.trim_end().len());
                out.push('\n');
                for entry in update["entries"].as_array().into_iter().flatten() {
                    let marker = match entry["status"].as_str() {
                        Some("completed") => "✓",
                        Some("in_progress") => "◐",
                        _ => "○",
                    };
                    let text = first_line(entry["content"].as_str().unwrap_or("?"));
                    let line = self.fit(text, 4, false).concat();
                    out.push_str(&format!("  {marker} {line}\n"));
                }
                (Block::Other, out)
            }
            _ => (Block::Nothing, String::new()),
        }
    }

    fn tool(&mut self, is_start: bool, update: &Value) -> String {
        let id = update["toolCallId"].as_str().unwrap_or("?").to_owned();
        let call = self.tools.entry(id).or_default();
        let title = update["title"].as_str();
        if let Some(title) = title {
            call.title = title.to_owned();
        }
        if let Some(kind) = update["kind"].as_str() {
            call.kind = kind.to_owned();
        }
        // A call starts out with its tool's name for a title; the command comes with an update.
        let generic = is_start
            && update["name"]
                .as_str()
                .is_none_or(|name| Some(name) == title);
        let announce = !call.announced && !generic;
        if announce {
            call.announced = true;
        }
        let (title, kind) = (call.title.clone(), call.kind.clone());

        let mut out = String::new();
        if announce {
            let icon = if kind == "execute" { "$" } else { "●" };
            let text = self.fit(first_line(&title), 2, self.full);
            let mut lines = text.into_iter();
            out.push_str(&self.header(
                icon,
                lines.next().as_deref().unwrap_or(""),
                Style::new().bold(),
            ));
            for line in lines {
                out.push_str(&format!("  {line}\n"));
            }
        }
        for block in update["content"].as_array().into_iter().flatten() {
            match block["type"].as_str() {
                Some("diff") => out.push_str(&self.diff(block)),
                Some("content") => {
                    if let Some(text) = block["content"]["text"].as_str() {
                        let lines = self.fit(text.trim_end(), GUTTER.chars().count(), self.full);
                        out.push_str(&self.indented(lines, OUTPUT_LIMIT, GUTTER, |_| DIM));
                    }
                }
                _ => {}
            }
        }
        if update["status"].as_str() == Some("failed") {
            out.push_str(&self.header("✗", "failed", color(AnsiColor::Red)));
        }
        out
    }

    fn diff(&self, block: &Value) -> String {
        let path = block["path"].as_str().unwrap_or("?");
        let old = block["oldText"].as_str().unwrap_or("");
        let new = block["newText"].as_str().unwrap_or("");
        let unified = TextDiff::from_lines(old, new)
            .unified_diff()
            .context_radius(2)
            .to_string();
        let body: Vec<_> = unified
            .lines()
            .skip_while(|line| line.starts_with("---") || line.starts_with("+++"))
            .collect();
        let lines = self.fit(&body.join("\n"), GUTTER.chars().count(), false);
        let mut out = self.header("●", &format!("edit {path}"), Style::new().bold());
        out.push_str(&self.indented(lines, OUTPUT_LIMIT, GUTTER, |line| {
            if line.starts_with('+') {
                color(AnsiColor::Green)
            } else if line.starts_with('-') {
                color(AnsiColor::Red)
            } else if line.starts_with("@@") {
                color(AnsiColor::Cyan)
            } else {
                DIM
            }
        }));
        out
    }
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("")
}

fn paragraph_boundary(text: &str) -> Option<usize> {
    let mut fence: Option<(char, usize)> = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        if !line.ends_with('\n') {
            break;
        }
        let trimmed = line.trim_start();
        let marker = trimmed.chars().next().unwrap_or(' ');
        let count = trimmed
            .chars()
            .take_while(|character| *character == marker)
            .count();
        if matches!(marker, '`' | '~') && count >= 3 {
            match fence {
                None => fence = Some((marker, count)),
                Some((opening, length))
                    if marker == opening
                        && count >= length
                        && trimmed[count..].trim().is_empty() =>
                {
                    fence = None
                }
                Some(_) => {}
            }
        }
        offset += line.len();
        if fence.is_none() && line.trim().is_empty() {
            return Some(offset);
        }
    }
    None
}

/// Markdown for a terminal: wrapped to `width`, with hanging indents for lists and quotes.
fn markdown(text: &str, width: usize) -> String {
    let mut writer = MarkdownWriter {
        width,
        ..MarkdownWriter::default()
    };
    for event in Parser::new(text) {
        writer.event(event);
    }
    writer.out
}

#[derive(Default)]
struct MarkdownWriter {
    width: usize,
    out: String,
    /// The inline text of the block being written.
    line: String,
    /// Each open list: the next number, or `None` for bullets.
    lists: Vec<Option<u64>>,
    quotes: usize,
    /// The marker of the item being written, until its first line is out.
    marker: Option<String>,
    styles: Vec<Style>,
    in_code: bool,
}

impl MarkdownWriter {
    fn indent(&self) -> String {
        format!(
            "{}{}",
            "│ ".repeat(self.quotes),
            "  ".repeat(self.lists.len().saturating_sub(1))
        )
    }

    fn flush(&mut self) {
        if self.line.is_empty() {
            return;
        }
        let line = std::mem::take(&mut self.line);
        let indent = self.indent();
        let first = match self.marker.take() {
            Some(marker) => format!("{indent}{marker}"),
            None if self.lists.is_empty() => indent.clone(),
            None => format!("{indent}  "),
        };
        let rest = format!(
            "{indent}{}",
            " ".repeat(first.chars().count() - indent.chars().count())
        );
        let options = textwrap::Options::new(self.width)
            .initial_indent(&first)
            .subsequent_indent(&rest);
        self.out.push_str(&textwrap::fill(&line, options));
        self.out.push('\n');
    }

    fn blank(&mut self) {
        if self.lists.is_empty() {
            self.out.push('\n');
        }
    }

    fn style_on(&mut self, style: Style) {
        self.line.push_str(&style.to_string());
        self.styles.push(style);
    }

    fn style_off(&mut self) {
        self.styles.pop();
        self.line.push_str(&format!("{:#}", Style::new().bold()));
        for style in &self.styles {
            self.line.push_str(&style.to_string());
        }
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(Tag::Strong) => self.style_on(Style::new().bold()),
            Event::Start(Tag::Emphasis) => self.style_on(Style::new().italic()),
            Event::Start(Tag::Heading { .. }) => self.style_on(Style::new().bold()),
            Event::End(TagEnd::Strong | TagEnd::Emphasis) => self.style_off(),
            Event::End(TagEnd::Heading(_)) => {
                self.style_off();
                self.flush();
                self.blank();
            }
            Event::Start(Tag::List(start)) => {
                self.flush();
                self.lists.push(start);
            }
            Event::End(TagEnd::List(_)) => {
                self.lists.pop();
                self.blank();
            }
            Event::Start(Tag::Item) => {
                self.flush();
                let marker = match self.lists.last_mut() {
                    Some(Some(number)) => {
                        let marker = format!("{number}. ");
                        *number += 1;
                        marker
                    }
                    _ => "• ".to_owned(),
                };
                self.marker = Some(marker);
            }
            Event::End(TagEnd::Item) => self.flush(),
            Event::Start(Tag::BlockQuote(_)) => {
                self.flush();
                self.quotes += 1;
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                self.flush();
                self.quotes -= 1;
                self.blank();
            }
            Event::End(TagEnd::Paragraph) => {
                self.flush();
                self.blank();
            }
            Event::Start(Tag::CodeBlock(_)) => {
                self.flush();
                self.in_code = true;
            }
            Event::End(TagEnd::CodeBlock) => {
                self.in_code = false;
                self.blank();
            }
            Event::Text(text) if self.in_code => {
                let indent = self.indent();
                for line in text.lines() {
                    self.out
                        .push_str(&format!("{indent}    {DIM}{line}{DIM:#}\n"));
                }
            }
            Event::Text(text) => self.line.push_str(&text),
            Event::Code(text) => {
                let code = color(AnsiColor::Cyan);
                self.line.push_str(&format!("{code}{text}{code:#}"));
                for style in &self.styles {
                    self.line.push_str(&style.to_string());
                }
            }
            Event::SoftBreak => self.line.push(' '),
            Event::HardBreak => self.line.push('\n'),
            Event::Rule => {
                self.flush();
                self.out
                    .push_str(&format!("{DIM}{}{DIM:#}\n", "─".repeat(self.width.min(40))));
                self.blank();
            }
            Event::Html(text) | Event::InlineHtml(text) => self.line.push_str(&text),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(text: &str) -> String {
        anstream::adapter::strip_str(text).to_string()
    }

    fn update(update: Value) -> TranscriptEntry {
        TranscriptEntry {
            at: jiff::Timestamp::UNIX_EPOCH,
            direction: Direction::FromAgent,
            message: serde_json::json!({"method": "session/update", "params": {"update": update}}),
        }
    }

    #[test]
    fn chunk_boundaries_do_not_break_markdown_or_fences() {
        let mut renderer = Renderer::new(false, false);
        let output: String = [
            "**Hel",
            "lo**\n\n```",
            "rust\n\nlet value = 1;\n",
            "```\n\n",
        ]
        .into_iter()
        .map(|text| renderer.chunk(Speaker::Agent, text))
        .collect();
        assert!(output.contains("Hello"));
        assert!(!output.contains("**"));
        assert!(output.contains("let value = 1;"));
        assert!(renderer.pending.is_empty());
    }

    #[test]
    fn list_items_wrap_under_their_text() {
        let output = plain(&markdown(
            "- one two three four five six seven\n- next\n\nAfter.",
            20,
        ));
        assert_eq!(
            output,
            "• one two three four\n  five six seven\n• next\n\nAfter.\n\n"
        );
    }

    #[test]
    fn a_tool_call_is_announced_once_and_its_output_is_cut_to_the_width() {
        let mut renderer = Renderer::new(false, false);
        renderer.width = Some(40);
        let long = "x".repeat(100);
        let steps = [
            serde_json::json!({"sessionUpdate": "tool_call", "toolCallId": "1", "name": "shell", "title": "shell", "kind": "execute", "status": "pending"}),
            serde_json::json!({"sessionUpdate": "tool_call_update", "toolCallId": "1", "kind": "execute", "title": format!("echo {long}"), "status": "in_progress"}),
            serde_json::json!({"sessionUpdate": "tool_call_update", "toolCallId": "1", "kind": "execute", "title": format!("echo {long}"), "status": "in_progress"}),
            serde_json::json!({"sessionUpdate": "tool_call_update", "toolCallId": "1", "status": "completed",
                "content": [{"type": "content", "content": {"type": "text", "text": format!("{long}\nsecond\n")}}]}),
        ];
        let output: String = steps
            .into_iter()
            .map(|step| plain(&renderer.render(&update(step))))
            .collect();
        assert_eq!(output.matches("$ echo").count(), 1, "{output}");
        assert!(
            output.lines().all(|line| line.chars().count() <= 40),
            "{output}"
        );
        assert!(output.contains("  │ second"));
    }

    #[test]
    fn harness_logs_and_unweighed_allows_show_only_with_full() {
        let entry = |direction, message| TranscriptEntry {
            at: jiff::Timestamp::UNIX_EPOCH,
            direction,
            message,
        };
        let log = entry(Direction::AgentLog, serde_json::json!("[session/create] x"));
        let allow = entry(
            Direction::Wf,
            serde_json::json!({"decision": {"permission": "allow", "by": "the agent's permissions"}}),
        );
        let weighed = entry(
            Direction::Wf,
            serde_json::json!({"decision": {"permission": "allow", "by": "rules", "reason": "safe"}}),
        );
        let denied = entry(
            Direction::Wf,
            serde_json::json!({"decision": {"permission": "deny", "by": "rules"}}),
        );
        let mut quiet = Renderer::new(false, false);
        assert_eq!(quiet.render(&log), "");
        assert_eq!(quiet.render(&allow), "");
        assert!(plain(&quiet.render(&weighed)).contains("allowed by rules: safe"));
        assert!(plain(&quiet.render(&denied)).contains("denied by rules"));
        let mut full = Renderer::new(false, true);
        assert!(plain(&full.render(&log)).contains("[session/create] x"));
        assert!(plain(&full.render(&allow)).contains("allowed by the agent's permissions"));
    }

    #[test]
    fn output_is_capped_unless_full() {
        let text: String = (0..30).map(|index| format!("line {index}\n")).collect();
        let call = serde_json::json!({"sessionUpdate": "tool_call_update", "toolCallId": "1", "status": "completed",
            "content": [{"type": "content", "content": {"type": "text", "text": text}}]});
        let capped = plain(&Renderer::new(false, false).render(&update(call.clone())));
        assert!(capped.contains("… 10 more lines (use --full)"));
        assert!(!capped.contains("line 29"));
        let full = plain(&Renderer::new(false, true).render(&update(call)));
        assert!(full.contains("line 29") && !full.contains("more lines"));
    }

    #[test]
    fn raw_keeps_markdown_and_has_no_escape_codes() {
        let mut renderer = Renderer::new(true, false);
        let output = renderer.chunk(Speaker::Agent, "**kept**");
        let output = output + &renderer.finish();
        assert!(output.contains("**kept**") && !output.contains('\x1b'));
    }

    #[test]
    fn diffs_show_the_path_and_changed_lines() {
        let mut renderer = Renderer::new(true, false);
        let output = renderer.render(&update(serde_json::json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "edit-1",
            "content": [{"type": "diff", "path": "src/main.rs", "oldText": "before\n", "newText": "after\n"}]
        })));
        assert!(output.contains("edit src/main.rs"));
        assert!(output.contains("-before") && output.contains("+after"));
        assert!(!output.contains("+++"));
    }
}

//! Append-only terminal presentation of ACP transcripts: no cursor movement, so it works on a
//! pipe as well. Everything is laid out for a fixed width, so long lines never wrap by themselves.
//!
//! A transcript reads as turns: what wf told the agent, then what the agent did, closed by a
//! footer. Each tool call is one block, printed once it is finished, that holds the call, the
//! answer to its permission request and its result. A call still open when the reader catches
//! up shows as a running line first.

use std::collections::HashMap;

use anstyle::{AnsiColor, Style};
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};
use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use serde_json::Value;
use similar::TextDiff;
use wf_harness::{Decision, Direction, Permission, TranscriptEntry};

/// Lines kept from the start and the end of long tool output: the tail holds the errors.
const HEAD: usize = 5;
const TAIL: usize = 5;
const DEFAULT_WIDTH: usize = 100;
const MAX_WIDTH: usize = 120;
const MIN_WIDTH: usize = 40;
/// Where everything inside a turn starts.
const MARGIN: usize = 2;
/// Where what belongs to a tool call starts, under its name.
const NESTED: usize = 4;

const DIM: Style = Style::new().dimmed();
const BOLD: Style = Style::new().bold();

fn color(color: AnsiColor) -> Style {
    Style::new().fg_color(Some(color.into()))
}

/// The width to lay out for: the terminal's, within readable bounds.
pub fn terminal_width() -> usize {
    terminal_size::terminal_size()
        .map_or(DEFAULT_WIDTH, |(width, _)| usize::from(width.0))
        .clamp(MIN_WIDTH, MAX_WIDTH)
}

/// How the transcript is drawn.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Color, box characters and rendered markdown.
    Styled,
    /// The same layout as plain text: markdown as written, no color and no box characters.
    Raw,
}

/// How much of the transcript is shown.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Detail {
    /// Briefs, commands and output cut short, and the harness's own lines left out.
    Abridged,
    Full,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Thinking {
    Message,
    Thought,
}

/// Agent text not printed yet: only whole paragraphs are rendered, as markdown can span chunks.
struct Prose {
    kind: Thinking,
    text: String,
}

/// What came last, to decide where blank lines go: tool calls follow each other closely, and
/// a turn's header sits right on top of its first block.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Block {
    Nothing,
    Header,
    Prose,
    Tool,
    Log,
    Other,
}

enum Turn {
    Closed,
    /// wf prompted the agent, which has not answered yet.
    Prompted {
        at: Timestamp,
    },
    /// The agent's header is out.
    Answering {
        since: Timestamp,
    },
}

enum Asked {
    Not,
    Waiting,
    Answered(Decision),
}

/// How a call ended, by the time of its last update.
enum Outcome {
    Completed(Timestamp),
    Failed(Timestamp),
}

struct ToolCall {
    id: String,
    /// The tool's own name when the agent gives one, such as `Bash`, otherwise its kind.
    name: String,
    title: String,
    /// When the call started running: when it was made, or when its permission request was
    /// answered.
    since: Timestamp,
    content: Vec<Value>,
    asked: Asked,
    /// Whether a running line went out for the permission state it is in now.
    shown: Option<bool>,
}

pub struct Renderer {
    agent: String,
    width: usize,
    format: Format,
    detail: Detail,
    time_zone: TimeZone,
    prose: Option<Prose>,
    last: Block,
    turn: Turn,
    prompts: usize,
    /// Open calls, in the order they started.
    calls: Vec<ToolCall>,
    /// The call each open permission request is about, by the request's JSON-RPC id.
    requests: HashMap<String, String>,
    /// wf records its decision right before it answers the request.
    decision: Option<Decision>,
}

impl Renderer {
    pub fn new(
        agent: &str,
        width: usize,
        format: Format,
        detail: Detail,
        time_zone: TimeZone,
    ) -> Self {
        Self {
            agent: agent.to_owned(),
            width,
            format,
            detail,
            time_zone,
            prose: None,
            last: Block::Nothing,
            turn: Turn::Closed,
            prompts: 0,
            calls: Vec::new(),
            requests: HashMap::new(),
            decision: None,
        }
    }

    fn styled(&self, text: &str, style: Style) -> String {
        match self.format {
            Format::Raw => text.to_owned(),
            Format::Styled if text.is_empty() => String::new(),
            Format::Styled => format!("{style}{text}{style:#}"),
        }
    }

    fn gutter(&self) -> &'static str {
        match self.format {
            Format::Styled => "│ ",
            Format::Raw => "| ",
        }
    }

    /// The separation before a new block.
    fn begin(&mut self, block: Block) -> String {
        let tight = matches!(self.last, Block::Nothing | Block::Header)
            || (self.last == block && matches!(block, Block::Tool | Block::Log));
        self.last = block;
        if tight {
            String::new()
        } else {
            "\n".to_owned()
        }
    }

    /// One line of `left` with `right` flush against the right edge. `left_width` is how many
    /// columns `left` takes without its escape codes.
    fn spread(&self, left: &str, left_width: usize, right: &str, right_style: Style) -> String {
        if right.is_empty() {
            return format!("{left}\n");
        }
        let padding = self
            .width
            .saturating_sub(left_width + width_of(right))
            .max(2);
        format!(
            "{left}{}{}\n",
            " ".repeat(padding),
            self.styled(right, right_style)
        )
    }

    /// Cuts or wraps each line of `text` to the width left after `indent` columns.
    fn fit(&self, text: &str, indent: usize, wrap: bool) -> Vec<String> {
        let room = self.width.saturating_sub(indent).max(10);
        text.lines()
            .flat_map(|line| {
                let line = line.trim_end();
                if width_of(line) <= room {
                    vec![line.to_owned()]
                } else if wrap {
                    let options = textwrap::Options::new(room).break_words(true);
                    textwrap::wrap(line, options)
                        .into_iter()
                        .map(|part| part.into_owned())
                        .collect()
                } else {
                    vec![cut(line, room)]
                }
            })
            .collect()
    }

    /// The first and last lines when there are many, unless `--full`.
    fn abridged(&self, lines: Vec<String>) -> Vec<Line> {
        if self.detail == Detail::Full || lines.len() <= HEAD + TAIL + 1 {
            return lines.into_iter().map(Line::Text).collect();
        }
        let hidden = lines.len() - HEAD - TAIL;
        let mut kept: Vec<Line> = Vec::with_capacity(HEAD + TAIL + 1);
        let mut lines = lines.into_iter();
        kept.extend(lines.by_ref().take(HEAD).map(Line::Text));
        kept.push(Line::Hidden(hidden));
        kept.extend(lines.skip(hidden).map(Line::Text));
        kept
    }

    /// Lines in a gutter under a tool call.
    fn guttered(
        &self,
        lines: Vec<Line>,
        gutter_style: Style,
        style: impl Fn(&str) -> Style,
    ) -> String {
        let indent = " ".repeat(NESTED);
        let gutter = self.gutter();
        let mut out = String::new();
        for line in lines {
            out.push_str(&indent);
            match line {
                Line::Text(text) if text.is_empty() => {
                    out.push_str(&self.styled(gutter.trim_end(), gutter_style));
                }
                Line::Text(text) => {
                    out.push_str(&self.styled(gutter, gutter_style));
                    out.push_str(&self.styled(&text, style(&text)));
                }
                Line::Hidden(count) => {
                    out.push_str(&self.styled(gutter, gutter_style));
                    let text = format!("… {count} lines hidden");
                    out.push_str(&self.styled(&text, DIM));
                }
            }
            out.push('\n');
        }
        out
    }

    fn time(&self, at: Timestamp) -> String {
        at.to_zoned(self.time_zone.clone())
            .strftime("%H:%M")
            .to_string()
    }

    /// `brief  00:49`, with what goes at the right edge.
    fn turn_header(&mut self, speaker: &str, style: Style, at: Timestamp, right: &str) -> String {
        let mut out = self.begin(Block::Other);
        self.last = Block::Header;
        let time = self.time(at);
        let left = format!(
            "{}  {}",
            self.styled(speaker, style),
            self.styled(&time, DIM)
        );
        out.push_str(&self.spread(&left, width_of(speaker) + 2 + width_of(&time), right, DIM));
        out
    }

    /// Opens the agent's part of the turn, unless it is open already.
    fn answering(&mut self, at: Timestamp) -> String {
        let since = match self.turn {
            Turn::Answering { .. } => return String::new(),
            Turn::Prompted { at } => at,
            Turn::Closed => at,
        };
        self.turn = Turn::Answering { since };
        let agent = self.agent.clone();
        self.turn_header(
            &agent,
            BOLD.fg_color(Some(AnsiColor::Magenta.into())),
            at,
            "",
        )
    }

    /// Prints the agent text held back, whole or not.
    fn flush_prose(&mut self) -> String {
        let Some(prose) = self.prose.take() else {
            return String::new();
        };
        let text = self.prose_paragraphs(prose.kind, &prose.text);
        if text.is_empty() {
            return text;
        }
        let mut out = self.begin(Block::Prose);
        out.push_str(&text);
        out
    }

    fn prose_paragraphs(&self, kind: Thinking, text: &str) -> String {
        if text.trim().is_empty() {
            return String::new();
        }
        let margin = " ".repeat(MARGIN);
        let room = self.width - MARGIN;
        let rendered = match self.format {
            Format::Raw => self.fit(text.trim_end(), MARGIN, true).join("\n"),
            Format::Styled => markdown(text, room),
        };
        let rendered = match (kind, self.format) {
            (Thinking::Thought, Format::Styled) => {
                let style = DIM.italic();
                let rendered = rendered.replace("\x1b[0m", &format!("\x1b[0m{style}"));
                rendered
                    .trim_end()
                    .lines()
                    .map(|line| format!("{style}{line}{style:#}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            (Thinking::Message, _) | (Thinking::Thought, Format::Raw) => rendered,
        };
        let mut out = String::new();
        for line in rendered.trim_end().lines() {
            if !anstream::adapter::strip_str(line)
                .to_string()
                .trim()
                .is_empty()
            {
                out.push_str(&margin);
                out.push_str(line);
            }
            out.push('\n');
        }
        out
    }

    fn chunk(&mut self, at: Timestamp, kind: Thinking, text: &str) -> String {
        let mut out = String::new();
        if self.prose.as_ref().is_some_and(|prose| prose.kind != kind) {
            out.push_str(&self.flush_prose());
        }
        out.push_str(&self.answering(at));
        let prose = self.prose.get_or_insert_with(|| Prose {
            kind,
            text: String::new(),
        });
        prose.text.push_str(text);
        while let Some(boundary) = self
            .prose
            .as_ref()
            .and_then(|prose| paragraph_boundary(&prose.text))
        {
            let prose = self.prose.as_mut().expect("a boundary was found in it");
            let tail = prose.text.split_off(boundary);
            out.push_str(&self.flush_prose());
            self.prose = Some(Prose { kind, text: tail });
        }
        out
    }

    /// What there is to say once everything written so far is rendered: the calls that are
    /// still open. Call it whenever the transcript has no more lines for now.
    pub fn caught_up(&mut self) -> String {
        let mut out = String::new();
        for index in 0..self.calls.len() {
            let waiting = matches!(self.calls[index].asked, Asked::Waiting);
            if self.calls[index].shown == Some(waiting) {
                continue;
            }
            self.calls[index].shown = Some(waiting);
            out.push_str(&self.begin(Block::Tool));
            let right = if waiting {
                "waiting for permission"
            } else {
                "running"
            };
            let call = &self.calls[index];
            out.push_str(&self.call_header(call, ("…", DIM), right, DIM));
            out.push_str(&self.reason(call));
        }
        out
    }

    /// Everything still held back, at the end of the transcript.
    pub fn finish(&mut self) -> String {
        let mut out = self.flush_prose();
        out.push_str(&self.caught_up());
        out
    }

    pub fn render(&mut self, entry: &TranscriptEntry) -> String {
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
                return self.chunk(entry.at, kind, text);
            }
        }
        match entry.direction {
            Direction::AgentLog => match self.detail {
                Detail::Abridged => String::new(),
                Detail::Full => {
                    let text = message.as_str().unwrap_or("");
                    let lines = self.fit(text, MARGIN + 4, true);
                    let mut out = self.flush_prose();
                    out.push_str(&self.begin(Block::Log));
                    for line in lines {
                        let label = self.styled("log", DIM);
                        out.push_str(&format!(
                            "{}{label} {}\n",
                            " ".repeat(MARGIN),
                            self.styled(&line, DIM)
                        ));
                    }
                    out
                }
            },
            Direction::ToAgent => match method {
                Some("session/prompt") => self.prompt(entry.at, message),
                Some(_) => String::new(),
                None => {
                    // The answer to a permission request.
                    let id = message["id"].to_string();
                    if let Some(call_id) = self.requests.remove(&id)
                        && let Some(call) = self.calls.iter_mut().find(|call| call.id == call_id)
                    {
                        call.since = entry.at;
                        call.asked = match self.decision.take() {
                            Some(decision) => Asked::Answered(decision),
                            None => Asked::Not,
                        };
                    }
                    String::new()
                }
            },
            Direction::Wf => {
                self.decision = serde_json::from_value(message["decision"].clone()).ok();
                String::new()
            }
            Direction::FromAgent => match method {
                Some("session/update") => self.update(entry.at, &message["params"]["update"]),
                Some("session/request_permission") => {
                    let tool_call = &message["params"]["toolCall"];
                    let call = self.call(entry.at, tool_call);
                    call.asked = Asked::Waiting;
                    let call_id = call.id.clone();
                    self.requests.insert(message["id"].to_string(), call_id);
                    String::new()
                }
                Some(_) => String::new(),
                None => {
                    if let Some(reason) = message["result"]["stopReason"].as_str() {
                        self.footer(entry.at, reason)
                    } else if let Some(error) = message["error"]["message"].as_str() {
                        let mut out = self.flush_prose();
                        out.push_str(&self.answering(entry.at));
                        out.push_str(&self.begin(Block::Other));
                        let text = cut(first_line(error), self.width - MARGIN - 2);
                        out.push_str(&format!(
                            "{}{} {}\n",
                            " ".repeat(MARGIN),
                            self.styled("✗", color(AnsiColor::Red)),
                            self.styled(&text, color(AnsiColor::Red))
                        ));
                        out
                    } else {
                        String::new()
                    }
                }
            },
        }
    }

    /// The first prompt is the brief; wf sends the later ones to keep the session going.
    fn prompt(&mut self, at: Timestamp, message: &Value) -> String {
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
        let speaker = if self.prompts == 0 { "brief" } else { "wf" };
        self.prompts += 1;
        let mut out = self.flush_prose();
        self.turn = Turn::Prompted { at };
        let lines = text.trim_end().lines().count();
        match self.detail {
            Detail::Abridged => {
                let count = match lines {
                    0 | 1 => String::new(),
                    lines => format!("{lines} lines"),
                };
                out.push_str(&self.turn_header(speaker, color(AnsiColor::Cyan).bold(), at, &count));
                let summary = text
                    .lines()
                    .map(|line| line.trim().trim_start_matches('#').trim())
                    .find(|line| !line.is_empty())
                    .unwrap_or("");
                out.push_str(&format!(
                    "{}{}\n",
                    " ".repeat(MARGIN),
                    cut(summary, self.width - MARGIN)
                ));
            }
            Detail::Full => {
                out.push_str(&self.turn_header(speaker, color(AnsiColor::Cyan).bold(), at, ""));
                out.push_str(&self.prose_paragraphs(Thinking::Message, &text));
            }
        }
        self.last = Block::Other;
        out
    }

    /// `finished` and how long the turn took.
    fn footer(&mut self, at: Timestamp, reason: &str) -> String {
        let mut out = self.flush_prose();
        let since = match self.turn {
            Turn::Prompted { at } | Turn::Answering { since: at } => Some(at),
            Turn::Closed => None,
        };
        self.turn = Turn::Closed;
        let (text, style) = match reason {
            "end_turn" => ("finished", DIM),
            "cancelled" => ("stopped", color(AnsiColor::Yellow)),
            "max_tokens" => ("stopped: out of tokens", color(AnsiColor::Yellow)),
            "max_turn_requests" => ("stopped: too many requests", color(AnsiColor::Yellow)),
            "refusal" => ("refused", color(AnsiColor::Yellow)),
            other => (other, color(AnsiColor::Yellow)),
        };
        out.push_str(&self.begin(Block::Other));
        let elapsed = since
            .map(|since| duration(at.duration_since(since)))
            .unwrap_or_default();
        out.push_str(&self.spread(&self.styled(text, style), width_of(text), &elapsed, DIM));
        out
    }

    fn update(&mut self, at: Timestamp, update: &Value) -> String {
        match update["sessionUpdate"].as_str() {
            Some("tool_call" | "tool_call_update") => self.tool(at, update),
            Some("plan") => {
                let mut out = self.flush_prose();
                out.push_str(&self.answering(at));
                out.push_str(&self.begin(Block::Other));
                out.push_str(&format!(
                    "{}{}\n",
                    " ".repeat(MARGIN),
                    self.styled("plan", BOLD)
                ));
                for entry in update["entries"].as_array().into_iter().flatten() {
                    let marker = match entry["status"].as_str() {
                        Some("completed") => "✓",
                        Some("in_progress") => "◐",
                        _ => "○",
                    };
                    let text = first_line(entry["content"].as_str().unwrap_or("?"));
                    let line = cut(text, self.width - NESTED - 2);
                    out.push_str(&format!("{}{marker} {line}\n", " ".repeat(NESTED)));
                }
                out
            }
            _ => String::new(),
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
                    shown: None,
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
            call.name = kind_name(kind).to_owned();
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

    fn tool(&mut self, at: Timestamp, update: &Value) -> String {
        let mut out = self.flush_prose();
        out.push_str(&self.answering(at));
        let outcome = match update["status"].as_str() {
            Some("completed") => Some(Outcome::Completed(at)),
            Some("failed") => Some(Outcome::Failed(at)),
            _ => None,
        };
        let id = self.call(at, update).id.clone();
        let Some(outcome) = outcome else {
            return out;
        };
        let index = self
            .calls
            .iter()
            .position(|call| call.id == id)
            .expect("the call was just taken in");
        let call = self.calls.remove(index);
        out.push_str(&self.begin(Block::Tool));
        out.push_str(&self.finished_call(&call, &outcome));
        out
    }

    /// `✓ Bash  cargo test  allowed by you     3.2s`, wrapped instead of cut with `--full`.
    fn call_header(
        &self,
        call: &ToolCall,
        (glyph, glyph_style): (&str, Style),
        right: &str,
        right_style: Style,
    ) -> String {
        let name = if call.name.is_empty() {
            "Tool"
        } else {
            &call.name
        };
        let summary = summary(call, name);
        let mut suffix = Vec::new();
        if let Some(changes) = diff_counts(&call.content) {
            suffix.push(changes);
        }
        let verdict = self.verdict(&call.asked);
        if let Some((verdict, _)) = &verdict {
            suffix.push(verdict.clone());
        }
        let suffix = suffix.join("  ");
        let indent = MARGIN + width_of(glyph) + 1 + width_of(name) + 2;
        let reserved = [&suffix, right]
            .iter()
            .filter(|text| !text.is_empty())
            .map(|text| width_of(text) + 2)
            .sum::<usize>();
        let room = self.width.saturating_sub(indent + reserved).max(10);
        let mut lines = match self.detail {
            Detail::Abridged => {
                let first = match summary.lines().nth(1) {
                    Some(_) => format!("{} …", first_line(&summary)),
                    None => summary.clone(),
                };
                vec![cut(&first, room)]
            }
            Detail::Full => self.wrap_under(&summary, indent, reserved),
        }
        .into_iter();
        let first = lines.next().unwrap_or_default();
        let mut left = format!(
            "{}{} {}  {first}",
            " ".repeat(MARGIN),
            self.styled(glyph, glyph_style),
            self.styled(name, BOLD)
        );
        let mut left_width = indent + width_of(&first);
        if !suffix.is_empty() {
            let suffix_style = match &verdict {
                Some((_, Permission::Deny)) => color(AnsiColor::Red),
                Some((_, Permission::Allow)) | None => DIM,
            };
            left.push_str("  ");
            left.push_str(&self.styled(&suffix, suffix_style));
            left_width += 2 + width_of(&suffix);
        }
        let mut out = self.spread(&left, left_width, right, right_style);
        for line in lines {
            out.push_str(&format!("{}{line}\n", " ".repeat(indent)));
        }
        out
    }

    /// `text` wrapped to the width after `indent` columns, its first line shorter by `reserved`.
    fn wrap_under(&self, text: &str, indent: usize, reserved: usize) -> Vec<String> {
        let room = self.width.saturating_sub(indent).max(10);
        let placeholder = " ".repeat(reserved.min(room / 2));
        let mut lines = Vec::new();
        for line in text.lines() {
            let initial = if lines.is_empty() {
                placeholder.as_str()
            } else {
                ""
            };
            let options = textwrap::Options::new(room)
                .initial_indent(initial)
                .break_words(true);
            for part in textwrap::wrap(line, options) {
                lines.push(part.into_owned());
            }
        }
        if let Some(first) = lines.first_mut() {
            *first = first[placeholder.len().min(first.len())..].to_owned();
        }
        lines
    }

    /// `allowed by you`, or nothing for an allow nobody weighed, unless `--full`.
    fn verdict(&self, asked: &Asked) -> Option<(String, Permission)> {
        let Asked::Answered(decision) = asked else {
            return None;
        };
        let unweighed = decision.by == Decision::BY_FIXED_PERMISSIONS
            && decision.permission == Permission::Allow
            && decision.reason.is_none();
        if unweighed && self.detail == Detail::Abridged {
            return None;
        }
        let by = match decision.by.as_str() {
            Decision::BY_PRINCIPAL => "you",
            by => by,
        };
        let verb = match decision.permission {
            Permission::Allow => "allowed",
            Permission::Deny => "denied",
        };
        Some((format!("{verb} by {by}"), decision.permission))
    }

    /// Why the permission request of `call` was decided as it was.
    fn reason(&self, call: &ToolCall) -> String {
        let Asked::Answered(Decision {
            reason: Some(reason),
            ..
        }) = &call.asked
        else {
            return String::new();
        };
        self.fit(reason, NESTED, true)
            .iter()
            .map(|line| format!("{}{}\n", " ".repeat(NESTED), self.styled(line, DIM)))
            .collect()
    }

    fn finished_call(&self, call: &ToolCall, outcome: &Outcome) -> String {
        let (glyph, state, style) = match outcome {
            Outcome::Completed(at) => (
                ("✓", color(AnsiColor::Green)),
                duration(at.duration_since(call.since)),
                DIM,
            ),
            Outcome::Failed(at) => (
                ("✗", color(AnsiColor::Red)),
                format!("failed  {}", duration(at.duration_since(call.since))),
                color(AnsiColor::Red),
            ),
        };
        let mut out = self.call_header(call, glyph, &state, style);
        out.push_str(&self.reason(call));
        let failed = matches!(outcome, Outcome::Failed(_));
        let diffs = call
            .content
            .iter()
            .filter(|block| block["type"] == "diff")
            .count();
        for block in &call.content {
            match block["type"].as_str() {
                Some("diff") => out.push_str(&self.diff(block, diffs > 1)),
                Some("content") => {
                    if let Some(text) = block["content"]["text"].as_str() {
                        out.push_str(&self.output(text, failed));
                    }
                }
                _ => {}
            }
        }
        out
    }

    fn output(&self, text: &str, failed: bool) -> String {
        let text = anstream::adapter::strip_str(unfenced(text)).to_string();
        let width = NESTED + width_of(self.gutter());
        let lines = self.fit(text.trim_end(), width, self.detail == Detail::Full);
        let (gutter, style) = if failed {
            (color(AnsiColor::Red), Style::new())
        } else {
            (DIM, DIM)
        };
        self.guttered(self.abridged(lines), gutter, |_| style)
    }

    /// The changed lines of one file, under its path when the call changed several.
    fn diff(&self, block: &Value, titled: bool) -> String {
        let path = block["path"].as_str().unwrap_or("?");
        let old = block["oldText"].as_str().unwrap_or("");
        let new = block["newText"].as_str().unwrap_or("");
        let unified = TextDiff::from_lines(old, new)
            .unified_diff()
            .context_radius(1)
            .to_string();
        let body: Vec<String> = unified
            .lines()
            .skip_while(|line| line.starts_with("---") || line.starts_with("+++"))
            .map(|line| {
                if line.starts_with("@@") {
                    "⋯".to_owned()
                } else {
                    line.to_owned()
                }
            })
            .collect();
        let width = NESTED + width_of(self.gutter());
        let lines = self.fit(&body.join("\n"), width, false);
        let mut out = String::new();
        if titled {
            out.push_str(&format!(
                "{}{}\n",
                " ".repeat(NESTED),
                self.styled(path, BOLD)
            ));
        }
        out.push_str(&self.guttered(self.abridged(lines), DIM, |line| {
            if line.starts_with('+') {
                color(AnsiColor::Green)
            } else if line.starts_with('-') {
                color(AnsiColor::Red)
            } else {
                DIM
            }
        }));
        out
    }
}

enum Line {
    Text(String),
    Hidden(usize),
}

fn width_of(text: &str) -> usize {
    textwrap::core::display_width(text)
}

/// `text` cut to `room` columns, ending in `…` when anything was cut.
fn cut(text: &str, room: usize) -> String {
    if width_of(text) <= room {
        return text.to_owned();
    }
    let mut kept = String::new();
    for character in text.chars() {
        if width_of(&kept) + width_of(&character.to_string()) + 1 > room {
            break;
        }
        kept.push(character);
    }
    format!("{}…", kept.trim_end())
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("")
}

/// What a call does, in a line: the changed files for an edit, otherwise its title without the
/// tool's name in front.
fn summary(call: &ToolCall, name: &str) -> String {
    let paths: Vec<&str> = call
        .content
        .iter()
        .filter(|block| block["type"] == "diff")
        .filter_map(|block| block["path"].as_str())
        .collect();
    if !paths.is_empty() {
        return paths.join(", ");
    }
    let title = call.title.trim();
    let title = title
        .strip_prefix(name)
        .filter(|rest| rest.starts_with(' '))
        .map_or(title, str::trim_start);
    title.trim_matches('`').to_owned()
}

/// What ACP says a call of `kind` does, for agents that do not name their tools.
fn kind_name(kind: &str) -> &'static str {
    match kind {
        "read" => "Read",
        "edit" => "Edit",
        "delete" => "Delete",
        "move" => "Move",
        "search" => "Search",
        "execute" => "Run",
        "think" => "Think",
        "fetch" => "Fetch",
        "switch_mode" => "Mode",
        _ => "Tool",
    }
}

/// `+3 -1` over all the diffs of a call.
fn diff_counts(content: &[Value]) -> Option<String> {
    let mut diffs = content
        .iter()
        .filter(|block| block["type"] == "diff")
        .peekable();
    diffs.peek()?;
    let (mut added, mut removed) = (0, 0);
    for block in diffs {
        let old = block["oldText"].as_str().unwrap_or("");
        let new = block["newText"].as_str().unwrap_or("");
        for change in TextDiff::from_lines(old, new).iter_all_changes() {
            match change.tag() {
                similar::ChangeTag::Insert => added += 1,
                similar::ChangeTag::Delete => removed += 1,
                similar::ChangeTag::Equal => {}
            }
        }
    }
    Some(format!("+{added} -{removed}"))
}

/// Tool output without the markdown fence some agents wrap it in.
fn unfenced(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return text;
    };
    let Some((info, body)) = rest.split_once('\n') else {
        return text;
    };
    if info.contains('`') {
        return text;
    }
    body.strip_suffix("```").unwrap_or(body)
}

/// `0.4s`, `2m05s` or `1h03m`.
fn duration(elapsed: SignedDuration) -> String {
    let millis = elapsed.as_millis().max(0);
    let seconds = millis / 1000;
    if seconds < 60 {
        format!("{}.{}s", seconds, millis % 1000 / 100)
    } else if seconds < 3600 {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h{:02}m", seconds / 3600, seconds % 3600 / 60)
    }
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

    fn renderer(format: Format, detail: Detail, width: usize) -> Renderer {
        Renderer::new("coder", width, format, detail, TimeZone::UTC)
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

    fn render_all(renderer: &mut Renderer, entries: &[TranscriptEntry]) -> String {
        let mut out: String = entries
            .iter()
            .map(|entry| plain(&renderer.render(entry)))
            .collect();
        out.push_str(&plain(&renderer.finish()));
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
        .map(|text| renderer.chunk(at(0), Thinking::Message, text))
        .collect();
        assert!(output.contains("Hello"));
        assert!(!output.contains("**"));
        assert!(output.contains("let value = 1;"));
        assert!(
            renderer
                .prose
                .as_ref()
                .is_none_or(|prose| prose.text.is_empty())
        );
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
            entry(
                2,
                Direction::Wf,
                serde_json::json!({"decision": {"permission": "allow", "by": "the principal", "reason": null}}),
            ),
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
            output.contains("    │ second") && !output.contains("```"),
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
        assert!(renderer.caught_up().starts_with("  … Bash  sleep 9"));
        assert_eq!(renderer.caught_up(), "");
        let ask = entry(
            1,
            Direction::FromAgent,
            serde_json::json!({"id": 1, "method": "session/request_permission", "params": {"toolCall": {"toolCallId": "1"}}}),
        );
        renderer.render(&ask);
        assert!(
            renderer
                .caught_up()
                .trim_end()
                .ends_with("waiting for permission")
        );
    }

    #[test]
    fn unweighed_allows_and_harness_logs_show_only_with_full() {
        let entries = |by: &str| {
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
                entry(
                    0,
                    Direction::Wf,
                    serde_json::json!({"decision": {"permission": "allow", "by": by, "reason": null}}),
                ),
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
            &entries("the agent's permissions"),
        );
        assert!(
            !quiet.contains("allowed") && !quiet.contains("session/create"),
            "{quiet}"
        );
        assert!(quiet.contains("✓ Read  a.rs"), "{quiet}");
        let full = render_all(
            &mut renderer(Format::Raw, Detail::Full, 80),
            &entries("the agent's permissions"),
        );
        assert!(
            full.contains("allowed by the agent's permissions")
                && full.contains("log [session/create] x"),
            "{full}"
        );
        let rules = render_all(
            &mut renderer(Format::Raw, Detail::Abridged, 80),
            &entries("rules"),
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
        let output = renderer.chunk(at(0), Thinking::Message, "**kept**");
        let output = output + &renderer.finish();
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
            output.contains("| -before") && output.contains("| +after"),
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
                serde_json::json!({"id": "p", "method": "session/prompt", "params": {"prompt": [{"type": "text", "text": "# Do it\n\nmore\nlines"}]}}),
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
            "brief  00:00                     4 lines\n  Do it\n\ncoder  00:00\n  Done.\n\nfinished                           1m05s\n"
        );
    }
}

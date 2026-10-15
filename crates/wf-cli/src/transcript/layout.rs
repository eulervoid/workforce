//! How a transcript is drawn in a terminal: each block laid out for a fixed width, so long lines
//! never wrap by themselves, as text with escape codes for its styles.

use anstyle::Style;
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};
use serde_json::Value;
use similar::TextDiff;
use wf_harness::{Decider, Decision, Permission};

use super::{
    Asked, Block, Outcome, PlanEntry, PlanStatus, Prompter, StopReason, Thinking, ToolCall,
};
use crate::theme;

/// Lines kept from the start and the end of long tool output: the tail holds the errors.
const HEAD: usize = 5;
const TAIL: usize = 5;
const DEFAULT_WIDTH: usize = 100;
const MAX_WIDTH: usize = 120;
const MIN_WIDTH: usize = 40;
/// Prompts up to this many lines are shown whole; longer ones are cut to their first line.
const PROMPT_LINES: usize = 10;
/// Where everything inside a turn starts.
const MARGIN: usize = 2;
/// Where what belongs to a tool call starts, under its name.
const NESTED: usize = 4;
/// The columns left of output and diffs under a tool call, after `NESTED`.
const GUTTER: usize = 2;

/// The width to lay out for: the terminal's, within readable bounds.
pub fn terminal_width() -> usize {
    terminal_size::terminal_size()
        .map_or(DEFAULT_WIDTH, |(width, _)| usize::from(width.0))
        .clamp(MIN_WIDTH, MAX_WIDTH)
}

/// How a transcript, or the markdown of an item, is drawn.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Color, box characters and rendered markdown.
    Styled,
    /// The same layout as plain text: markdown as written, no color and no box characters.
    Raw,
}

/// How much of the transcript is shown.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Verbosity {
    /// Briefs, commands and output cut short, and the harness's own lines left out.
    Abridged,
    Full,
}

/// What a block is, for the blank lines around it: tool calls follow each other closely, and a
/// turn's header sits right on top of its first block.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Spacing {
    Nothing,
    Header,
    Prose,
    Tool,
    Log,
    Other,
}

impl Spacing {
    /// What goes between a block that left off as `self` and one that starts as `next`.
    pub fn before(self, next: Spacing) -> &'static str {
        let tight = matches!(self, Spacing::Nothing | Spacing::Header)
            || (self == next && matches!(next, Spacing::Tool | Spacing::Log));
        if tight { "" } else { "\n" }
    }
}

impl Block {
    /// How the block starts.
    pub fn opens(&self) -> Spacing {
        match self {
            Block::Prose { .. } => Spacing::Prose,
            Block::Call { .. } => Spacing::Tool,
            Block::Log(_) => Spacing::Log,
            Block::Prompt { .. }
            | Block::Answer { .. }
            | Block::Plan(_)
            | Block::Error { .. }
            | Block::Footer { .. } => Spacing::Other,
        }
    }

    /// How the block leaves off: the agent's header sits on top of what follows.
    pub fn leaves(&self) -> Spacing {
        match self {
            Block::Answer { .. } => Spacing::Header,
            Block::Prompt { .. }
            | Block::Prose { .. }
            | Block::Call { .. }
            | Block::Log(_)
            | Block::Plan(_)
            | Block::Error { .. }
            | Block::Footer { .. } => self.opens(),
        }
    }
}

#[derive(Clone, PartialEq)]
pub struct Layout {
    agent: String,
    width: usize,
    format: Format,
    verbosity: Verbosity,
    time_zone: TimeZone,
}

impl Layout {
    pub fn new(
        agent: &str,
        width: usize,
        format: Format,
        verbosity: Verbosity,
        time_zone: TimeZone,
    ) -> Self {
        Self {
            agent: agent.to_owned(),
            width,
            format,
            verbosity,
            time_zone,
        }
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn verbosity(&self) -> Verbosity {
        self.verbosity
    }

    /// The lines of `block`, each ending in a newline; empty when it shows nothing.
    pub fn block(&self, block: &Block) -> String {
        match block {
            Block::Prompt { by, at, text } => self.prompt(*by, *at, text),
            Block::Answer { at } => self.turn_header(&self.agent, theme::agent().bold(), *at, ""),
            Block::Prose { kind, text, .. } => self.prose(*kind, text),
            Block::Call { call, outcome } => self.finished_call(call, outcome),
            Block::Log(text) => self.log(text),
            Block::Plan(entries) => self.plan(entries),
            Block::Error { message, elapsed } => self.error(message, *elapsed),
            Block::Footer { reason, elapsed } => self.footer(reason, *elapsed),
        }
    }

    /// The line of a call that is still open: running, or waiting for its permission request to
    /// be answered.
    pub fn running(&self, call: &ToolCall) -> String {
        let right = match call.asked {
            Asked::Waiting => "waiting for permission",
            Asked::Not | Asked::Answered(_) => "running",
        };
        let mut out = self.call_header(
            call,
            ("…", theme::text_dimmed()),
            right,
            theme::text_dimmed(),
        );
        out.push_str(&self.reason(call));
        out
    }

    fn styled(&self, text: &str, style: Style) -> String {
        match self.format {
            Format::Raw => text.to_owned(),
            Format::Styled if text.is_empty() => String::new(),
            Format::Styled => format!("{style}{text}{style:#}"),
        }
    }

    /// What goes left of a line under a tool call. A line there drew the eye away from the code
    /// it framed, so the indent alone sets output and diffs apart from the agent's text, and a
    /// bar only marks the output of a call that failed.
    fn gutter(&self, gutter: Gutter) -> String {
        match (gutter, self.format) {
            (Gutter::Blank, _) => "  ".to_owned(),
            (Gutter::Failed, Format::Styled) => self.styled("│", theme::error()) + " ",
            (Gutter::Failed, Format::Raw) => "| ".to_owned(),
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
        if self.verbosity == Verbosity::Full || lines.len() <= HEAD + TAIL + 1 {
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
    fn guttered(&self, lines: Vec<Line>, gutter: Gutter, style: impl Fn(&str) -> Style) -> String {
        let left = format!("{}{}", " ".repeat(NESTED), self.gutter(gutter));
        let mut out = String::new();
        for line in lines {
            match line {
                Line::Text(text) if text.is_empty() => out.push_str(left.trim_end()),
                Line::Text(text) => {
                    out.push_str(&left);
                    out.push_str(&self.styled(&text, style(&text)));
                }
                Line::Hidden(count) => {
                    out.push_str(&left);
                    let text = format!("… {count} lines hidden");
                    out.push_str(&self.styled(&text, theme::text_dimmed()));
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
    fn turn_header(&self, speaker: &str, style: Style, at: Timestamp, right: &str) -> String {
        let time = self.time(at);
        let left = format!(
            "{}  {}",
            self.styled(speaker, style),
            self.styled(&time, theme::text_dimmed())
        );
        self.spread(
            &left,
            width_of(speaker) + 2 + width_of(&time),
            right,
            theme::text_dimmed(),
        )
    }

    /// A paragraph of the agent's text, rendered as markdown unless raw.
    pub fn prose(&self, kind: Thinking, text: &str) -> String {
        if text.trim().is_empty() {
            return String::new();
        }
        let margin = " ".repeat(MARGIN);
        let room = self.width - MARGIN;
        let rendered = match self.format {
            Format::Raw => self.fit(text.trim_end(), MARGIN, true).join("\n"),
            Format::Styled => crate::markdown::render(text, Some(room), ""),
        };
        let rendered = match (kind, self.format) {
            (Thinking::Thought, Format::Styled) => {
                let style = theme::text_dimmed().italic();
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

    /// The harness's own lines, only with `--full`.
    fn log(&self, text: &str) -> String {
        match self.verbosity {
            Verbosity::Abridged => String::new(),
            Verbosity::Full => {
                let mut out = String::new();
                for line in self.fit(text, MARGIN + 4, true) {
                    let label = self.styled("log", theme::text_dimmed());
                    out.push_str(&format!(
                        "{}{label} {}\n",
                        " ".repeat(MARGIN),
                        self.styled(&line, theme::text_dimmed())
                    ));
                }
                out
            }
        }
    }

    /// A prompt keeps its lines, cut to its first line when it is long, unless `--full`.
    fn prompt(&self, by: Prompter, at: Timestamp, text: &str) -> String {
        let speaker = match by {
            Prompter::Brief => "brief",
            Prompter::Wf => "wf",
        };
        let style = theme::daemon().bold();
        let lines = text.trim_end().lines().count();
        let mut out = String::new();
        match self.verbosity {
            Verbosity::Abridged if lines > PROMPT_LINES => {
                let count = format!("{lines} lines");
                out.push_str(&self.turn_header(speaker, style, at, &count));
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
            Verbosity::Abridged | Verbosity::Full => {
                out.push_str(&self.turn_header(speaker, style, at, ""));
                // Line by line as written: a prompt is wf's text, and markdown would join lines.
                let margin = " ".repeat(MARGIN);
                let room = self.width.saturating_sub(MARGIN).max(10);
                for line in text.trim_end().lines() {
                    let line = line.trim_end();
                    if line.is_empty() {
                        out.push('\n');
                        continue;
                    }
                    let hanging = " ".repeat(hanging_indent(line));
                    let options = textwrap::Options::new(room)
                        .subsequent_indent(&hanging)
                        .break_words(true);
                    for part in textwrap::wrap(line, options) {
                        out.push_str(&format!("{margin}{part}\n"));
                    }
                }
            }
        }
        out
    }

    fn error(&self, message: &str, elapsed: Option<SignedDuration>) -> String {
        let text = cut(first_line(message), self.width - MARGIN - 2);
        let left = format!(
            "{}{} {}",
            " ".repeat(MARGIN),
            self.styled("✗", theme::error()),
            self.styled(&text, theme::error())
        );
        let elapsed = elapsed.map(duration).unwrap_or_default();
        self.spread(
            &left,
            MARGIN + 2 + width_of(&text),
            &elapsed,
            theme::text_dimmed(),
        )
    }

    /// `finished` and how long the turn took.
    fn footer(&self, reason: &StopReason, elapsed: Option<SignedDuration>) -> String {
        let warn = theme::warning();
        let (text, style) = match reason {
            StopReason::EndTurn => ("finished", theme::text_dimmed()),
            StopReason::Cancelled => ("stopped", warn),
            StopReason::MaxTokens => ("stopped: out of tokens", warn),
            StopReason::MaxTurnRequests => ("stopped: too many requests", warn),
            StopReason::Refusal => ("refused", warn),
            StopReason::Other(other) => (other.as_str(), warn),
        };
        let elapsed = elapsed.map(duration).unwrap_or_default();
        self.spread(
            &self.styled(text, style),
            width_of(text),
            &elapsed,
            theme::text_dimmed(),
        )
    }

    fn plan(&self, entries: &[PlanEntry]) -> String {
        let mut out = format!(
            "{}{}\n",
            " ".repeat(MARGIN),
            self.styled("plan", theme::text_bold())
        );
        for entry in entries {
            let marker = match entry.status {
                PlanStatus::Completed => "✓",
                PlanStatus::InProgress => "◐",
                PlanStatus::Pending => "○",
            };
            let line = cut(first_line(&entry.content), self.width - NESTED - 2);
            out.push_str(&format!("{}{marker} {line}\n", " ".repeat(NESTED)));
        }
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
        // A verdict with a reason goes on a line of its own, under the call.
        let verdict = self
            .verdict(&call.asked)
            .filter(|_| !has_reason(&call.asked));
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
        let mut lines = match self.verbosity {
            Verbosity::Abridged => {
                let first = match summary.lines().nth(1) {
                    Some(_) => format!("{} …", first_line(&summary)),
                    None => summary.clone(),
                };
                vec![cut(&first, room)]
            }
            Verbosity::Full => self.wrap_under(&summary, indent, reserved),
        }
        .into_iter();
        let first = lines.next().unwrap_or_default();
        let mut left = format!(
            "{}{} {}  {first}",
            " ".repeat(MARGIN),
            self.styled(glyph, glyph_style),
            self.styled(name, theme::text_bold())
        );
        let mut left_width = indent + width_of(&first);
        if !suffix.is_empty() {
            let suffix_style = match &verdict {
                Some((_, Permission::Deny)) => theme::error(),
                Some((_, Permission::Allow)) | None => theme::text_dimmed(),
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
        let unweighed = decision.by == Decider::FixedPermissions
            && decision.permission == Permission::Allow
            && decision.reason.is_none();
        if unweighed && self.verbosity == Verbosity::Abridged {
            return None;
        }
        let by = match &decision.by {
            Decider::Principal => "you".to_owned(),
            by => by.to_string(),
        };
        let verb = match decision.permission {
            Permission::Allow => "allowed",
            Permission::Deny => "denied",
        };
        Some((format!("{verb} by {by}"), decision.permission))
    }

    /// `denied by hook sh: no edits today`: who decided the permission request of `call`, and
    /// why, when the decider said why.
    fn reason(&self, call: &ToolCall) -> String {
        let Asked::Answered(Decision {
            reason: Some(reason),
            ..
        }) = &call.asked
        else {
            return String::new();
        };
        let Some((verdict, permission)) = self.verdict(&call.asked) else {
            return String::new();
        };
        let style = match permission {
            Permission::Allow => theme::text_dimmed(),
            Permission::Deny => theme::error(),
        };
        self.fit(&format!("{verdict}: {reason}"), NESTED, true)
            .iter()
            .map(|line| format!("{}{}\n", " ".repeat(NESTED), self.styled(line, style)))
            .collect()
    }

    fn finished_call(&self, call: &ToolCall, outcome: &Outcome) -> String {
        let (glyph, state, style) = match outcome {
            Outcome::Completed(at) => (
                ("✓", theme::success()),
                duration(at.duration_since(call.since)),
                theme::text_dimmed(),
            ),
            Outcome::Failed(at) => (
                ("✗", theme::error()),
                format!("failed  {}", duration(at.duration_since(call.since))),
                theme::error(),
            ),
            Outcome::Denied => (("✗", theme::error()), String::new(), theme::text_dimmed()),
            Outcome::Unfinished => (
                ("·", theme::text_dimmed()),
                "unfinished".to_owned(),
                theme::text_dimmed(),
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
        let lines = self.fit(
            text.trim_end(),
            NESTED + GUTTER,
            self.verbosity == Verbosity::Full,
        );
        let (gutter, style) = if failed {
            (Gutter::Failed, Style::new())
        } else {
            (Gutter::Blank, theme::text_dimmed())
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
        let lines = self.fit(&body.join("\n"), NESTED + GUTTER, false);
        let mut out = String::new();
        if titled {
            out.push_str(&format!(
                "{}{}\n",
                " ".repeat(NESTED),
                self.styled(path, theme::text_bold())
            ));
        }
        out.push_str(&self.guttered(self.abridged(lines), Gutter::Blank, |line| {
            if line.starts_with('+') {
                theme::diff_added()
            } else if line.starts_with('-') {
                theme::diff_removed()
            } else {
                theme::text_dimmed()
            }
        }));
        out
    }
}

/// `Bash  cargo test`: what a call does, in a line, for a list of sessions.
pub fn call_line(call: &ToolCall) -> String {
    let name = if call.name.is_empty() {
        "Tool"
    } else {
        &call.name
    };
    format!("{name}  {}", first_line(&summary(call, name)))
}

#[derive(Clone, Copy)]
enum Gutter {
    Blank,
    Failed,
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

fn has_reason(asked: &Asked) -> bool {
    matches!(
        asked,
        Asked::Answered(Decision {
            reason: Some(_),
            ..
        })
    )
}

/// Where the wrapped lines of `line` start: under its text, past its indent and list marker.
fn hanging_indent(line: &str) -> usize {
    let text = line.trim_start();
    let indent = line.len() - text.len();
    let marker = ["- ", "* "]
        .iter()
        .find(|marker| text.starts_with(**marker))
        .map_or(0, |marker| marker.len());
    indent + marker
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
pub(super) fn kind_name(kind: &str) -> &'static str {
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

pub(super) fn paragraph_boundary(text: &str) -> Option<usize> {
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

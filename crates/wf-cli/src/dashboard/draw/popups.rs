//! What is drawn over the body: the choice, the keys and the prompts.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use tui_input::Input;
use wf_core::id::HashId;

use super::chrome::SEPARATOR;
use super::{KeysAt, Themed, accent, clear, highlight, themed};
use crate::dashboard::app::{App, Choice, Prompt, Starter, Written};
use crate::dashboard::hint::HELP;
use crate::render;
use crate::theme;

/// A list to pick from, at the bottom of the body, with the option picked in `accent`.
pub(super) fn draw_choice(
    frame: &mut Frame,
    choice: &Choice,
    accent: Color,
    body: Rect,
    base: Style,
) {
    let (title, options): (String, Vec<String>) = match choice {
        Choice::Start { item, starters, .. } => (
            format!(" start #{} ", item.short()),
            starters
                .iter()
                .map(|starter| match starter {
                    Starter::Dispatcher => "release: open it for the dispatcher".to_string(),
                    Starter::Agent(agent) => agent.to_string(),
                })
                .collect(),
        ),
        Choice::State { item, states, .. } => (
            format!(" move #{} to ", item.short()),
            states
                .iter()
                .map(|state| format!("{} {state}", render::state_icon(*state)))
                .collect(),
        ),
    };
    let lines: Vec<Line> = options
        .into_iter()
        .enumerate()
        .map(|(index, option)| {
            if index == choice.index() {
                Line::from(highlight(
                    vec![Span::raw(option)],
                    Style::from(theme::text_bold()),
                    accent,
                ))
            } else {
                Line::from(format!(" {option}"))
            }
        })
        .collect();
    let height = (lines.len() as u16 + 2).min(body.height);
    let width = lines
        .iter()
        .map(Line::width)
        .chain([title.chars().count()])
        .max()
        .unwrap_or_default() as u16
        + 4;
    let area = Rect {
        x: body.x + 1,
        y: body.y + body.height - height,
        width: width.min(body.width.saturating_sub(1)),
        height,
    };
    clear(frame, area, base);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_style(Style::from(theme::divider()))
                .title(title),
        ),
        area,
    );
}

/// Draws the keys, under their title in `accent`.
pub(super) fn draw_keys(frame: &mut Frame, accent: Color, body: Rect, base: Style) {
    let lines: Vec<Line> = HELP
        .iter()
        .map(|hint| {
            Line::from(vec![
                themed(format!(" {:<12}", hint.label), theme::key()),
                Span::raw(hint.what),
            ])
        })
        .collect();
    let height = (lines.len() as u16 + 2).min(body.height);
    // The borders, and a space between the longest line and the right one.
    let width = lines.iter().map(Line::width).max().unwrap_or_default() as u16 + 3;
    let area = Rect {
        x: body.x + 1,
        y: body.y,
        width: width.min(body.width.saturating_sub(2)),
        height,
    };
    // The title is drawn over the border, and without this would take on its dim.
    let undimmed = Style::new().remove_modifier(Modifier::DIM);
    let mut title = vec![themed("─", theme::divider())];
    title.extend(
        highlight(
            vec![Span::raw("keys")],
            Style::from(theme::text_bold()),
            accent,
        )
        .into_iter()
        .map(|span| {
            let style = undimmed.patch(span.style);
            span.style(style)
        }),
    );
    clear(frame, area, base);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_style(Style::from(theme::divider()))
                .title(Line::from(title)),
        ),
        area,
    );
}

/// The line the footer asks for, with the cursor in it.
pub(super) fn draw_prompt(frame: &mut Frame, app: &App, prompt: &Prompt, area: Rect) {
    // The label of what is asked has the accent; a question answered with keys has it on the
    // whole line.
    let accent = accent(app, KeysAt::Footer);
    let question = |text: String| {
        Line::from(Span::styled(
            text,
            Style::from(theme::text_bold()).bg(accent),
        ))
    };
    let (label, input, hint) = match prompt {
        Prompt::Filter { input, .. } => ("/".to_owned(), input, ""),
        Prompt::Comment { item, input } => (
            format!("comment on #{}:", item.short()),
            input,
            "enter post · ctrl+g editor · esc cancel ",
        ),
        Prompt::LineComment {
            kind,
            lines,
            then,
            input,
            ..
        } => (
            format!("{} on {}:", kind.label(), lines.location()),
            input,
            match then {
                Written::Post => "enter post · ctrl+g editor · esc cancel ",
                Written::Draft { .. } => "enter keep for the review · ctrl+g editor · esc cancel ",
            },
        ),
        Prompt::Stop { session } => {
            let agent = app
                .snapshot
                .working
                .iter()
                .find(|working| working.id == *session)
                .map_or_else(String::new, |working| {
                    format!("{} on #{} ", working.agent_name, working.item.short())
                });
            let mut line = question(format!(" stop {agent}now? "));
            line.extend(answer_keys(&["y/n"]));
            draw_question(frame, app, line, area);
            return;
        }
        Prompt::Choose(_) | Prompt::Keys => return,
        Prompt::Answer { blocker, input, .. } => (
            format!("⧗ {} answer:", blocker.short()),
            input,
            "enter resolve · ctrl+g editor · esc cancel ",
        ),
        Prompt::Verdict { item } => {
            let drafts = match app.drafts(*item).len() {
                0 => String::new(),
                1 => " with 1 comment on lines".to_owned(),
                count => format!(" with {count} comments on lines"),
            };
            let mut line = question(format!(" ◎ #{}{drafts}: ", item.short()));
            line.extend(answer_keys(&["a approve", "r changes", "D diff", "esc"]));
            draw_question(frame, app, line, area);
            return;
        }
    };
    // The label between its ends, and a space before what is typed.
    let mut label = highlight(
        vec![Span::raw(label)],
        Style::from(theme::text_bold()),
        accent,
    );
    label.push(Span::raw(" "));
    let label_width: usize = label.iter().map(Span::width).sum();
    // The hint keeps to the right while it leaves room to type; a narrow footer leaves it out.
    let hint_width = textwrap::core::display_width(hint) as u16;
    let area = if area.width >= label_width as u16 + hint_width + 16 {
        let [typed, hint_area] =
            Layout::horizontal([Constraint::Fill(1), Constraint::Length(hint_width)]).areas(area);
        frame.render_widget(
            Paragraph::new(Line::from(hint).themed(theme::text_dimmed())),
            hint_area,
        );
        typed
    } else {
        area
    };
    let width = usize::from(area.width).saturating_sub(label_width + 1);
    let (typed, cursor) = visible_input(input, width);
    label.push(Span::raw(typed));
    let line = Line::from(label);
    frame.render_widget(Paragraph::new(line), area);
    frame.set_cursor_position((area.x + label_width as u16 + cursor, area.y));
}

/// What of the typed text shows in `width` cells, and the cell the cursor is in. `tui-input`
/// scrolls in terminal cells, so the text is skipped by width, and the cursor moves by the cells
/// actually skipped.
pub(super) fn visible_input(input: &Input, width: usize) -> (String, u16) {
    let scroll = input.visual_scroll(width);
    let mut skipped = 0;
    let mut characters = input.value().chars();
    let mut rest = characters.clone();
    while skipped < scroll {
        let Some(character) = characters.next() else {
            break;
        };
        skipped += textwrap::core::display_width(character.encode_utf8(&mut [0; 4]));
        rest = characters.clone();
    }
    (
        rest.collect(),
        input.visual_cursor().saturating_sub(skipped) as u16,
    )
}

/// Draws a question answered with keys: the accent covers the whole line, so the keys read as
/// part of it.
pub(super) fn draw_question(frame: &mut Frame, app: &App, line: Line, area: Rect) {
    let style = Style::new().bg(accent(app, KeysAt::Footer));
    frame.render_widget(Paragraph::new(line).style(style), area);
}

/// The `keys` that answer a question, each a key and what it does: the key in bold, the words in
/// normal weight.
pub(super) fn answer_keys(keys: &[&'static str]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (index, hint) in keys.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(SEPARATOR));
        }
        let (key, what) = hint.split_once(' ').unwrap_or((hint, ""));
        spans.push(themed(key, theme::on_accent().bold()));
        if !what.is_empty() {
            spans.push(Span::raw(format!(" {what}")));
        }
    }
    spans
}

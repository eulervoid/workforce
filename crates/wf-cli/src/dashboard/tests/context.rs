//! Dashboard tests: context.

use super::support::*;
use crate::theme;
use jiff::tz::TimeZone;
use ratatui::crossterm::event::KeyCode;
use ratatui::style::Style;

#[test]
fn session_rows_show_how_full_their_context_is() {
    let mut app = app();
    with_usage(&mut app);
    let text = screen(&mut app, 60, 22);
    insta::assert_snapshot!("now_context", text);
    // From 80% of the window on, the context is yellow. The backend quotes each line, which the
    // column leaves out.
    let (y, line) = text
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains("170k/200k"))
        .unwrap();
    let x = line[1..line.find("170k").unwrap()].chars().count();
    let (x, y) = (u16::try_from(x).unwrap(), u16::try_from(y).unwrap());
    assert_eq!(
        Drawn::of(&mut app, 60, 22).style_at(x, y).fg,
        Style::from(theme::attention()).fg
    );
    press(&mut app, KeyCode::Char('3'));
    app.refresh(with_agents());
    insta::assert_snapshot!("agents_context", screen(&mut app, 60, 16));
}

#[test]
fn a_session_detail_shows_its_context_above_the_transcript() {
    let mut app = app();
    with_usage(&mut app);
    let coder = app.snapshot.working[0].clone();
    press(&mut app, KeyCode::Enter);
    let mut snapshot = snapshot();
    snapshot.in_view = Some(session_in_view(coder));
    app.refresh(snapshot);
    insta::assert_snapshot!("detail_context", screen(&mut app, 60, 16));
}

#[test]
fn a_nearly_full_context_is_yellow_in_the_detail() {
    let mut app = app();
    app.resize(130, 24);
    with_usage(&mut app);
    for _ in 0..4 {
        press(&mut app, KeyCode::Char('j'));
    }
    let mut snapshot = snapshot();
    snapshot.in_view = Some(session_in_view(snapshot.working[1].clone()));
    app.refresh(snapshot);
    insta::assert_snapshot!("detail_context_full", screen(&mut app, 130, 14));
    let line = screen(&mut app, 130, 14)
        .lines()
        .position(|line| line.contains("170k of 200k (85%)"))
        .unwrap();
    let x = app.list_width() + 3;
    let y = u16::try_from(line).unwrap();
    assert_eq!(
        Drawn::of(&mut app, 130, 14).style_at(x, y).fg,
        Style::from(theme::attention()).fg
    );
}

#[test]
fn a_session_detail_names_its_model_before_any_usage() {
    let mut app = app();
    let coder = app.snapshot.working[0].clone();
    let mut transcript = vec![session_on_opus()];
    transcript.extend(coder_transcript());
    app.transcribed(coder.id, &transcript);
    press(&mut app, KeyCode::Enter);
    let mut snapshot = snapshot();
    snapshot.in_view = Some(session_in_view(coder));
    app.refresh(snapshot);
    insta::assert_snapshot!("detail_model", screen(&mut app, 60, 16));
}

#[test]
fn a_list_moves_only_as_far_as_keeps_the_selection_on_screen() {
    use crate::dashboard::draw::list_top;
    // Within the lines on screen, the list stays.
    assert_eq!(list_top(5, Some(10), 40, 10), 5);
    // Past its end, it moves to keep the selection and the line after it on screen.
    assert_eq!(list_top(5, Some(14), 40, 10), 6);
    // Above its top, it moves to keep the line before the selection on screen.
    assert_eq!(list_top(5, Some(5), 40, 10), 4);
    assert_eq!(list_top(5, Some(0), 40, 10), 0);
    // A list that got shorter is drawn to its end.
    assert_eq!(list_top(30, None, 12, 10), 2);
}

/// `transcript` laid out at once into one text, as the session detail did before it kept the
/// lines of each block.
fn laid_out_at_once(
    layout: &crate::transcript::Layout,
    transcript: &crate::transcript::Transcript,
) -> Vec<ratatui::text::Line<'static>> {
    use crate::transcript::Spacing;
    use ansi_to_tui::IntoText;
    let mut out = String::new();
    let mut last = Spacing::Nothing;
    let mut add = |opens: Spacing, leaves: Spacing, text: String| {
        if !text.is_empty() {
            out.push_str(last.before(opens));
            out.push_str(&text);
            last = leaves;
        }
    };
    for block in transcript.blocks() {
        add(block.opens(), block.leaves(), layout.block(block));
    }
    if let Some(prose) = transcript.open_prose() {
        add(
            Spacing::Prose,
            Spacing::Prose,
            layout.prose(prose.kind, &prose.text),
        );
    }
    for call in transcript.open_calls() {
        add(Spacing::Tool, Spacing::Tool, layout.running(call));
    }
    out.into_text().unwrap().lines
}

#[test]
fn the_kept_lines_of_a_transcript_are_those_of_laying_it_out_at_once() {
    use crate::dashboard::transcript_lines::TranscriptLines;
    use crate::transcript::{Format, Layout, Transcript, Verbosity};
    let layout = |width| {
        Layout::new(
            "coder",
            width,
            Format::Styled,
            Verbosity::Abridged,
            TimeZone::UTC,
        )
    };
    let mut transcript = Transcript::new();
    let mut lines = TranscriptLines::new(layout(60));
    // Parsed as one text, the styles a block ends with are reset in the next one, rather than
    // left out: they differ as spans, so they are compared as drawn.
    let drawn = |lines: Vec<ratatui::text::Line<'static>>, width: usize| {
        let area = ratatui::layout::Rect::new(0, 0, width as u16, lines.len() as u16);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        ratatui::widgets::Widget::render(
            ratatui::widgets::Paragraph::new(lines),
            area,
            &mut buffer,
        );
        buffer
    };
    let assert_same = |lines: &TranscriptLines, transcript: &Transcript, width| {
        let mut kept = lines.lines().to_vec();
        kept.extend(lines.open_lines(transcript));
        let at_once = laid_out_at_once(&layout(width), transcript);
        assert_eq!(kept.len(), at_once.len());
        assert_eq!(drawn(kept, width), drawn(at_once, width));
    };
    let mut entries = coder_transcript();
    entries.extend(coder_transcript_continued());
    // Taken in a few entries at a time, with a call or some prose open in between.
    for step in entries.chunks(3) {
        for entry in step {
            transcript.push(entry);
        }
        lines.update(layout(60), &transcript);
        assert_same(&lines, &transcript, 60);
    }
    assert!(lines.lines().len() > 20, "{}", lines.lines().len());
    // A narrower window lays it all out again.
    lines.update(layout(30), &transcript);
    assert_same(&lines, &transcript, 30);
}

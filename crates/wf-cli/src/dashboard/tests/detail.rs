//! Dashboard tests: detail.

use super::support::*;
use crate::dashboard::app::{App, NowRow};
use ratatui::crossterm::event::KeyCode;
use wf_core::id::{ItemId, SessionId};

#[test]
fn a_long_item_shows_how_far_down_it_is_scrolled_and_keeps_off_the_right_edge() {
    let mut app = app();
    let spans = ItemId::from_bytes([2; 32]);
    app.open_item(spans);
    let mut snapshot = snapshot();
    // A word too long for the pane, then twelve short lines.
    let mut text = format!("{}\n", "x".repeat(200));
    text.extend((1..=12).map(|line| format!("line {line}\n")));
    snapshot.in_view = Some(item_in_view(spans, text, None));
    app.refresh(snapshot);
    // The body is on rows 4 to 6, its text 58 columns wide after the margin at the left: 16
    // lines, scrolled from 0 to 13.
    let body = |app: &mut App| -> Vec<String> { Drawn::of(app, 60, 10).lines()[4..=6].to_vec() };
    let at_top = body(&mut app);
    assert!(at_top[2].ends_with(" 0% "), "{at_top:#?}");
    // The long word is wrapped a column short of the edge.
    assert!(at_top[0].starts_with(&format!(" {} ", "x".repeat(58))));
    for _ in 0..6 {
        press(&mut app, KeyCode::Char('j'));
        screen(&mut app, 60, 10);
    }
    let halfway = body(&mut app);
    assert!(halfway[2].ends_with(" 46% "), "{halfway:#?}");
    for _ in 0..20 {
        press(&mut app, KeyCode::Char('j'));
        screen(&mut app, 60, 10);
    }
    let at_end = body(&mut app);
    assert!(at_end[2].ends_with(" 100% "), "{at_end:#?}");
    assert_eq!(at_end[0].trim(), "line 10");
    // No text in the last column, at any scroll.
    for line in at_top.iter().chain(&halfway).chain(&at_end) {
        assert!(line.ends_with(' '), "{line:?}");
    }
}

#[test]
fn a_line_as_wide_as_the_laid_out_item_stays_one_line() {
    let mut app = app();
    let spans = ItemId::from_bytes([2; 32]);
    app.open_item(spans);
    screen(&mut app, 60, 10);
    // Words filling exactly the width the item is laid out for.
    let width = usize::from(app.detail_width());
    let mut full = "word ".repeat(width / 5);
    full.truncate(full.len() - 1);
    full.push_str(&"x".repeat(width - full.len()));
    let mut snapshot = snapshot();
    snapshot.in_view = Some(item_in_view(spans, format!("{full}\nnext\n"), None));
    app.refresh(snapshot);
    let body = Drawn::of(&mut app, 60, 10).lines()[4..=6].to_vec();
    assert_eq!(body[0].trim(), full, "{body:#?}");
    assert_eq!(body[1].trim(), "next", "{body:#?}");
}

#[test]
fn rendered_markdown_is_laid_out_at_the_width_it_was_rendered_for() {
    let mut app = app();
    let spans = ItemId::from_bytes([2; 32]);
    app.open_item(spans);
    screen(&mut app, 60, 10);
    // Bold words fill each line up to the width; their escape codes take no columns.
    let words = "**word** ".repeat(usize::from(app.detail_width()) / 5 * 2);
    let text = crate::markdown::render(&words, Some(usize::from(app.detail_width())), "  ");
    let rendered: Vec<String> = text
        .lines()
        .map(|line| anstream::adapter::strip_str(line).to_string())
        .collect();
    let mut snapshot = snapshot();
    snapshot.in_view = Some(item_in_view(spans, text, None));
    app.refresh(snapshot);
    let body = Drawn::of(&mut app, 60, 10).lines()[4..=5].to_vec();
    assert_eq!(body[0].trim(), rendered[0].trim(), "{body:#?}");
    assert_eq!(body[1].trim(), rendered[1].trim(), "{body:#?}");
}

#[test]
fn an_item_that_fits_shows_no_position() {
    let mut app = app();
    let spans = ItemId::from_bytes([2; 32]);
    app.open_item(spans);
    let mut snapshot = snapshot();
    snapshot.in_view = Some(item_in_view(spans, "line 1\nline 2\n".to_owned(), None));
    app.refresh(snapshot);
    let body = Drawn::of(&mut app, 60, 10).lines()[4..=6].join("\n");
    for position in ["Top", "Bot", "All", "%"] {
        assert!(!body.contains(position), "{body}");
    }
}

#[test]
fn a_long_list_counts_the_entries_out_of_view() {
    let mut app = app();
    // Everything fits: no indicator.
    let all = screen(&mut app, 60, 24);
    assert!(!all.contains('↑') && !all.contains('↓'), "{all}");
    // Below the review: the three sessions and the flag, the reviewer's two lines counting once.
    let top = Drawn::of(&mut app, 60, 12).lines();
    assert!(top[8].ends_with(" ↓ 4 "), "{top:#?}");
    assert!(!top.join("").contains('↑'));
    // At the end, above the reviewer: the three waiting and the other two sessions, the
    // blocker's two lines counting once.
    for _ in 0..6 {
        press(&mut app, KeyCode::Char('j'));
    }
    let end = Drawn::of(&mut app, 60, 12).lines();
    assert!(end[3].contains("Archive by category"));
    assert!(end[3].ends_with(" ↑ 5 "), "{end:#?}");
    assert!(!end.join("").contains('↓'));
    // A click on the indicator is one on its row.
    app.click(57, 3);
    let reviewer = SessionId::from_bytes([13; 32]);
    assert_eq!(app.now.selection.get(), Some(&NowRow::Session(reviewer)));
}

#[test]
fn an_indicator_on_the_selection_has_its_background() {
    let mut app = app();
    press(&mut app, KeyCode::Char('j'));
    // Three rows of the list: the request, then both lines of the selected blocker.
    let lines = Drawn::of(&mut app, 60, 9).lines();
    assert!(lines[5].ends_with(" ↓ 5 "), "{lines:#?}");
    assert_eq!(Drawn::of(&mut app, 60, 9).bg_at(57, 5), Some(accent()));
}

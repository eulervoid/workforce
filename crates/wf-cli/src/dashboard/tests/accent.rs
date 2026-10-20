//! Dashboard tests: accent.

use super::support::*;
use crate::dashboard::app::{App, Prompt, Tab};
use crate::dashboard::snapshot::Detail;
use crate::theme;
use ratatui::crossterm::event::KeyCode;
use ratatui::style::Style;
use wf_core::id::SessionId;

#[test]
fn the_part_with_the_keys_has_the_accent_and_the_other_the_dimmed_accent() {
    let mut app = app();
    app.resize(130, 24);
    let blocked = SessionId::from_bytes([12; 32]);
    for _ in 0..4 {
        press(&mut app, KeyCode::Char('j'));
    }
    let mut snapshot = snapshot();
    snapshot.in_view = Some(session_in_view(snapshot.working[1].clone()));
    app.refresh(snapshot.clone());
    // The parked coder is selected on line 9; the breadcrumb is on line 3, right of the list.
    let row = 9;
    let (crumb_x, crumb_y) = find(&mut app, 130, 14, "coder · work");
    assert_eq!(app.wanted().detail, Some(Detail::Session(blocked)));
    assert_eq!(Drawn::of(&mut app, 130, 14).bg_at(30, row), Some(accent()));
    assert_eq!(
        Drawn::of(&mut app, 130, 14).bg_at(crumb_x, crumb_y),
        Some(accent_dimmed())
    );
    // In the detail, the list keeps the selection in the dimmed accent, and the breadcrumb has
    // the accent.
    press(&mut app, KeyCode::Char('l'));
    app.refresh(snapshot);
    assert_eq!(
        Drawn::of(&mut app, 130, 14).bg_at(30, row),
        Some(accent_dimmed())
    );
    assert_eq!(
        Drawn::of(&mut app, 130, 14).bg_at(crumb_x, crumb_y),
        Some(accent())
    );
}

#[test]
fn the_bar_and_the_background_cover_every_line_of_the_selection() {
    let mut app = app();
    press(&mut app, KeyCode::Char('j'));
    let drawn = Drawn::of(&mut app, 60, 22);
    let (_, row) = drawn.find("blocker from");
    for line in [row, row + 1] {
        assert_eq!(drawn.symbol_at(0, line), "▎");
        // The bar is white, on the background of the row, with no gap to the left edge.
        assert_eq!(
            drawn.style_at(0, line).fg,
            Style::from(theme::on_accent()).fg
        );
        assert_eq!(drawn.bg_at(0, line), Some(accent()));
        assert_eq!(drawn.bg_at(10, line), Some(accent()));
    }
    // The line after the entry is the next one's.
    assert_eq!(drawn.symbol_at(0, row + 2), " ");
    assert_ne!(drawn.bg_at(10, row + 2), Some(accent()));
}

#[test]
fn the_accent_goes_to_the_breadcrumb_on_enter_and_back_on_esc() {
    let mut app = app();
    let coder = app.snapshot.working[0].clone();
    let (_, row) = find(&mut app, 60, 16, "wants:");
    assert_eq!(Drawn::of(&mut app, 60, 16).bg_at(10, row), Some(accent()));
    press(&mut app, KeyCode::Enter);
    let mut snapshot = snapshot();
    snapshot.in_view = Some(session_in_view(coder));
    app.refresh(snapshot);
    let (column, crumb_y) = find(&mut app, 60, 16, "coder · work");
    assert_eq!(
        Drawn::of(&mut app, 60, 16).bg_at(column, crumb_y),
        Some(accent())
    );
    press(&mut app, KeyCode::Esc);
    assert_eq!(Drawn::of(&mut app, 60, 16).bg_at(10, row), Some(accent()));
}

#[test]
fn the_last_part_of_the_breadcrumb_has_its_ends_in_its_accent() {
    let mut app = app();
    app.resize(130, 24);
    for _ in 0..4 {
        press(&mut app, KeyCode::Char('j'));
    }
    let mut snapshot = snapshot();
    snapshot.in_view = Some(session_in_view(snapshot.working[1].clone()));
    app.refresh(snapshot.clone());
    let crumb = "coder · work · 40m ◇ parked";
    // The list has the keys: the ends are dimmed with the rest of the highlight.
    assert_ends(&mut app, 130, 14, crumb, accent_dimmed());
    press(&mut app, KeyCode::Char('l'));
    app.refresh(snapshot);
    assert_ends(&mut app, 130, 14, crumb, accent());
}

#[test]
fn a_breadcrumb_cut_short_leaves_out_the_end_of_its_highlight() {
    let mut app = app();
    let coder = app.snapshot.working[0].clone();
    press(&mut app, KeyCode::Enter);
    let mut snapshot = snapshot();
    snapshot.in_view = Some(session_in_view(coder));
    app.refresh(snapshot);
    // Wide enough for the text and its end, for the text only, and for neither.
    let (column, row) = find(&mut app, 60, 16, "◆▌");
    let fits = column + 1;
    for (width, last) in [(fits + 1, "▌"), (fits, "◆"), (fits - 5, "…")] {
        assert_eq!(
            Drawn::of(&mut app, width, 16).symbol_at(width - 1, row),
            last
        );
        assert_eq!(
            Drawn::of(&mut app, width, 16).bg_at(width - 1, row) == Some(accent()),
            last != "▌"
        );
    }
}

#[test]
fn the_active_tab_has_its_ends_in_the_colour_of_its_highlight() {
    let mut app = app();
    let behind = tab_active().bg.unwrap();
    assert_ends(&mut app, 60, 16, "1 Now", behind);
    let (column, row) = find(&mut app, 60, 16, "1 Now");
    // The others have spaces in their place, and are clicked there too.
    assert_eq!(Drawn::of(&mut app, 60, 16).symbol_at(column + 9, row), " ");
    app.click(column + 9, row);
    assert_eq!(app.tab, Tab::Items);
    assert_ends(&mut app, 60, 16, "2 Items", behind);
    // The log looks like the others.
    press(&mut app, KeyCode::Char('4'));
    assert_ends(&mut app, 60, 16, "4 Log", behind);
    // Its ends are part of it.
    let (column, row) = find(&mut app, 60, 16, "▐4 Log▌");
    for end in [column, column + 6] {
        press(&mut app, KeyCode::Char('1'));
        app.click(end, row);
        assert_eq!(app.tab, Tab::Log);
    }
}

#[test]
fn while_the_terminal_pane_is_not_focused_only_the_background_of_the_active_tab_dims() {
    let mut app = app();
    press(&mut app, KeyCode::Char('4'));
    let (column, row) = find(&mut app, 60, 16, "▐4 Log▌");
    let label = |app: &mut App| Drawn::of(app, 60, 16).style_at(column + 1, row);
    let ends =
        |app: &mut App| [column, column + 6].map(|x| Drawn::of(app, 60, 16).style_at(x, row).fg);
    let focused = label(&mut app);
    assert_eq!(focused.bg, tab_active().bg);
    assert_eq!(ends(&mut app), [tab_active().bg; 2]);
    app.terminal_focus(false);
    let unfocused = label(&mut app);
    assert_eq!(unfocused.bg, tab_active_dimmed().bg);
    assert_ne!(unfocused.bg, focused.bg);
    // The label and its ends take the dimmer background; the text stays as it was.
    assert_eq!(ends(&mut app), [tab_active_dimmed().bg; 2]);
    assert_eq!(
        Style {
            bg: None,
            ..unfocused
        },
        Style {
            bg: None,
            ..focused
        }
    );
}

#[test]
fn a_prompt_on_the_log_tab_has_the_accent() {
    let mut app = app();
    press(&mut app, KeyCode::Char('4'));
    press(&mut app, KeyCode::Char('/'));
    assert_eq!(Drawn::of(&mut app, 60, 16).bg_at(1, 15), Some(accent()));
}

#[test]
fn the_review_prompt_has_the_accent_on_the_whole_line() {
    let mut app = app();
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('v'));
    assert!(matches!(app.prompt, Some(Prompt::Verdict { .. })));
    let drawn = Drawn::of(&mut app, 60, 24);
    let (column, row) = drawn.find("r changes");
    assert_eq!(row, 23);
    // The key and its words, and the last column of the line.
    let key = drawn.style_at(column, row);
    assert_eq!(key.bg, Some(accent()));
    assert!(key.add_modifier.contains(ratatui::style::Modifier::BOLD));
    assert_eq!(drawn.bg_at(column + 3, row), Some(accent()));
    assert_eq!(drawn.bg_at(59, row), Some(accent()));
}

#[test]
fn while_the_terminal_pane_is_not_focused_the_accent_is_dimmed() {
    let mut app = app();
    let (_, row) = find(&mut app, 60, 16, "wants:");
    assert!(app.terminal_focus(false));
    assert!(!app.terminal_focus(false));
    assert_eq!(
        Drawn::of(&mut app, 60, 16).bg_at(10, row),
        Some(accent_dimmed())
    );
    // The bar takes the colour of the dimmed background, so it does not show.
    let bar = Drawn::of(&mut app, 60, 16).style_at(0, row);
    assert_eq!(
        (bar.fg, bar.bg),
        (Some(accent_dimmed()), Some(accent_dimmed()))
    );
    assert!(app.terminal_focus(true));
    assert_eq!(Drawn::of(&mut app, 60, 16).bg_at(10, row), Some(accent()));
    let bar = Drawn::of(&mut app, 60, 16).style_at(0, row);
    assert_eq!(
        (bar.fg, bar.bg),
        (Style::from(theme::on_accent()).fg, Some(accent()))
    );
}

#[test]
fn leaving_the_terminal_pane_lets_go_of_the_link_under_the_pointer() {
    let mut app = app();
    let (column, row) = find(&mut app, 60, 24, "a / d");
    assert!(app.hover(column, row));
    screen(&mut app, 60, 24);
    assert!(app.hovered.is_some());
    app.terminal_focus(false);
    screen(&mut app, 60, 24);
    assert!(app.hovered.is_none());
}

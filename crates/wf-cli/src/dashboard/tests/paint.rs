//! Dashboard tests: paint.

use super::support::*;
use crate::dashboard::draw;
use crate::theme::Background;
use ratatui::crossterm::event::KeyCode;
use ratatui::style::{Color, Style};

#[test]
fn a_background_colour_and_the_text_are_under_every_cell_popups_included() {
    let background = anstyle::RgbColor(0x1b, 0x1d, 0x23);
    let text = anstyle::RgbColor(0xd0, 0xd0, 0xd0);
    let mut app = app();
    app.base = draw::base(
        anstyle::Style::new().fg_color(Some(text.into())),
        Background::Color(background.into()),
    );
    let (background, text) = (Color::from(background), Color::from(text));
    // The keys are a popup over the list, drawn on an area cleared for it.
    press(&mut app, KeyCode::Char('?'));
    let drawn = Drawn::of(&mut app, 60, 24);
    let buffer = drawn.buffer();
    let accents = [
        accent(),
        accent_dimmed(),
        tab_active().bg.unwrap(),
        tab_active_dimmed().bg.unwrap(),
    ];
    for cell in buffer.content() {
        let style = cell.style();
        assert!(
            style.bg == Some(background) || style.bg.is_some_and(|bg| accents.contains(&bg)),
            "{:?} on {:?}",
            cell.symbol(),
            style.bg
        );
        assert!(
            style.fg.is_some_and(|fg| fg != Color::Reset),
            "{:?} in {:?}",
            cell.symbol(),
            style.fg
        );
    }
    // A blank inside the popup, and one outside it, in the text colour on the background.
    let (x, y) = find(&mut app, 60, 24, "switch tabs");
    for (x, y) in [(x + 35, y), (59, 0)] {
        let style = buffer[(x, y)].style();
        assert_eq!(buffer[(x, y)].symbol(), " ");
        assert_eq!((style.fg, style.bg), (Some(text), Some(background)));
    }
}

#[test]
fn the_terminals_background_and_no_text_paint_nothing() {
    let mut app = app();
    // The built-in theme.
    assert_eq!(app.base, Style::new());
    assert_eq!(
        app.base,
        draw::base(anstyle::Style::new(), Background::Terminal)
    );
    press(&mut app, KeyCode::Char('?'));
    let drawn = Drawn::of(&mut app, 60, 24);
    let buffer = drawn.buffer();
    // Nothing but the accents has a background, inside the popup as outside it.
    let accents = [
        accent(),
        accent_dimmed(),
        tab_active().bg.unwrap(),
        tab_active_dimmed().bg.unwrap(),
    ];
    for cell in buffer.content() {
        let bg = cell.style().bg;
        assert!(
            bg == Some(Color::Reset) || bg.is_some_and(|bg| accents.contains(&bg)),
            "{:?} on {:?}",
            cell.symbol(),
            bg
        );
    }
    // A blank inside the popup, and one outside it, in the terminal's own colours.
    let (x, y) = find(&mut app, 60, 24, "switch tabs");
    for (x, y) in [(x + 35, y), (59, 0)] {
        assert_eq!(buffer[(x, y)], ratatui::buffer::Cell::EMPTY);
    }
}

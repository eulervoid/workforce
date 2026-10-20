//! Dashboard tests: pointer.

use super::support::*;
use crate::dashboard::app::{AgentRow, Effect, Prompt, Tab, Wheel};
use crate::dashboard::snapshot::{Detail, ItemListing};
use ratatui::crossterm::event::KeyCode;
use wf_core::id::{ItemId, SessionId};

#[test]
fn the_wheel_scrolls_the_list_and_leaves_the_selection() {
    let mut app = long_item_list();
    let first = Some(ItemId::from_bytes([1; 32]));
    let detail = app.wanted().detail;
    screen(&mut app, 130, 14);
    for _ in 0..4 {
        app.wheel(10, Wheel::Down);
        screen(&mut app, 130, 14);
    }
    // Drawing keeps the list where the wheel left it, with the selection off screen.
    let shown = screen(&mut app, 130, 14);
    assert!(!shown.contains("Item 1 "));
    assert!(shown.contains("Item 14"));
    assert_eq!(selected_item(&app), first);
    assert_eq!(app.wanted().detail, detail);
    assert!(app.list_has_keys());
    // Past the end of the list, the wheel stops; one notch back moves it up again.
    for _ in 0..20 {
        app.wheel(10, Wheel::Down);
        screen(&mut app, 130, 14);
    }
    assert!(screen(&mut app, 130, 14).contains("Item 30"));
    app.wheel(10, Wheel::Up);
    assert!(!screen(&mut app, 130, 14).contains("Item 30"));
    // A refresh keeps the list where it was scrolled to.
    let shown = screen(&mut app, 130, 14);
    app.refresh(app.snapshot.clone());
    assert_eq!(screen(&mut app, 130, 14), shown);
}

#[test]
fn a_key_after_scrolling_moves_the_selection_and_shows_it() {
    let mut app = long_item_list();
    screen(&mut app, 130, 14);
    for _ in 0..4 {
        app.wheel(10, Wheel::Down);
        screen(&mut app, 130, 14);
    }
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(selected_item(&app), Some(ItemId::from_bytes([2; 32])));
    let shown = screen(&mut app, 130, 14);
    assert!(shown.contains("Item 1 "));
    assert!(shown.contains("Item 2 "));
}

#[test]
fn a_refresh_that_moves_the_selection_shows_it() {
    let mut app = long_item_list();
    screen(&mut app, 130, 14);
    for _ in 0..4 {
        app.wheel(10, Wheel::Down);
        screen(&mut app, 130, 14);
    }
    // The selected item is gone, so the selection moves to the next one, which the list shows.
    let mut snapshot = app.snapshot.clone();
    snapshot.items[0].items.remove(0);
    app.refresh(snapshot);
    assert_eq!(selected_item(&app), Some(ItemId::from_bytes([2; 32])));
    assert!(screen(&mut app, 130, 14).contains("Item 2 "));
}

#[test]
fn a_click_on_a_scrolled_list_selects_the_row_under_it() {
    let mut app = long_item_list();
    screen(&mut app, 130, 14);
    for _ in 0..4 {
        app.wheel(10, Wheel::Down);
        screen(&mut app, 130, 14);
    }
    let row = screen(&mut app, 130, 14)
        .lines()
        .position(|line| line.contains("Item 15 "))
        .unwrap();
    app.click(10, u16::try_from(row).unwrap());
    assert_eq!(selected_item(&app), Some(ItemId::from_bytes([15; 32])));
    // The list stays where it was, the new selection on screen.
    assert_eq!(
        screen(&mut app, 130, 14)
            .lines()
            .position(|line| line.contains("Item 15 ")),
        Some(row)
    );
}

#[test]
fn the_list_behind_an_open_detail_takes_no_keys() {
    // An agent opened and `→` pressed again stays one level deep.
    let mut app = app();
    press(&mut app, KeyCode::Char('3'));
    app.refresh(with_agents());
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Right);
    assert_eq!(app.path().len(), 1);
    assert!(!screen(&mut app, 60, 16).contains("coder › coder"));
    // A session opened from the Now tab goes one further, to the diff of its item, and no
    // further.
    press(&mut app, KeyCode::Char('1'));
    app.refresh(snapshot());
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Right);
    assert!(matches!(
        app.path()[..],
        [Detail::Session(_), Detail::Diff(_)]
    ));
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.path(), Vec::new());
    // The grouping and filters of the items stay as they are under an item, while the keys of
    // the dashboard still work.
    press(&mut app, KeyCode::Char('2'));
    app.refresh(with_items());
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Char('o'));
    press(&mut app, KeyCode::Char('O'));
    press(&mut app, KeyCode::Char('F'));
    press(&mut app, KeyCode::Char('/'));
    assert_eq!(
        item_listing(&app),
        Some(ItemListing {
            grouping: crate::cli::ItemGrouping::State,
            order: crate::cli::ItemOrder::Priority,
            search: None,
        })
    );
    assert!(app.prompt.is_none());
    assert_eq!(app.item_groups().len(), with_items().items.len());
    press(&mut app, KeyCode::Char('?'));
    assert!(matches!(app.prompt, Some(Prompt::Keys)));
    press(&mut app, KeyCode::Esc);
    assert_eq!(press(&mut app, KeyCode::Char('q')), Some(Effect::Quit));
}

#[test]
fn a_click_on_a_tab_switches_to_it() {
    let mut app = app();
    let (column, row) = find(&mut app, 60, 24, "3 Agents");
    app.click(column, row);
    assert_eq!(app.tab, Tab::Agents);
    // Its padding is part of it; the space between two tabs is not.
    let (column, row) = find(&mut app, 60, 24, "2 Items");
    app.click(column - 1, row);
    assert_eq!(app.tab, Tab::Items);
    let (column, row) = find(&mut app, 60, 24, "1 Now");
    app.click(column - 2, row);
    assert_eq!(app.tab, Tab::Items);
    // While a prompt is open, a click does nothing.
    press(&mut app, KeyCode::Char('?'));
    app.click(column, row);
    assert_eq!(app.tab, Tab::Items);
    assert!(matches!(app.prompt, Some(Prompt::Keys)));
    // The filter of the log lets the mouse through, and is taken as typed.
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('4'));
    press(&mut app, KeyCode::Char('/'));
    press(&mut app, KeyCode::Char('x'));
    app.click(column, row);
    assert_eq!(app.tab, Tab::Now);
    assert!(app.prompt.is_none());
    assert_eq!(app.log.filter, "x");
}

#[test]
fn a_click_on_an_entry_selects_it_and_a_second_opens_it() {
    let mut app = app();
    press(&mut app, KeyCode::Char('2'));
    app.refresh(with_items());
    let dashboard = ItemId::from_bytes([4; 32]);
    let blocked = ItemId::from_bytes([5; 32]);
    let parse = ItemId::from_bytes([1; 32]);
    // Four lines of the list are on screen, scrolled down to its last item.
    press(&mut app, KeyCode::Char('G'));
    let (_, row) = find(&mut app, 60, 11, "Session subscription");
    assert!(!screen(&mut app, 60, 11).contains("Dashboard"));
    app.click(20, row);
    assert_eq!(app.items.list.selection.get(), Some(&blocked));
    assert_eq!(app.wanted().detail, None);
    // A heading or a blank line is no entry.
    let (_, heading) = find(&mut app, 60, 11, "active (1)");
    app.click(20, heading);
    app.click(20, heading - 1);
    assert_eq!(app.items.list.selection.get(), Some(&blocked));
    // The keys move on from the entry clicked.
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.items.list.selection.get(), Some(&parse));
    // A click on the selection opens it, as enter would.
    let (_, row) = find(&mut app, 60, 11, "Parse config lazily");
    app.click(20, row);
    assert_eq!(app.wanted().detail, Some(Detail::Item(parse)));
    // With the detail in place of the list, the click lands in the detail.
    screen(&mut app, 60, 11);
    app.click(20, row);
    assert_eq!(app.path(), vec![Detail::Item(parse)]);
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('g'));
    assert_eq!(app.items.list.selection.get(), Some(&dashboard));
}

#[test]
fn a_click_below_the_last_entry_does_nothing() {
    let mut app = app();
    press(&mut app, KeyCode::Char('2'));
    app.refresh(with_items());
    let (_, row) = find(&mut app, 60, 24, "Parse config lazily");
    app.click(20, row + 1);
    app.click(20, row + 5);
    let dashboard = ItemId::from_bytes([4; 32]);
    assert_eq!(app.items.list.selection.get(), Some(&dashboard));
    assert_eq!(app.wanted().detail, None);
}

#[test]
fn a_click_beside_the_list_gives_the_detail_the_keys() {
    let mut app = app();
    app.resize(130, 24);
    press(&mut app, KeyCode::Char('3'));
    app.refresh(with_agents());
    let coder = SessionId::from_bytes([11; 32]);
    screen(&mut app, 130, 24);
    app.click(100, 10);
    assert!(!app.list_has_keys());
    // A click on the selection only takes the keys back to the list; the next opens it.
    let (column, row) = find(&mut app, 130, 24, "claude-code · opus");
    app.click(column, row);
    assert!(app.list_has_keys());
    assert_eq!(app.stack().len(), 1);
    app.click(column, row);
    assert!(!app.list_has_keys());
    // A click on another entry selects it, and its detail takes the place of the one opened.
    let (column, row) = find(&mut app, 130, 24, "#aeaqca Parse");
    app.click(column, row);
    assert_eq!(app.agents.selection.get(), Some(&AgentRow::Session(coder)));
    assert!(app.list_has_keys());
    assert_eq!(app.wanted().detail, Some(Detail::Session(coder)));
    // Not on the footer below the detail.
    app.click(100, 23);
    assert!(app.list_has_keys());
}

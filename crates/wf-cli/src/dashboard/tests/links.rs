//! Dashboard tests: links.

use super::support::*;
use crate::dashboard::app::{App, Prompt, Target};
use crate::dashboard::snapshot::Detail;
use ratatui::crossterm::event::KeyCode;
use wf_core::id::{CommentId, HashId, ItemId, SessionId};

#[test]
fn a_click_on_an_item_id_an_agent_or_a_session_opens_it_and_esc_comes_back() {
    let mut app = app_with_links();
    let spans = Detail::Item(ItemId::from_bytes([2; 32]));
    let opens = [
        ("#aeaqca", Detail::Item(ItemId::from_bytes([1; 32]))),
        ("@ada/coder.", Detail::Agent(coder_agent())),
        (
            "@ada/coder/",
            Detail::Session(SessionId::from_bytes([11; 32])),
        ),
    ];
    for (text, detail) in opens {
        let (column, row) = find(&mut app, 60, 24, text);
        assert_eq!(app.click(column + 1, row), None);
        assert_eq!(app.path(), vec![spans.clone(), detail], "{text}");
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.path(), vec![spans.clone()]);
    }
    // A principal has no detail, and the item on screen is no link to itself.
    let (column, row) = find(&mut app, 60, 24, "@ada ·");
    app.click(column + 1, row);
    let (column, row) = find(&mut app, 60, 24, "#aibaea (task");
    app.click(column + 1, row);
    assert_eq!(app.path(), vec![spans]);
}

#[test]
fn a_click_on_a_commit_shows_the_diff_of_its_item() {
    let mut app = app_with_links();
    let (column, row) = find(&mut app, 60, 24, "a8d49484");
    assert_eq!(app.click(column, row), None);
    assert_eq!(
        app.wanted().detail,
        Some(Detail::Diff(ItemId::from_bytes([2; 32])))
    );
}

#[test]
fn a_link_in_the_log_opens_in_its_place_and_esc_goes_back_to_the_log() {
    let mut app = app_with_links();
    press(&mut app, KeyCode::Char('4'));
    let comment = CommentId::from_bytes([21; 32]).short();
    app.logged([format!(
        "07:05  #aibaea  @ada · commented · {comment}\n         Keep the span on the error.\n"
    )]);
    // A comment is about the item on its line.
    let (column, row) = find(&mut app, 60, 24, &comment);
    app.click(column, row);
    let spans = Detail::Item(ItemId::from_bytes([2; 32]));
    assert_eq!(app.wanted().detail, Some(spans.clone()));
    assert!(screen(&mut app, 60, 24).contains("Log ›▐#aibaea"));
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.wanted().detail, None);
    assert!(screen(&mut app, 60, 24).contains("Keep the span"));
    // A word as long as a comment id that is none is no link.
    let (column, row) = find(&mut app, 60, 24, "Keep");
    app.click(column, row);
    assert_eq!(app.wanted().detail, None);
}

#[test]
fn h_on_a_detail_opened_from_the_log_closes_it_as_the_log_has_no_pane_to_keep_it_beside() {
    let mut app = app_with_links();
    press(&mut app, KeyCode::Char('4'));
    app.logged(["07:05  #aibaea  @ada · claimed\n".to_owned()]);
    let (column, row) = find(&mut app, 60, 24, "#aibaea");
    app.click(column, row);
    assert_eq!(app.stack().len(), 1);
    press(&mut app, KeyCode::Char('h'));
    assert!(app.stack().is_empty());
    assert!(app.list_has_keys());
    assert_eq!(app.wanted().detail, None);
    assert!(screen(&mut app, 60, 24).contains("/ filter"));
}

#[test]
fn a_click_on_a_key_hint_does_what_its_key_does() {
    let mut app = app();
    let archive = ItemId::from_bytes([3; 32]);
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    let (column, row) = find(&mut app, 60, 24, "v review");
    assert_eq!(app.click(column + 3, row), None);
    assert!(matches!(app.prompt, Some(Prompt::Verdict { item }) if item == archive));
    press(&mut app, KeyCode::Esc);
    let (column, row) = find(&mut app, 60, 24, "D diff");
    assert_eq!(app.click(column, row), None);
    assert_eq!(app.wanted().detail, Some(Detail::Diff(archive)));
    press(&mut app, KeyCode::Esc);
    // `j/k` names two keys, and the space between two hints is none.
    let selected = app.now.selection.get().cloned();
    let (column, row) = find(&mut app, 60, 24, "j/k move");
    app.click(column, row);
    app.click(column - 2, row);
    assert_eq!(app.now.selection.get().cloned(), selected);
    assert_eq!(app.wanted().detail, None);
    let (column, row) = find(&mut app, 60, 24, "enter open");
    app.click(column, row);
    assert_eq!(app.wanted().detail, Some(Detail::Item(archive)));
}

#[test]
fn a_click_on_a_part_of_the_breadcrumb_goes_back_to_it() {
    let mut app = app_with_links();
    let spans = Detail::Item(ItemId::from_bytes([2; 32]));
    let open_parse = |app: &mut App| {
        let (column, row) = find(app, 60, 24, "#aeaqca,");
        app.click(column, row);
        assert_eq!(app.path().len(), 2);
    };
    open_parse(&mut app);
    // The detail on top is where the breadcrumb is already.
    let (column, row) = find(&mut app, 60, 24, "#aeaqca▌");
    app.click(column, row);
    assert_eq!(app.path().len(), 2);
    let (column, row) = find(&mut app, 60, 24, "#aibaea ›");
    app.click(column + 6, row);
    assert_eq!(app.path(), vec![spans]);
    open_parse(&mut app);
    let (column, row) = find(&mut app, 60, 24, "Now ›");
    app.click(column, row);
    assert!(app.stack().is_empty());
    assert!(app.list_has_keys());
}

#[test]
fn a_detail_kept_beside_the_list_has_no_crumbs_or_hints_to_click() {
    let (mut app, scrolled) = scrolled_item(130);
    press(&mut app, KeyCode::Char('h'));
    let screen = screen(&mut app, 130, 24);
    assert!(screen.contains("enter open"), "{screen}");
    assert!(!screen.contains("esc back"), "{screen}");
    assert!(
        !app.clickable
            .spots
            .iter()
            .any(|spot| matches!(spot.target, Target::Back { .. }))
    );
    assert_eq!(app.on_screen(), Some(scrolled));
}

#[test]
fn a_move_draws_again_only_onto_or_off_a_link_or_a_button() {
    let mut app = app_with_links();
    let (column, row) = find(&mut app, 60, 24, "#aeaqca,");
    let (plain, plain_row) = find(&mut app, 60, 24, "After");
    assert!(!app.hover(plain, plain_row));
    assert!(!app.hover(plain + 2, plain_row));
    assert!(app.hover(column, row));
    // Along the same link.
    assert!(!app.hover(column + 3, row));
    assert!(app.hover(plain, plain_row));
    // Onto a button.
    let (column, row) = find(&mut app, 60, 24, "esc back");
    assert!(app.hover(column, row));
}

#[test]
fn a_link_under_the_pointer_is_underlined_and_its_tip_shows() {
    use ratatui::style::Modifier;
    let mut app = app_with_links();
    let tips = [
        ("#aeaqca,", "open #aeaqca"),
        ("@ada/coder.", "open @ada/coder"),
        ("@ada/coder/", "open the transcript of @ada/coder/bmfqwc"),
        ("a8d49484", "open the diff of a8d49484"),
        ("esc back", "back (esc)"),
        ("Now ›", "back to Now"),
    ];
    for (text, tip) in tips {
        let (column, row) = find(&mut app, ROOMY, 24, text);
        app.hover(column + 1, row);
        assert!(
            Drawn::of(&mut app, ROOMY, 24)
                .style_at(column + 1, row)
                .add_modifier
                .contains(Modifier::UNDERLINED),
            "{text}"
        );
        assert_tip(&mut app, tip);
    }
    // Back on plain text, the tip goes.
    let (column, row) = find(&mut app, ROOMY, 24, "After");
    app.hover(column, row);
    assert!(!screen(&mut app, ROOMY, 24).contains("back to Now"));
    assert!(
        !Drawn::of(&mut app, ROOMY, 24)
            .style_at(column, row)
            .add_modifier
            .contains(Modifier::UNDERLINED)
    );
}

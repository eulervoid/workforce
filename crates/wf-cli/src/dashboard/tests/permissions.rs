//! Dashboard tests: permissions.

use super::support::*;
use crate::dashboard::app::{App, CONFIRM_LASTS_MS, Tab};
use crate::dashboard::snapshot::InView;
use ratatui::crossterm::event::KeyCode;
use ratatui::style::Modifier;
use wf_harness::Permission;

#[test]
fn a_tip_shows_beside_the_latest_event_or_not_at_all() {
    let mut app = app();
    let event = "07:04  #ambqga  @ada/coder/pp74s3 · submitted 3e8a1c";
    let (deny, row) = find(&mut app, ROOMY, 24, "a / d");
    app.hover(deny + 4, row);
    assert_tip(&mut app, "deny the request");
    assert_eq!(find(&mut app, ROOMY, 24, event), (1, LATEST));
    // Where the latest event leaves no room for it, the tip is left out, not cut.
    let (deny, row) = find(&mut app, 60, 24, "a / d");
    app.hover(deny + 4, row);
    let narrow = screen(&mut app, 60, 24);
    let latest = narrow.lines().nth(usize::from(LATEST)).unwrap();
    assert!(!latest.contains("deny"), "{narrow}");
    assert_eq!(find(&mut app, 60, 24, event), (1, LATEST));
}

#[test]
fn one_click_on_allow_allows_nothing_and_a_second_allows() {
    let mut app = app();
    let (allow, row) = find(&mut app, ROOMY, 24, "a / d");
    app.hover(allow, row);
    assert_tip(&mut app, "allow the request: click twice");
    assert_eq!(app.click(allow, row), None);
    assert_tip(&mut app, "allow: click again to confirm");
    assert_eq!(app.click(allow, row), permit(Permission::Allow));
}

#[test]
fn a_click_on_deny_denies_at_once() {
    let mut app = app();
    let (allow, row) = find(&mut app, ROOMY, 24, "a / d");
    app.hover(allow + 4, row);
    assert_tip(&mut app, "deny the request");
    assert_eq!(app.click(allow + 4, row), permit(Permission::Deny));
}

#[test]
fn a_first_click_on_allow_lapses_after_its_time_or_on_anything_else() {
    let mut app = app();
    let (allow, row) = find(&mut app, 60, 24, "a / d");
    // Another click in between.
    app.click(allow, row);
    let (live, counts) = find(&mut app, 60, 24, "3 live");
    app.click(live, counts);
    assert_eq!(app.click(allow, row), None);
    // A key in between.
    press(&mut app, KeyCode::Char('?'));
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.click(allow, row), None);
    // Too long in between.
    let mut later = snapshot();
    later.now_ms = NOW_MS + CONFIRM_LASTS_MS;
    app.refresh(later.clone());
    assert!(!screen(&mut app, 60, 24).contains("allow: click again to confirm"));
    assert_eq!(app.click(allow, row), None);
    // Its request answered in between, by another.
    let mut answered = later;
    answered.waiting.remove(0);
    app.refresh(answered);
    assert_eq!(app.unconfirmed_allow, None);
}

#[test]
fn the_answers_under_a_session_and_in_the_footer_are_buttons_too() {
    let mut app = app();
    // In the footer of the Now tab.
    let (allow, row) = find(&mut app, 60, 24, "a allow");
    assert_eq!(app.click(allow, row), None);
    assert_eq!(app.click(allow, row), permit(Permission::Allow));
    let (deny, row) = find(&mut app, 60, 24, "d deny");
    assert_eq!(app.click(deny, row), permit(Permission::Deny));
    // Under the transcript, above the footer.
    let coder = app.snapshot.working[0].clone();
    press(&mut app, KeyCode::Enter);
    let mut snapshot = snapshot();
    snapshot.in_view = Some(InView::Session(coder));
    app.refresh(snapshot);
    let (allow, row) = find(&mut app, 60, 24, "a allow");
    assert!(row < LATEST);
    assert_eq!(app.click(allow, row), None);
    assert_eq!(app.click(allow, row), permit(Permission::Allow));
    let (deny, row) = find(&mut app, 60, 24, "d deny");
    assert!(row < LATEST);
    assert_eq!(app.click(deny, row), permit(Permission::Deny));
}

#[test]
fn the_title_of_the_keys_is_not_dimmed_as_their_border() {
    let mut app = app();
    press(&mut app, KeyCode::Char('?'));
    let (title, row) = find(&mut app, 60, 18, "▐keys▌");
    for column in title..title + "▐keys▌".chars().count() as u16 {
        let style = Drawn::of(&mut app, 60, 18).style_at(column, row);
        assert!(!style.add_modifier.contains(Modifier::DIM), "{column}");
    }
    let border = Drawn::of(&mut app, 60, 18).style_at(title - 1, row);
    assert!(border.add_modifier.contains(Modifier::DIM));
}

#[test]
fn a_click_on_a_count_in_the_header_does_nothing() {
    let mut app = app();
    let before = screen(&mut app, 60, 24);
    for count in ["3 live", "14 open", "1 blocked"] {
        let (column, row) = find(&mut app, 60, 24, count);
        assert!(!app.hover(column, row), "{count}");
        assert_eq!(app.click(column, row), None, "{count}");
        assert_eq!(app.tab, Tab::Now, "{count}");
        assert_eq!(screen(&mut app, 60, 24), before, "{count}");
    }
}

#[test]
fn an_inactive_tab_under_the_pointer_is_drawn_stronger_than_the_others() {
    use ratatui::style::Modifier;
    let mut app = app();
    let (items, row) = find(&mut app, 60, 24, "2 Items");
    let (agents, _) = find(&mut app, 60, 24, "3 Agents");
    let (now, _) = find(&mut app, 60, 24, "1 Now");
    let modifiers =
        |app: &mut App, column| Drawn::of(app, 60, 24).style_at(column, row).add_modifier;
    let bg = |app: &mut App, column| Drawn::of(app, 60, 24).bg_at(column, row);
    assert!(modifiers(&mut app, items).contains(Modifier::DIM));
    assert!(app.hover(items, row));
    assert!(!modifiers(&mut app, items).contains(Modifier::DIM));
    assert_ne!(bg(&mut app, items), tab_active().bg);
    assert!(modifiers(&mut app, agents).contains(Modifier::DIM));
    assert_eq!(bg(&mut app, now), tab_active().bg);
    // Along the same tab nothing is drawn again; off it, it is dim again.
    assert!(!app.hover(items + 3, row));
    assert!(app.hover(items, row + 3));
    assert!(modifiers(&mut app, items).contains(Modifier::DIM));
}

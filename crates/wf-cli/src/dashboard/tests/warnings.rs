//! Dashboard tests: warnings.

use super::support::*;
use crate::dashboard::app::{App, STATUS_LASTS_MS};
use crate::dashboard::snapshot::{Detail, MovedBack, Snapshot, moves_back};
use ratatui::crossterm::event::KeyCode;
use ratatui::style::Color;
use wf_core::id::{CommentId, ItemId};
use wf_core::item::ItemState;

#[test]
fn a_move_back_to_open_warns_beside_the_latest_event() {
    let mut app = app();
    app.warn([conflict(1)]);
    insta::assert_snapshot!("moved_back", footer(&mut app, ROOMY));
    let (column, row) = find(&mut app, ROOMY, 24, "⚠ #aeaqca back to open");
    assert_eq!(row, LATEST);
    let style = Drawn::of(&mut app, ROOMY, 24).style_at(column, row);
    assert_eq!(style.fg, Some(Color::Yellow));
    // Several show the latest with a count, the reason cut to make room for it.
    app.warn([conflict(2)]);
    insta::assert_snapshot!("moved_back_twice", footer(&mut app, 60));
}

#[test]
fn a_warning_outlasts_a_status_and_goes_with_a_key() {
    let mut app = app();
    app.warn([conflict(1)]);
    app.refresh(Snapshot {
        now_ms: NOW_MS + STATUS_LASTS_MS * 10,
        ..snapshot()
    });
    assert!(footer(&mut app, ROOMY).contains("back to open"));
    press(&mut app, KeyCode::Char('j'));
    assert!(!footer(&mut app, ROOMY).contains("back to open"));
}

#[test]
fn a_click_on_a_warning_opens_its_item_and_ends_the_warning() {
    let mut app = app();
    app.warn([conflict(1), conflict(2)]);
    let (column, row) = find(&mut app, ROOMY, 24, "⚠ #aibaea");
    app.hover(column, row);
    // Under the pointer, the warning stays rather than giving way to its tip.
    assert!(footer(&mut app, ROOMY).contains("⚠ #aibaea back to open"));
    app.click(column, row);
    assert_eq!(
        app.on_screen().map(|opened| opened.detail),
        Some(Detail::Item(ItemId::from_bytes([2; 32])))
    );
    // The other one is left.
    assert!(footer(&mut app, ROOMY).contains("⚠ #aeaqca back to open"));
}

#[test]
fn a_tip_takes_the_place_of_a_warning_while_it_shows() {
    let mut app = app();
    app.warn([conflict(1)]);
    let (deny, row) = find(&mut app, ROOMY, 24, "a / d");
    app.hover(deny + 4, row);
    assert_tip(&mut app, "deny the request");
    assert!(!footer(&mut app, ROOMY).contains("back to open"));
    app.hover(0, 0);
    assert!(footer(&mut app, ROOMY).contains("back to open"));
}

#[test]
fn only_new_daemon_moves_from_review_back_to_open_with_a_reason_warn() {
    use wf_core::event::{Event, EventBody};
    use wf_core::identity::Actor;
    use wf_core::testing::{Fixture, comment, create, moved};

    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let mut events = Vec::new();
    let apply = |state: &mut wf_core::state::State, events: &mut Vec<Event>, body| {
        let event = fixture.apply(state, 0, 2_000 + events.len() as u64, body);
        events.push(event.clone());
        event
    };
    let created = apply(&mut state, &mut events, create("task", "Conflicts"));
    let item = ItemId::from(created.id());
    apply(&mut state, &mut events, moved(item, ItemState::Review));
    let daemon = Actor::Daemon(fixture.founder.key());
    let note = fixture.author_as(
        &state,
        0,
        daemon.clone(),
        3_000,
        comment(
            item,
            "Merging into main conflicts in README.md. The next session resolves it.",
        ),
    );
    let mut back = moved(item, ItemState::Open);
    if let EventBody::ItemMoved(back) = &mut back {
        back.because = Some(CommentId::from(note.id()));
    }
    let back = fixture.author_as(&state, 0, daemon, 3_001, back);
    events.extend([note, back]);

    let warned = vec![MovedBack {
        item,
        reason: "merging into main conflicts in README.md".to_owned(),
    }];
    assert_eq!(moves_back(&events, 2), warned);
    // A move from before the dashboard started does not warn.
    assert_eq!(moves_back(&events, events.len()), Vec::new());
    // Nor does one the principal made.
    let by_principal = fixture.author(&state, 0, 3_002, moved(item, ItemState::Open));
    events.push(by_principal);
    assert_eq!(moves_back(&events, events.len() - 1), Vec::new());
}

#[test]
fn the_header_says_how_the_checkouts_target_stands_against_wfs() {
    use crate::view::CheckoutTarget;
    use wf_vcs::TargetStanding;

    let mut app = app();
    let header = |app: &mut App| screen(app, ROOMY, 24).lines().next().unwrap().to_owned();
    app.refresh(Snapshot {
        checkout_target: CheckoutTarget::new("main", TargetStanding::Ahead { commits: 3 }),
        ..snapshot()
    });
    insta::assert_snapshot!("checkout_ahead", header(&mut app));
    let (column, row) = find(&mut app, ROOMY, 24, "⚠ main 3 ahead");
    let style = Drawn::of(&mut app, ROOMY, 24).style_at(column, row);
    assert_eq!(style.fg, Some(Color::Yellow));
    app.refresh(Snapshot {
        checkout_target: CheckoutTarget::new("main", TargetStanding::Behind { commits: 2 }),
        ..snapshot()
    });
    insta::assert_snapshot!("checkout_behind", header(&mut app));
    app.refresh(Snapshot {
        checkout_target: CheckoutTarget::new("main", TargetStanding::Same),
        ..snapshot()
    });
    assert!(!header(&mut app).contains("main"), "{}", header(&mut app));
}

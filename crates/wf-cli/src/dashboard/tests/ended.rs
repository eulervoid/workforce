//! Dashboard tests: a session that ends while its detail is open, and the one its item goes on in.

use super::support::*;
use crate::dashboard::app::NowRow;
use crate::dashboard::snapshot::{Detail, InView, Snapshot};
use crate::view::SessionStatus;
use ratatui::crossterm::event::KeyCode;
use wf_core::id::SessionId;
use wf_core::session::EndReason;

/// The snapshot once the coder's session was stopped, with its permission request gone, and the
/// coder's ended session in view; `successor` is the session its item went on in, if one started.
fn coder_stopped(successor: Option<crate::dashboard::snapshot::SessionInfo>) -> Snapshot {
    let mut snapshot = snapshot();
    let coder = with_status(snapshot.working.remove(0), ended(EndReason::Cancelled, 0));
    snapshot.waiting.remove(0);
    if let Some(successor) = &successor {
        snapshot.working.insert(0, successor.clone());
    }
    snapshot.in_view = Some(InView::Session {
        info: coder,
        successor,
    });
    snapshot
}

/// The coder's next session on its item, started a minute ago.
fn next_session() -> crate::dashboard::snapshot::SessionInfo {
    session(14, "coder", item(1, "Parse config lazily"), 1)
}

#[test]
fn an_open_session_that_ends_says_so_and_offers_the_session_its_item_goes_on_in() {
    let mut app = app();
    let coder = app.snapshot.working[0].clone();
    app.transcribed(coder.id, &coder_transcript());
    press(&mut app, KeyCode::Enter);
    let mut snapshot = snapshot();
    snapshot.in_view = Some(session_in_view(coder.clone()));
    app.refresh(snapshot);
    assert!(!screen(&mut app, 60, 16).contains("ended"));
    // Stopped: the transcript stays open, and says it ended.
    app.refresh(coder_stopped(None));
    assert_eq!(app.wanted().detail, Some(Detail::Session(coder.id)));
    let stopped = screen(&mut app, 60, 16);
    assert!(stopped.contains("■ ended: cancelled"), "{stopped}");
    assert!(!stopped.contains("next session"), "{stopped}");
    // The dispatcher starts the next: offered without selecting anything again.
    let next = next_session();
    app.refresh(coder_stopped(Some(next.clone())));
    let offered = screen(&mut app, 90, 16);
    assert!(
        offered.contains("its item goes on in coder · work · 1m ◆"),
        "{offered}"
    );
    assert!(offered.contains("n next session"), "{offered}");
    press(&mut app, KeyCode::Char('n'));
    assert_eq!(app.wanted().detail, Some(Detail::Session(next.id)));
    // In place of the one that ended: esc goes back to the list.
    assert_eq!(app.stack().len(), 1);
}

#[test]
fn the_next_session_takes_the_place_of_the_ended_one_opened_from_its_item() {
    let mut app = app();
    let coder = app.snapshot.working[0].clone();
    let parse = coder.item;
    app.open_item(parse);
    let mut snapshot = snapshot();
    snapshot.in_view = Some(item_in_view(parse, "Parse\n".to_owned(), Some(coder.id)));
    app.refresh(snapshot);
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.wanted().detail, Some(Detail::Session(coder.id)));
    let next = next_session();
    app.refresh(coder_stopped(Some(next.clone())));
    // A click on the key in the bar under the transcript does what `n` does.
    let (column, row) = find(&mut app, 60, 16, "n next session");
    app.click(column, row);
    assert_eq!(app.wanted().detail, Some(Detail::Session(next.id)));
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.wanted().detail, Some(Detail::Item(parse)));
}

#[test]
fn a_live_session_offers_no_next_one() {
    let mut app = app();
    let coder = app.snapshot.working[0].clone();
    press(&mut app, KeyCode::Enter);
    let mut snapshot = snapshot();
    snapshot.in_view = Some(session_in_view(coder.clone()));
    app.refresh(snapshot);
    press(&mut app, KeyCode::Char('n'));
    assert_eq!(app.wanted().detail, Some(Detail::Session(coder.id)));
}

#[test]
fn a_session_kept_beside_the_list_gives_way_when_its_row_is_gone() {
    let mut app = app();
    app.resize(130, 24);
    let coder = app.snapshot.working[0].clone();
    let reviewer = SessionId::from_bytes([13; 32]);
    // The coder's row, under the three that wait: opened, then the keys back to the list.
    for _ in 0..3 {
        press(&mut app, KeyCode::Char('j'));
    }
    assert_eq!(app.now.selection.get(), Some(&NowRow::Session(coder.id)));
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Esc);
    assert!(app.list_has_keys());
    assert!(!app.stack().is_empty());
    app.refresh(coder_stopped(None));
    // The selection moved to the row now where the coder's was, and its detail is beside it.
    assert_eq!(app.now.selection.get(), Some(&NowRow::Session(reviewer)));
    assert!(app.stack().is_empty());
    assert_eq!(app.wanted().detail, Some(Detail::Session(reviewer)));
}

#[test]
fn a_session_ended_long_ago_says_so_too() {
    let mut app = app();
    let coder = app.snapshot.working[0].clone();
    press(&mut app, KeyCode::Enter);
    let mut snapshot = snapshot();
    snapshot.in_view = Some(session_in_view(with_status(
        coder,
        SessionStatus::Ended {
            reason: EndReason::Submitted,
            at: crate::view::timestamp_ms(NOW_MS - 3_600_000),
        },
    )));
    app.refresh(snapshot);
    let screen = screen(&mut app, 60, 16);
    assert!(screen.contains("■ ended: submitted"), "{screen}");
}

#[test]
fn a_session_in_view_has_a_successor_once_it_ended_and_its_item_went_on() {
    use crate::dashboard::snapshot::Sources;
    use wf_core::state::State;
    use wf_core::testing::*;

    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let apply = |state: &mut State, now_ms, body| fixture.apply(state, 0, now_ms, body);
    apply(&mut state, 1_000, define_agent("coder"));
    let item = apply(&mut state, 1_000, create("task", "Parse"))
        .id()
        .into();
    let stopped: SessionId = apply(&mut state, 2_000, start_session("coder", item))
        .id()
        .into();
    let successor_of = |state: &State, session: SessionId| {
        let graph = state.dependency_graph();
        let sources = Sources {
            state,
            graph: &graph,
            events: &[],
            event: &|_| None,
            own: fixture.devices[0].key(),
            now_ms: 9_000,
            detail_width: 80,
        };
        match InView::take(&sources, &[], Detail::Session(session)) {
            Some(InView::Session { successor, .. }) => successor.map(|successor| successor.id),
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(successor_of(&state, stopped), None, "live");
    apply(
        &mut state,
        3_000,
        end_session(stopped, EndReason::Cancelled),
    );
    assert_eq!(successor_of(&state, stopped), None, "ended, nothing after");
    let next: SessionId = apply(&mut state, 4_000, start_session("coder", item))
        .id()
        .into();
    assert_eq!(successor_of(&state, stopped), Some(next));
    assert_eq!(successor_of(&state, next), None, "the one it went on in");
}

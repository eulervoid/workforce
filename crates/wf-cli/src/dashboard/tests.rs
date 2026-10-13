//! The dashboard drawn into a test terminal, from snapshots made by hand.

use jiff::tz::TimeZone;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use serde_json::json;
use wf_core::id::{CommentId, ItemId, SessionId};
use wf_harness::{Direction, Permission, TranscriptEntry};

use super::app::{App, Effect, Row, Tab};
use super::draw;
use super::snapshot::{
    Counts, InView, ItemRef, Place, SessionInfo, SessionStatus, Snapshot, Waiting,
};

/// 2026-10-14 07:00 UTC.
const NOW_MS: u64 = 1_792_047_600_000;

fn screen(app: &App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| draw::draw(frame, app)).unwrap();
    terminal.backend().to_string()
}

fn item(seed: u8, title: &str) -> ItemRef {
    ItemRef {
        id: ItemId::from_bytes([seed; 32]),
        title: title.to_owned(),
    }
}

fn session(seed: u8, agent: &str, item: ItemRef, minutes: u64) -> SessionInfo {
    SessionInfo {
        id: SessionId::from_bytes([seed; 32]),
        agent: agent.to_owned(),
        role: "work",
        item,
        status: SessionStatus::Live,
        started_ms: NOW_MS - minutes * 60_000,
        place: Place::Here,
    }
}

fn snapshot() -> Snapshot {
    let parse = item(1, "Parse config lazily");
    let spans = item(2, "Fix error spans");
    let archive = item(3, "Archive by category");
    let coder = session(11, "coder", parse.clone(), 12);
    let parked = SessionInfo {
        status: SessionStatus::Parked,
        ..session(12, "coder", spans.clone(), 40)
    };
    let reviewer = SessionInfo {
        place: Place::Elsewhere("laptop".to_owned()),
        ..session(13, "reviewer", archive.clone(), 2)
    };
    Snapshot {
        name: "workforce".to_owned(),
        now_ms: NOW_MS,
        counts: Counts {
            live_sessions: 3,
            open_items: 14,
            blocked_items: 1,
            devices: 2,
        },
        waiting: vec![
            Waiting::Permission {
                session: coder.id,
                since_ms: NOW_MS - 5_000,
                agent: "coder".to_owned(),
                item: Some(parse),
                what: "rm -rf target".to_owned(),
            },
            Waiting::Blocker {
                item: spans,
                comment: CommentId::from_bytes([21; 32]),
                by: "coder".to_owned(),
                text: "Should the span live on ParseError or Location?".to_owned(),
            },
            Waiting::Review {
                item: archive,
                commit: None,
            },
        ],
        working: vec![coder, parked, reviewer],
        flagged: vec!["#q9r1aa \"Watch\" is stranded: a dependency was cancelled".to_owned()],
        in_view: None,
    }
}

fn app() -> App {
    let mut app = App::new(snapshot(), TimeZone::UTC);
    app.resize(60);
    app.logged([
        "06:59  #aibaea  @ada · resolved a91d\n         Fix the formatting of the body, nothing else\n".to_owned(),
        "07:04  #ambqga  @ada/coder/pp74s3 · submitted 3e8a1c → in_review\n".to_owned(),
    ]);
    app
}

fn press(app: &mut App, code: KeyCode) -> Option<Effect> {
    app.key(KeyEvent::from(code))
}

fn entry(seconds: i64, direction: Direction, message: serde_json::Value) -> TranscriptEntry {
    TranscriptEntry {
        at: jiff::Timestamp::from_second(seconds).unwrap(),
        direction,
        message,
    }
}

fn update(seconds: i64, update: serde_json::Value) -> TranscriptEntry {
    entry(
        seconds,
        Direction::FromAgent,
        json!({"method": "session/update", "params": {"update": update}}),
    )
}

/// What the coder of `#parse` has done so far: read a file, and started the tests.
fn coder_transcript() -> Vec<TranscriptEntry> {
    vec![
        entry(
            0,
            Direction::ToAgent,
            json!({"id": 1, "method": "session/prompt", "params": {"prompt": [{"type": "text", "text": "Parse the config lazily."}]}}),
        ),
        update(
            1,
            json!({"sessionUpdate": "tool_call", "toolCallId": "1", "name": "Read", "title": "Read crates/wf-core/src/policy.rs"}),
        ),
        update(
            2,
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "1", "status": "completed"}),
        ),
        update(
            3,
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "The loader now defers parsing until the first access.\n\n"}}),
        ),
        update(
            4,
            json!({"sessionUpdate": "tool_call", "toolCallId": "2", "name": "Bash", "title": "cargo test -p wf-core"}),
        ),
    ]
}

#[test]
fn the_now_tab_shows_what_waits_what_works_and_what_is_flagged() {
    let mut app = app();
    let coder = app.snapshot.working[0].id;
    app.transcribed(coder, &coder_transcript());
    insta::assert_snapshot!(screen(&app, 60, 22));
}

#[test]
fn keys_switch_tabs_and_quit() {
    let mut app = app();
    assert_eq!(press(&mut app, KeyCode::Char('3')), None);
    assert_eq!(app.tab, Tab::Agents);
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.tab, Tab::Log);
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.tab, Tab::Now);
    press(&mut app, KeyCode::BackTab);
    assert_eq!(app.tab, Tab::Log);
    assert_eq!(press(&mut app, KeyCode::Char('q')), Some(Effect::Quit));
}

#[test]
fn the_selection_stays_on_its_row_when_rows_come_and_go() {
    let mut app = app();
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    let review = app.now.selection.get().cloned();
    assert!(matches!(review, Some(Row::Waiting(_))));
    let mut answered = snapshot();
    answered.waiting.remove(0);
    app.refresh(answered);
    assert_eq!(app.now.selection.get().cloned(), review);
    let mut reviewed = snapshot();
    reviewed.waiting.clear();
    app.refresh(reviewed);
    // The row is gone: the selection stays where it was.
    assert_eq!(
        app.now.selection.get(),
        Some(&Row::Session(SessionId::from_bytes([12; 32])))
    );
}

#[test]
fn a_session_opens_on_its_transcript_and_esc_goes_back() {
    let mut app = app();
    let coder = app.snapshot.working[0].clone();
    app.transcribed(coder.id, &coder_transcript());
    // The permission request of the coder is selected first; it opens the coder's session.
    press(&mut app, KeyCode::Enter);
    assert_eq!(
        app.wanted(),
        Some(super::snapshot::Wanted::Session(coder.id))
    );
    let mut snapshot = snapshot();
    snapshot.in_view = Some(InView::Session(coder));
    app.refresh(snapshot);
    insta::assert_snapshot!(screen(&app, 60, 16));
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.wanted(), None);
}

#[test]
fn a_wide_window_shows_the_selection_beside_the_list() {
    let mut app = app();
    app.resize(130);
    let blocked = SessionId::from_bytes([12; 32]);
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(
        app.wanted(),
        Some(super::snapshot::Wanted::Session(blocked))
    );
    let mut snapshot = snapshot();
    snapshot.in_view = Some(InView::Session(snapshot.working[1].clone()));
    app.refresh(snapshot);
    insta::assert_snapshot!(screen(&app, 130, 14));
}

#[test]
fn an_item_detail_shows_the_item_as_wf_item_show_prints_it() {
    let mut app = app();
    let spans = ItemId::from_bytes([2; 32]);
    app.open_item(spans);
    let mut snapshot = snapshot();
    snapshot.in_view = Some(InView::Item {
        id: spans,
        text: "◐ #cicicicicici (task:in_progress) Fix error spans\n\nThe span is lost.\n"
            .to_owned(),
    });
    app.refresh(snapshot);
    insta::assert_snapshot!(screen(&app, 60, 10));
}

#[test]
fn the_log_tab_follows_the_stream_and_filters_it() {
    let mut app = app();
    press(&mut app, KeyCode::Char('4'));
    insta::assert_snapshot!("log", screen(&app, 60, 9));
    press(&mut app, KeyCode::Char('/'));
    for character in "ambq".chars() {
        press(&mut app, KeyCode::Char(character));
    }
    // The filter applies while it is typed, and `q` is text, not quitting.
    assert_eq!(press(&mut app, KeyCode::Char('q')), None);
    press(&mut app, KeyCode::Backspace);
    insta::assert_snapshot!("log_filtered", screen(&app, 60, 9));
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.log_lines().count(), 1);
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.log_lines().count(), 2);
}

#[test]
fn a_permission_request_is_answered_where_it_is_listed() {
    let mut app = app();
    let coder = SessionId::from_bytes([11; 32]);
    assert_eq!(
        press(&mut app, KeyCode::Char('a')),
        Some(Effect::Permit {
            session: coder,
            answer: Permission::Allow
        })
    );
    // On the coder's working session as well.
    press(&mut app, KeyCode::Char('G'));
    press(&mut app, KeyCode::Char('k'));
    press(&mut app, KeyCode::Char('k'));
    press(&mut app, KeyCode::Char('k'));
    assert_eq!(app.now.selection.get(), Some(&Row::Session(coder)));
    assert_eq!(
        press(&mut app, KeyCode::Char('d')),
        Some(Effect::Permit {
            session: coder,
            answer: Permission::Deny
        })
    );
    // Nothing else waits on the parked coder.
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(press(&mut app, KeyCode::Char('a')), None);
}

#[test]
fn a_session_with_a_request_shows_it_under_its_transcript() {
    let mut app = app();
    let coder = app.snapshot.working[0].clone();
    app.transcribed(coder.id, &coder_transcript());
    press(&mut app, KeyCode::Enter);
    let mut snapshot = snapshot();
    snapshot.in_view = Some(InView::Session(coder));
    app.refresh(snapshot);
    insta::assert_snapshot!(screen(&app, 60, 12));
}

#[test]
fn a_blocker_is_answered_in_the_footer() {
    let mut app = app();
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(press(&mut app, KeyCode::Char('r')), None);
    for character in "Keep it on ParseError".chars() {
        // Letters that are keys elsewhere are text here.
        assert_eq!(press(&mut app, KeyCode::Char(character)), None);
    }
    insta::assert_snapshot!(screen(&app, 60, 8));
    assert_eq!(
        press(&mut app, KeyCode::Enter),
        Some(Effect::Resolve {
            item: ItemId::from_bytes([2; 32]),
            blocker: CommentId::from_bytes([21; 32]),
            text: "Keep it on ParseError".to_owned(),
        })
    );
    assert!(app.prompt.is_none());
    // An empty answer resolves nothing.
    press(&mut app, KeyCode::Char('r'));
    assert_eq!(press(&mut app, KeyCode::Enter), None);
}

#[test]
fn a_review_asks_for_the_verdict_and_offers_the_diff_first() {
    let mut app = app();
    let archive = ItemId::from_bytes([3; 32]);
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(press(&mut app, KeyCode::Char('v')), None);
    insta::assert_snapshot!(screen(&app, 60, 8));
    assert_eq!(
        press(&mut app, KeyCode::Char('D')),
        Some(Effect::ShowDiff { item: archive })
    );
    assert!(app.prompt.is_some(), "the verdict is still asked for");
    assert_eq!(
        press(&mut app, KeyCode::Char('a')),
        Some(Effect::Approve { item: archive })
    );
    press(&mut app, KeyCode::Char('v'));
    assert_eq!(
        press(&mut app, KeyCode::Char('r')),
        Some(Effect::RequestChanges { item: archive })
    );
}

#[test]
fn what_an_action_did_shows_until_the_next_key() {
    let mut app = app();
    app.did("allowed: rm -rf target".to_owned());
    assert!(screen(&app, 60, 8).contains("✓ allowed: rm -rf target"));
    app.refresh(snapshot());
    assert!(
        screen(&app, 60, 8).contains("✓ allowed"),
        "a refresh keeps it"
    );
    press(&mut app, KeyCode::Char('j'));
    assert!(!screen(&app, 60, 8).contains("✓ allowed"));
}

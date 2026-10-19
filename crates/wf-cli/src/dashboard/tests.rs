//! The dashboard drawn into a test terminal, from snapshots made by hand.

use jiff::tz::TimeZone;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use serde_json::json;
use wf_core::graph::Readiness;
use wf_core::id::{CommentId, HashId, ItemId, SessionId};
use wf_core::item::{ItemState, Priority};
use wf_core::name::AgentName;
use wf_harness::{Direction, Permission, TranscriptEntry};

use super::app::{
    AgentRow, App, CONFIRM_LASTS_MS, Choice, Effect, Opened, Prompt, Request, Row, STATUS_LASTS_MS,
    Scroll, Split, Starter, Status, Tab, Target, Wheel,
};
use super::draw;
use super::snapshot::{
    AgentInfo, Counts, Detail, InView, ItemListing, Links, MovedBack, Place, SessionInfo, Snapshot,
    Waiting, moves_back,
};
use crate::view::{Flag, ItemGroup, ItemRefView, ItemSummary, SessionStatus};
use crate::{render, theme};
use ratatui::style::{Color, Modifier, Style};
use wf_core::identity::AgentId;
use wf_core::session::EndReason;
use wf_core::session::SessionRole;
use wf_core::stats::{AgentStats, ProjectStats};
use wf_daemon::machine_config::Background;

/// 2026-10-14 07:00 UTC.
const NOW_MS: u64 = 1_792_047_600_000;

fn screen(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| draw::draw(frame, app)).unwrap();
    terminal.backend().to_string()
}

fn item(seed: u8, title: &str) -> ItemRefView {
    ItemRefView {
        id: ItemId::from_bytes([seed; 32]),
        item_type: "task".to_owned(),
        title: title.to_owned(),
        state: ItemState::Open,
        priority: None,
    }
}

fn session(seed: u8, agent: &str, item: ItemRefView, minutes: u64) -> SessionInfo {
    SessionInfo {
        view: session_view(seed, agent, &item, SessionStatus::Live, minutes),
        agent_name: AgentName::new(agent).unwrap(),
        place: Place::Here,
    }
}

/// `info` with another status.
fn with_status(info: SessionInfo, status: SessionStatus) -> SessionInfo {
    SessionInfo {
        view: crate::view::SessionView {
            status,
            ..info.view
        },
        ..info
    }
}

fn accent() -> Color {
    theme::accent().into()
}

fn accent_dimmed() -> Color {
    theme::accent_dimmed().into()
}

/// The style of the label of the tab on screen, focused and not.
fn tab_active() -> Style {
    theme::tab_active().into()
}

fn tab_active_dimmed() -> Style {
    theme::tab_active_dimmed().into()
}

fn snapshot() -> Snapshot {
    let parse = item(1, "Parse config lazily");
    let spans = item(2, "Fix error spans");
    let archive = item(3, "Archive by category");
    let coder = session(11, "coder", parse.clone(), 12);
    let parked = with_status(
        session(12, "coder", spans.clone(), 40),
        SessionStatus::Parked,
    );
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
        flagged: vec![Flag::Stranded {
            item: item(4, "Watch").id,
            title: "Watch".to_owned(),
        }],
        in_view: None,
        items: Vec::new(),
        agents: Vec::new(),
        stats: ProjectStats::default(),
        links: Links::default(),
    }
}

fn app() -> App {
    let mut app = App::new(snapshot(), TimeZone::UTC);
    app.resize(60, 24);
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
    insta::assert_snapshot!(screen(&mut app, 60, 22));
}

#[test]
fn a_flag_about_an_item_opens_the_item() {
    let mut app = app();
    press(&mut app, KeyCode::Char('G'));
    assert!(matches!(app.now.selection.get(), Some(Row::Flag(_))));
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.wanted().detail, Some(Detail::Item(item(4, "Watch").id)));
}

#[test]
fn a_flag_about_no_item_opens_in_full_with_its_command_on_its_own_line() {
    let flag = Flag::UnreadablePolicy {
        error: "unknown field `review_levels` in the policy of 2026-10-14, expected one of `types`, `lanes`, `recovery`".to_owned(),
    };
    let mut flagged = snapshot();
    flagged.flagged = vec![flag.clone()];
    let mut app = App::new(flagged.clone(), TimeZone::UTC);
    app.resize(44, 24);
    press(&mut app, KeyCode::Char('G'));
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.wanted().detail, Some(Detail::Flag(flag.clone())));
    flagged.in_view = Some(InView::Flag {
        text: render::flag_detail_for(&flag, Some(42)),
        flag,
    });
    app.refresh(flagged);
    let shown = screen(&mut app, 44, 24);
    assert!(shown.contains("┃ wf policy edit"), "{shown}");
    insta::assert_snapshot!(shown);
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.wanted().detail, None);
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
    assert_eq!(app.wanted().detail, Some(Detail::Session(coder.id)));
    let mut snapshot = snapshot();
    snapshot.in_view = Some(InView::Session(coder));
    app.refresh(snapshot);
    insta::assert_snapshot!(screen(&mut app, 60, 16));
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.wanted().detail, None);
}

#[test]
fn a_wide_window_shows_the_selection_beside_the_list() {
    let mut app = app();
    app.resize(130, 24);
    let blocked = SessionId::from_bytes([12; 32]);
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.wanted().detail, Some(Detail::Session(blocked)));
    let mut snapshot = snapshot();
    snapshot.in_view = Some(InView::Session(snapshot.working[1].clone()));
    app.refresh(snapshot);
    insta::assert_snapshot!(screen(&mut app, 130, 14));
}

#[test]
fn h_and_l_fold_the_list_or_the_detail_to_a_strip() {
    use ratatui::crossterm::event::KeyModifiers;
    let mut app = app();
    app.resize(130, 24);
    for _ in 0..4 {
        press(&mut app, KeyCode::Char('j'));
    }
    let mut snapshot = snapshot();
    snapshot.in_view = Some(InView::Session(snapshot.working[1].clone()));
    app.refresh(snapshot);
    // The list takes the window; the detail is a strip at the right.
    press(&mut app, KeyCode::Char('H'));
    assert!(app.list_has_keys());
    assert_eq!(app.wanted().detail, None);
    insta::assert_snapshot!("list_only", screen(&mut app, 130, 14));
    // H again goes no further; L brings both back, the list keeping the keys.
    press(&mut app, KeyCode::Char('H'));
    insta::assert_snapshot!("list_only", screen(&mut app, 130, 14));
    press(&mut app, KeyCode::Char('L'));
    assert!(app.list_has_keys());
    insta::assert_snapshot!(
        "a_wide_window_shows_the_selection_beside_the_list",
        screen(&mut app, 130, 14)
    );
    // Shift → gives the detail the window, with the list a strip at the left.
    app.key(KeyEvent::new(KeyCode::Right, KeyModifiers::SHIFT));
    assert!(!app.list_has_keys());
    assert_eq!(app.detail_width(), 127);
    insta::assert_snapshot!("detail_only", screen(&mut app, 130, 14));
    // Esc goes back to the list in the whole window, as in a narrow one.
    press(&mut app, KeyCode::Esc);
    assert!(app.list_has_keys());
    insta::assert_snapshot!("list_only", screen(&mut app, 130, 14));
    // Enter opens the detail in the whole window again; shift ← brings both back.
    press(&mut app, KeyCode::Enter);
    app.key(KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT));
    assert!(!app.list_has_keys());
    insta::assert_snapshot!("both_with_the_detail_keys", screen(&mut app, 130, 14));
}

#[test]
fn a_click_on_the_strip_brings_both_panes_back() {
    let mut app = app();
    app.resize(130, 24);
    press(&mut app, KeyCode::Char('L'));
    screen(&mut app, 130, 14);
    // The list folded away to the left: a click in the detail does nothing to the panes.
    app.click(60, 8);
    assert_eq!(app.split, Split::One);
    app.click(0, 8);
    assert_eq!(app.split, Split::Both);
    assert!(!app.list_has_keys());
    // Neither H nor L does anything on the Log tab, or in a narrow window.
    press(&mut app, KeyCode::Char('4'));
    press(&mut app, KeyCode::Char('H'));
    assert_eq!(app.split, Split::Both);
    press(&mut app, KeyCode::Char('1'));
    app.resize(60, 24);
    press(&mut app, KeyCode::Char('H'));
    assert_eq!(app.split, Split::Both);
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
        session: Some(SessionId::from_bytes([12; 32])),
    });
    app.refresh(snapshot);
    insta::assert_snapshot!(screen(&mut app, 60, 10));
}

#[test]
fn enter_on_what_waits_opens_the_item_with_its_notice_in_view() {
    let held = item(4, "Held merge");
    let what = "75eba435 is ready to merge, but main is checked out with changes; the daemon's comment on the item names them";
    let mut snapshot = snapshot();
    snapshot.waiting = vec![Waiting::Notice {
        item: held.clone(),
        kind: wf_core::inbox::InboxKind::Merge,
        what: what.to_owned(),
    }];
    let mut app = App::new(snapshot.clone(), TimeZone::UTC);
    app.resize(60, 24);
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.wanted().detail, Some(Detail::Item(held.id)));
    assert_eq!(
        app.in_view().map(|opened| opened.scroll),
        Some(Scroll::At(0))
    );
    // As `wf item show` renders it, with a long body under the notice.
    let bar = "\x1b[33m┃\x1b[0m";
    let mut text = format!(
        "◎ #aqcaqcaqcaqc (task:review) Held merge\n\n  {bar} \x1b[1;33mmerge waits\x1b[0m\n"
    );
    for line in textwrap::wrap(what, 50) {
        text.push_str(&format!("  {bar} {line}\n"));
    }
    text.push_str(&format!(
        "  {bar} Waiting to merge: main has changes to src/lib.rs.\n\n"
    ));
    for line in 1..=30 {
        text.push_str(&format!("  body line {line}\n"));
    }
    snapshot.in_view = Some(InView::Item {
        id: held.id,
        text,
        session: None,
    });
    app.refresh(snapshot);
    let shown = screen(&mut app, 60, 16);
    assert!(shown.contains("┃ merge waits"), "{shown}");
    assert!(shown.contains("┃ names them"), "{shown}");
    assert!(shown.contains("changes to src/lib.rs."), "{shown}");
}

#[test]
fn scrolling_stops_at_the_end_of_a_detail() {
    let mut app = app();
    let spans = ItemId::from_bytes([2; 32]);
    app.open_item(spans);
    let mut snapshot = snapshot();
    let text: String = (1..=12).map(|line| format!("line {line}\n")).collect();
    snapshot.in_view = Some(InView::Item {
        id: spans,
        text,
        session: None,
    });
    app.refresh(snapshot);
    // Drawn after each key, as the dashboard is.
    for _ in 0..20 {
        press(&mut app, KeyCode::Char('j'));
        screen(&mut app, 60, 10);
    }
    let at_end = screen(&mut app, 60, 10);
    assert!(at_end.contains("line 12"));
    press(&mut app, KeyCode::Char('k'));
    assert_ne!(screen(&mut app, 60, 10), at_end);
    press(&mut app, KeyCode::PageDown);
    assert_eq!(screen(&mut app, 60, 10), at_end);
    // Above the top of the log, which starts at its end.
    press(&mut app, KeyCode::Char('4'));
    let at_end = screen(&mut app, 60, 8);
    for _ in 0..10 {
        press(&mut app, KeyCode::Char('k'));
        screen(&mut app, 60, 8);
    }
    let at_top = screen(&mut app, 60, 8);
    assert_ne!(at_top, at_end);
    press(&mut app, KeyCode::PageUp);
    assert_eq!(screen(&mut app, 60, 8), at_top);
    press(&mut app, KeyCode::Char('j'));
    assert_ne!(screen(&mut app, 60, 8), at_top);
    // At its end, the log follows what is added.
    press(&mut app, KeyCode::PageDown);
    screen(&mut app, 60, 8);
    app.logged(["07:09  #ambqga  @ada · resolved b72c\n".to_owned()]);
    assert!(screen(&mut app, 60, 8).contains("b72c"));
}

#[test]
fn scrolled_up_text_holds_still_and_follows_again_at_its_end() {
    let mut app = app();
    press(&mut app, KeyCode::Char('4'));
    let lines = |from: usize, to: usize| -> Vec<String> {
        (from..=to)
            .map(|line| format!("07:{line:02}  @ada · line {line}\n"))
            .collect()
    };
    app.logged(lines(10, 30));
    screen(&mut app, 60, 10);
    press(&mut app, KeyCode::Char('k'));
    press(&mut app, KeyCode::Char('k'));
    // The body, without the latest event under it, nor the position at its right end, which
    // moves up as the text grows.
    let body = |app: &mut App| -> Vec<String> {
        screen(app, 60, 10)
            .lines()
            .skip(3)
            .take(4)
            .map(|line| line.chars().take(50).collect())
            .collect()
    };
    let scrolled = body(&mut app);
    // What is added below does not move what is being read.
    app.logged(lines(31, 35));
    assert_eq!(body(&mut app), scrolled);
    // Back at the end, the log follows again.
    for _ in 0..10 {
        press(&mut app, KeyCode::Char('j'));
        screen(&mut app, 60, 10);
    }
    app.logged(lines(36, 36));
    assert!(screen(&mut app, 60, 10).contains("line 36"));
}

#[test]
fn the_log_tab_follows_the_stream_and_filters_it() {
    let mut app = app();
    press(&mut app, KeyCode::Char('4'));
    insta::assert_snapshot!("log", screen(&mut app, 60, 9));
    press(&mut app, KeyCode::Char('/'));
    for character in "ambq".chars() {
        press(&mut app, KeyCode::Char(character));
    }
    // The filter applies while it is typed, and `q` is text, not quitting.
    assert_eq!(press(&mut app, KeyCode::Char('q')), None);
    press(&mut app, KeyCode::Backspace);
    insta::assert_snapshot!("log_filtered", screen(&mut app, 60, 9));
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
            request: coders_request(),
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
            request: coders_request(),
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
    insta::assert_snapshot!(screen(&mut app, 60, 12));
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
    insta::assert_snapshot!(screen(&mut app, 60, 8));
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
fn pasted_text_goes_into_the_prompt_with_line_breaks_as_spaces() {
    let mut app = app();
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('r'));
    assert!(app.paste("a\nx\r\ny"));
    // Nothing of it ran as keys: the prompt is open and nothing was answered.
    assert!(app.prompt.is_some());
    assert_eq!(
        press(&mut app, KeyCode::Enter),
        Some(Effect::Resolve {
            item: ItemId::from_bytes([2; 32]),
            blocker: CommentId::from_bytes([21; 32]),
            text: "a x y".to_owned(),
        })
    );
}

#[test]
fn pasted_text_without_a_prompt_is_ignored() {
    let mut app = app();
    assert!(!app.paste("a\nx\ny\n"));
    assert!(app.prompt.is_none());
}

#[test]
fn a_prompt_scrolls_by_cell_width_and_keeps_the_cursor_in_place() {
    let mut app = app();
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('r'));
    app.paste("日本語日本語日本語日本語日本語日本語日本語日本語日本語日本語end");
    let screen = screen(&mut app, 40, 8);
    let footer = screen
        .lines()
        .rev()
        .find(|line| line.contains("end"))
        .unwrap();
    // The end shows whole, and its start is cut at a character, not in the middle of one.
    // (The test backend prints a blank after each wide character.)
    let shown: String = footer.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(shown.contains("日本語日本語日本語end"), "{footer}");
}

#[test]
fn ctrl_g_carries_what_is_typed_into_the_editor() {
    use ratatui::crossterm::event::KeyModifiers;
    let ctrl_g = KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL);
    let mut app = app();
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('r'));
    for character in "Keep it".chars() {
        press(&mut app, KeyCode::Char(character));
    }
    // A plain g is text.
    assert_eq!(press(&mut app, KeyCode::Char('g')), None);
    assert_eq!(
        app.key(ctrl_g),
        Some(Effect::ResolveInEditor {
            item: ItemId::from_bytes([2; 32]),
            blocker: CommentId::from_bytes([21; 32]),
            text: "Keep itg".to_owned(),
        })
    );
    assert!(app.prompt.is_none());
    assert_eq!(
        press(&mut app, KeyCode::Char('R')),
        Some(Effect::ResolveInEditor {
            item: ItemId::from_bytes([2; 32]),
            blocker: CommentId::from_bytes([21; 32]),
            text: String::new(),
        }),
        "R starts from nothing"
    );
    press(&mut app, KeyCode::Char('c'));
    for character in "Longer than a line".chars() {
        press(&mut app, KeyCode::Char(character));
    }
    insta::assert_snapshot!(screen(&mut app, 80, 8));
    assert_eq!(
        app.key(ctrl_g),
        Some(Effect::CommentInEditor {
            item: ItemId::from_bytes([2; 32]),
            text: "Longer than a line".to_owned(),
        })
    );
    assert!(app.prompt.is_none());
}

#[test]
fn a_review_asks_for_the_verdict_and_offers_the_diff_first() {
    let mut app = app();
    let archive = ItemId::from_bytes([3; 32]);
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(press(&mut app, KeyCode::Char('v')), None);
    insta::assert_snapshot!(screen(&mut app, 60, 8));
    assert_eq!(press(&mut app, KeyCode::Char('D')), None);
    assert!(app.prompt.is_none(), "the diff takes the keys");
    assert_eq!(app.wanted().detail, Some(Detail::Diff(archive)));
    // The verdict is asked for again from the diff, where `v` selects lines.
    press(&mut app, KeyCode::Char('R'));
    assert_eq!(
        press(&mut app, KeyCode::Char('a')),
        Some(Effect::Approve {
            item: archive,
            reviewed: None,
            comments: Vec::new()
        })
    );
    press(&mut app, KeyCode::Char('R'));
    assert_eq!(
        press(&mut app, KeyCode::Char('r')),
        Some(Effect::RequestChanges {
            item: archive,
            reviewed: None,
            comments: Vec::new()
        })
    );
}

#[test]
fn what_an_action_did_shows_until_the_next_key() {
    let mut app = app();
    app.did("allowed: rm -rf target".to_owned(), NOW_MS);
    assert!(screen(&mut app, 60, 8).contains("✓ allowed: rm -rf target"));
    app.refresh(snapshot());
    assert!(
        screen(&mut app, 60, 8).contains("✓ allowed"),
        "a refresh keeps it"
    );
    press(&mut app, KeyCode::Char('j'));
    assert!(!screen(&mut app, 60, 8).contains("✓ allowed"));
}

#[test]
fn an_action_error_goes_away_after_its_time() {
    let mut app = app();
    app.action_failed(&anyhow::anyhow!("the daemon refused"), NOW_MS);
    let later = |ms| Snapshot {
        now_ms: NOW_MS + ms,
        ..snapshot()
    };
    app.refresh(later(STATUS_LASTS_MS - 1));
    assert!(screen(&mut app, 60, 8).contains("error: the daemon refused"));
    app.refresh(later(STATUS_LASTS_MS));
    assert!(!screen(&mut app, 60, 8).contains("error"));

    app.did("allowed: rm -rf target".to_owned(), NOW_MS);
    app.refresh(later(STATUS_LASTS_MS));
    assert!(!screen(&mut app, 60, 8).contains("✓ allowed"));
}

fn item_row(seed: u8, title: &str, state: ItemState) -> ItemSummary {
    ItemSummary {
        id: ItemId::from_bytes([seed; 32]),
        item_type: "task".to_owned(),
        title: title.to_owned(),
        state,
        priority: None,
        readiness: None,
        blocked_by: Vec::new(),
        blocking: Vec::new(),
        labels: Vec::new(),
        lane: None,
        steps: None,
        archived: false,
    }
}

/// The snapshot while the Items tab is on screen.
fn with_items() -> Snapshot {
    let mut snapshot = snapshot();
    snapshot.items = vec![
        ItemGroup {
            group: "open".to_owned(),
            items: vec![
                ItemSummary {
                    priority: Some(Priority::P1),
                    readiness: Some(Readiness::Ready),
                    ..item_row(4, "Dashboard", ItemState::Open)
                },
                ItemSummary {
                    readiness: Some(Readiness::Blocked),
                    ..item_row(5, "Session subscription", ItemState::Open)
                },
            ],
        },
        ItemGroup {
            group: "active".to_owned(),
            items: vec![ItemSummary {
                steps: Some(crate::view::StepsView { done: 2, total: 5 }),
                priority: Some(Priority::P0),
                ..item_row(1, "Parse config lazily", ItemState::Active)
            }],
        },
    ];
    snapshot
}

#[test]
fn the_items_tab_lists_items_by_state_and_filters_them() {
    let mut app = app();
    press(&mut app, KeyCode::Char('2'));
    assert_eq!(
        app.wanted().items.map(|listing| listing.grouping),
        Some(crate::cli::ItemGrouping::State)
    );
    app.refresh(with_items());
    insta::assert_snapshot!("items", screen(&mut app, 60, 14));
    press(&mut app, KeyCode::Char('F'));
    assert_eq!(app.item_groups().len(), 1, "only the ready item");
    press(&mut app, KeyCode::Char('F'));
    press(&mut app, KeyCode::Char('F'));
    press(&mut app, KeyCode::Char('F'));
    press(&mut app, KeyCode::Char('/'));
    for character in "parse".chars() {
        press(&mut app, KeyCode::Char(character));
    }
    press(&mut app, KeyCode::Enter);
    let titles: Vec<&str> = app
        .item_groups()
        .into_iter()
        .flat_map(|(_, items)| items.into_iter().map(|item| item.title.as_str()))
        .collect();
    assert_eq!(titles, ["Parse config lazily"]);
    press(&mut app, KeyCode::Char('o'));
    assert_eq!(
        app.wanted().items.map(|listing| listing.grouping),
        Some(crate::cli::ItemGrouping::Lane)
    );
}

#[test]
fn the_items_tab_cycles_its_order_and_shows_it_in_its_title() {
    use crate::cli::ItemOrder;
    let mut app = app();
    press(&mut app, KeyCode::Char('2'));
    app.refresh(with_items());
    let order = |app: &App| app.wanted().items.map(|listing| listing.order);
    assert_eq!(order(&app), Some(ItemOrder::Priority));
    assert!(screen(&mut app, 60, 14).contains("by state · sorted by priority"));
    press(&mut app, KeyCode::Char('O'));
    assert_eq!(order(&app), Some(ItemOrder::Created));
    assert!(screen(&mut app, 60, 14).contains("sorted by creation"));
    press(&mut app, KeyCode::Char('O'));
    assert_eq!(order(&app), Some(ItemOrder::Merged));
    assert!(screen(&mut app, 60, 14).contains("sorted by merge"));
    press(&mut app, KeyCode::Char('O'));
    assert_eq!(order(&app), Some(ItemOrder::Priority));
}

#[test]
fn the_items_filter_matches_the_start_of_an_id_with_or_without_a_hash() {
    let mut app = app();
    press(&mut app, KeyCode::Char('2'));
    app.refresh(with_items());
    for filter in ["aeaqca", "#AEAQ"] {
        press(&mut app, KeyCode::Char('/'));
        for character in filter.chars() {
            press(&mut app, KeyCode::Char(character));
        }
        press(&mut app, KeyCode::Enter);
        let titles: Vec<&str> = app
            .item_groups()
            .into_iter()
            .flat_map(|(_, items)| items.into_iter().map(|item| item.title.as_str()))
            .collect();
        assert_eq!(titles, ["Parse config lazily"], "filtered by {filter}");
        press(&mut app, KeyCode::Esc);
    }
}

#[test]
fn an_item_row_shows_its_priority_in_the_style_of_wf_item_list() {
    let mut app = app();
    press(&mut app, KeyCode::Char('2'));
    app.refresh(with_items());
    let text = screen(&mut app, 60, 14);
    let (y, line) = text
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains("Parse config lazily"))
        .unwrap();
    let column = |text: &str| u16::try_from(line[1..line.find(text).unwrap()].chars().count());
    let y = u16::try_from(y).unwrap();
    let look = |style: Style| (style.fg, style.add_modifier);
    let priority = style_at(&mut app, 60, 14, column("P0").unwrap(), y);
    let p0 = Style::from(theme::priority(Priority::P0));
    assert_eq!(look(priority), look(p0));
    let steps = style_at(&mut app, 60, 14, column("2/5").unwrap(), y);
    assert_eq!(look(steps), (Some(Color::Reset), Modifier::DIM));
}

#[test]
fn an_item_opens_on_its_detail_and_enter_goes_on_to_its_session() {
    let mut app = app();
    press(&mut app, KeyCode::Char('2'));
    app.refresh(with_items());
    press(&mut app, KeyCode::Char('G'));
    press(&mut app, KeyCode::Enter);
    let parse = ItemId::from_bytes([1; 32]);
    assert_eq!(app.wanted().detail, Some(Detail::Item(parse)));
    let coder = SessionId::from_bytes([11; 32]);
    let mut snapshot = with_items();
    snapshot.in_view = Some(InView::Item {
        id: parse,
        text: "◐ #aeaqca (task:in_progress) Parse config lazily\n".to_owned(),
        session: Some(coder),
    });
    app.refresh(snapshot);
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.wanted().detail, Some(Detail::Session(coder)));
    // Its tab keeps its own details: the Now tab is as it was.
    press(&mut app, KeyCode::Char('1'));
    assert_eq!(app.wanted().detail, None);
    press(&mut app, KeyCode::Char('2'));
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.wanted().detail, None);
}

#[test]
fn a_move_of_the_selection_takes_in_only_the_new_detail() {
    let mut app = app();
    app.resize(130, 24);
    press(&mut app, KeyCode::Char('2'));
    app.refresh(with_items());
    press(&mut app, KeyCode::Char('j'));
    let subscription = ItemId::from_bytes([5; 32]);
    assert_eq!(app.wanted().detail, Some(Detail::Item(subscription)));
    let in_view = InView::Item {
        id: subscription,
        text: "#aukcuq Session subscription\n".to_owned(),
        session: None,
    };
    app.refresh_detail(Some(in_view.clone()));
    assert_eq!(app.snapshot.in_view, Some(in_view));
    let titles: Vec<&str> = app
        .item_groups()
        .into_iter()
        .flat_map(|(_, items)| items.into_iter().map(|item| item.title.as_str()))
        .collect();
    assert_eq!(
        titles,
        ["Dashboard", "Session subscription", "Parse config lazily"]
    );
    assert_eq!(app.snapshot.flagged, snapshot().flagged);
    assert_eq!(app.snapshot.waiting, snapshot().waiting);
    assert_eq!(app.snapshot.working, snapshot().working);
    assert_eq!(app.wanted().detail, Some(Detail::Item(subscription)));
}

/// The Items tab with its last item, scrolled two lines down from where it opened.
fn scrolled_item(width: u16) -> (App, Opened) {
    let mut app = app();
    app.resize(width, 24);
    press(&mut app, KeyCode::Char('2'));
    let parse = ItemId::from_bytes([1; 32]);
    let mut snapshot = with_items();
    snapshot.in_view = Some(InView::Item {
        id: parse,
        text: (1..=40).map(|line| format!("line {line}\n")).collect(),
        session: None,
    });
    app.refresh(snapshot);
    press(&mut app, KeyCode::Char('G'));
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    let scrolled = Opened {
        detail: Detail::Item(parse),
        scroll: Scroll::At(2),
    };
    assert_eq!(app.in_view(), Some(scrolled.clone()));
    (app, scrolled)
}

#[test]
fn going_back_to_the_list_keeps_the_item_where_it_was_scrolled_to() {
    let (mut app, scrolled) = scrolled_item(130);
    press(&mut app, KeyCode::Char('h'));
    assert!(app.list_has_keys());
    assert_eq!(app.in_view(), Some(scrolled.clone()));
    press(&mut app, KeyCode::Char('l'));
    assert!(!app.list_has_keys());
    assert_eq!(app.in_view(), Some(scrolled.clone()));
    // Away from the item and back, it starts at its top.
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('k'));
    press(&mut app, KeyCode::Char('j'));
    let at_top = Opened {
        scroll: Scroll::At(0),
        ..scrolled
    };
    assert_eq!(app.in_view(), Some(at_top.clone()));
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.in_view(), Some(at_top));
}

#[test]
fn in_a_narrow_window_the_list_comes_back_to_the_item_where_it_was() {
    let (mut app, scrolled) = scrolled_item(60);
    press(&mut app, KeyCode::Left);
    assert!(app.list_has_keys());
    assert_eq!(app.in_view(), None);
    assert_eq!(app.path(), Vec::new());
    press(&mut app, KeyCode::Right);
    assert_eq!(app.in_view(), Some(scrolled.clone()));
    // Away from the item and back, it starts at its top.
    press(&mut app, KeyCode::Char('h'));
    press(&mut app, KeyCode::Char('k'));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('l'));
    assert_eq!(
        app.in_view(),
        Some(Opened {
            scroll: Scroll::At(0),
            ..scrolled
        })
    );
}

#[test]
fn with_one_pane_the_list_comes_back_to_the_item_where_it_was() {
    let (mut app, scrolled) = scrolled_item(130);
    press(&mut app, KeyCode::Char('L'));
    assert_eq!(app.split, Split::One);
    press(&mut app, KeyCode::Char('h'));
    assert!(app.list_has_keys());
    assert_eq!(app.in_view(), None);
    press(&mut app, KeyCode::Char('l'));
    assert_eq!(app.in_view(), Some(scrolled));
}

#[test]
fn going_back_from_a_session_opened_on_an_item_leaves_the_item_as_it_was() {
    let mut app = app();
    press(&mut app, KeyCode::Char('2'));
    let parse = ItemId::from_bytes([1; 32]);
    let coder = SessionId::from_bytes([11; 32]);
    let mut snapshot = with_items();
    snapshot.in_view = Some(InView::Item {
        id: parse,
        text: (1..=40).map(|line| format!("line {line}\n")).collect(),
        session: Some(coder),
    });
    app.refresh(snapshot);
    press(&mut app, KeyCode::Char('G'));
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.path(), [Detail::Item(parse), Detail::Session(coder)]);
    press(&mut app, KeyCode::Esc);
    assert!(!app.list_has_keys());
    assert_eq!(
        app.in_view(),
        Some(Opened {
            detail: Detail::Item(parse),
            scroll: Scroll::At(1),
        })
    );
}

#[test]
fn item_keys_act_on_the_selected_item() {
    let mut app = app();
    press(&mut app, KeyCode::Char('2'));
    app.refresh(with_items());
    let dashboard = ItemId::from_bytes([4; 32]);
    assert_eq!(
        press(&mut app, KeyCode::Char('s')),
        Some(Effect::ChooseStart { item: dashboard })
    );
    app.choose(Choice::Start {
        item: dashboard,
        starters: vec![
            Starter::Agent(AgentName::new("coder").unwrap()),
            Starter::Agent(AgentName::new("reviewer").unwrap()),
        ],
        index: 0,
    });
    insta::assert_snapshot!("choose_agent", screen(&mut app, 60, 12));
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(
        press(&mut app, KeyCode::Enter),
        Some(Effect::StartSession {
            item: dashboard,
            agent: AgentName::new("reviewer").unwrap()
        })
    );
    assert_eq!(
        press(&mut app, KeyCode::Char('m')),
        Some(Effect::ChooseState { item: dashboard })
    );
    app.choose(Choice::State {
        item: dashboard,
        states: vec![ItemState::Draft, ItemState::Done, ItemState::Cancelled],
        index: 0,
    });
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(
        press(&mut app, KeyCode::Enter),
        Some(Effect::Move {
            item: dashboard,
            state: ItemState::Done
        })
    );
    app.choose(Choice::Start {
        item: dashboard,
        starters: vec![
            Starter::Dispatcher,
            Starter::Agent(AgentName::new("coder").unwrap()),
        ],
        index: 0,
    });
    insta::assert_snapshot!("choose_release", screen(&mut app, 60, 12));
    assert_eq!(
        press(&mut app, KeyCode::Enter),
        Some(Effect::Move {
            item: dashboard,
            state: ItemState::Open
        }),
        "s enter releases a draft"
    );
    assert_eq!(press(&mut app, KeyCode::Char('c')), None);
    for character in "Start with the Now tab".chars() {
        press(&mut app, KeyCode::Char(character));
    }
    assert_eq!(
        press(&mut app, KeyCode::Enter),
        Some(Effect::Comment {
            item: dashboard,
            text: "Start with the Now tab".to_owned()
        })
    );
    assert_eq!(
        press(&mut app, KeyCode::Char('e')),
        Some(Effect::Edit { item: dashboard })
    );
    // An item has no session to stop.
    assert_eq!(press(&mut app, KeyCode::Char('x')), None);
    assert!(app.prompt.is_none());
}

#[test]
fn a_session_is_stopped_only_when_confirmed() {
    let mut app = app();
    let coder = SessionId::from_bytes([11; 32]);
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.now.selection.get(), Some(&Row::Session(coder)));
    assert_eq!(press(&mut app, KeyCode::Char('x')), None);
    insta::assert_snapshot!("stop", screen(&mut app, 60, 6));
    assert_eq!(press(&mut app, KeyCode::Char('n')), None);
    assert!(app.prompt.is_none());
    press(&mut app, KeyCode::Char('x'));
    assert_eq!(
        press(&mut app, KeyCode::Char('y')),
        Some(Effect::StopSession { session: coder })
    );
}

#[test]
fn the_keys_show_on_a_question_mark() {
    let mut app = app();
    press(&mut app, KeyCode::Char('?'));
    assert!(matches!(app.prompt, Some(Prompt::Keys)));
    insta::assert_snapshot!("keys", screen(&mut app, 60, 18));
    // Any key closes it, and does nothing else.
    assert_eq!(press(&mut app, KeyCode::Char('q')), None);
    assert!(app.prompt.is_none());
}

fn agent_view(name: &str, harness: &str, model: Option<&str>) -> crate::view::AgentView {
    crate::view::AgentView {
        id: format!("@ada/{name}"),
        harness: harness.to_owned(),
        harness_startable_here: true,
        model: model.map(str::to_owned),
        capabilities: Vec::new(),
        lanes: Vec::new(),
        start: "manual".to_owned(),
        permissions: "ask".to_owned(),
        nudges: 0,
        park_timeout: "1h".to_owned(),
        max_duration: None,
        instructions: None,
    }
}

/// The snapshot while the Agents tab is on screen.
fn with_agents() -> Snapshot {
    let mut snapshot = snapshot();
    let principal = wf_core::testing::Fixture::new(1).founder.key();
    let ended = with_status(
        session(14, "coder", item(4, "Dashboard"), 300),
        SessionStatus::Ended {
            reason: EndReason::Submitted { commit: None },
            at: crate::view::timestamp_ms(NOW_MS - 2 * 60 * 60_000),
        },
    );
    snapshot.agents = vec![
        AgentInfo {
            id: AgentId {
                principal,
                name: AgentName::new("coder").unwrap(),
            },
            view: agent_view("coder", "claude-code", Some("opus")),
            capacity: Some(2),
            stats: AgentStats {
                sessions: 3,
                live: 2,
                finished: 1,
                failed: 0,
                median_duration_ms: Some(5 * 60 * 60_000),
            },
            sessions: vec![
                snapshot.working[0].clone(),
                snapshot.working[1].clone(),
                ended,
            ],
        },
        AgentInfo {
            id: AgentId {
                principal,
                name: AgentName::new("reviewer").unwrap(),
            },
            view: agent_view("reviewer", "claude-code", None),
            capacity: None,
            stats: AgentStats {
                live: 1,
                sessions: 1,
                ..AgentStats::default()
            },
            sessions: vec![snapshot.working[2].clone()],
        },
    ];
    snapshot.stats = ProjectStats {
        done_lately: 5,
        median_lead_ms: Some(3 * 60 * 60_000),
        attempts: 9,
        failed_attempts: 2,
        merged: 4,
        change_requests: 2,
    };
    snapshot
}

#[test]
fn the_agents_tab_lists_each_agent_with_its_sessions() {
    let mut app = app();
    press(&mut app, KeyCode::Char('3'));
    assert!(app.wanted().agents);
    app.refresh(with_agents());
    insta::assert_snapshot!("agents", screen(&mut app, 60, 16));
    // An agent opens on its definition, a session on its transcript.
    press(&mut app, KeyCode::Enter);
    assert!(
        matches!(app.wanted().detail, Some(Detail::Agent(agent)) if agent.name.as_str() == "coder")
    );
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(
        app.agents.selection.get(),
        Some(&AgentRow::Session(SessionId::from_bytes([11; 32])))
    );
    // The keys of a session work here too.
    assert_eq!(
        press(&mut app, KeyCode::Char('a')),
        Some(Effect::Permit {
            request: coders_request(),
            answer: Permission::Allow
        })
    );
    press(&mut app, KeyCode::Char('G'));
    press(&mut app, KeyCode::Char('k'));
    // A session that ended cannot be stopped.
    assert_eq!(press(&mut app, KeyCode::Char('x')), None);
    assert!(app.prompt.is_none());
}

#[test]
fn the_wheel_moves_what_is_under_the_pointer() {
    let mut app = app();
    app.resize(130, 24);
    let item = Detail::Item(ItemId::from_bytes([4; 32]));
    let blocked = Detail::Item(ItemId::from_bytes([5; 32]));
    // Over the detail beside the list, the wheel scrolls it and leaves the keys with the list.
    app.wheel(100, Wheel::Down);
    assert_eq!(
        app.stack(),
        &vec![Opened {
            detail: Detail::Session(SessionId::from_bytes([11; 32])),
            scroll: Scroll::END,
        }]
    );
    assert!(app.list_has_keys());
    press(&mut app, KeyCode::Char('2'));
    app.refresh(with_items());
    app.wheel(100, Wheel::Down);
    app.wheel(100, Wheel::Down);
    assert_eq!(
        app.stack(),
        &vec![Opened {
            detail: item.clone(),
            scroll: Scroll::At(6),
        }]
    );
    assert!(app.list_has_keys());
    // Over the list, the wheel scrolls it; the keys, the selection and the detail stay.
    app.wheel(10, Wheel::Down);
    assert!(app.list_has_keys());
    assert_eq!(app.stack().len(), 1);
    assert_eq!(app.wanted().detail, Some(item.clone()));
    // The keys move the selection, whose detail takes the place of the one opened.
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.stack().len(), 0);
    assert_eq!(app.wanted().detail, Some(blocked));
    press(&mut app, KeyCode::Char('k'));
    assert_eq!(app.wanted().detail, Some(item.clone()));
    // Over the detail of a flag, the wheel scrolls the detail it opens.
    press(&mut app, KeyCode::Char('1'));
    app.refresh(snapshot());
    press(&mut app, KeyCode::Char('G'));
    assert!(matches!(app.now.selection.get(), Some(Row::Flag(_))));
    app.wheel(100, Wheel::Down);
    assert_eq!(app.stack().len(), 1);
    assert_eq!(app.wanted().detail, Some(item.clone()));
    press(&mut app, KeyCode::Esc);
    assert!(app.list_has_keys());
    press(&mut app, KeyCode::Char('2'));
    app.refresh(with_items());
    // In a narrow window the wheel moves what is on screen, wherever the pointer is.
    app.resize(60, 24);
    app.wheel(50, Wheel::Down);
    assert!(app.list_has_keys());
    press(&mut app, KeyCode::Char('k'));
    press(&mut app, KeyCode::Enter);
    app.wheel(10, Wheel::Down);
    assert_eq!(
        app.stack().last().map(|opened| opened.scroll),
        Some(Scroll::At(3))
    );
}

/// The Items tab in a wide window too short for its thirty items, the first selected.
fn long_item_list() -> App {
    let mut app = app();
    app.resize(130, 14);
    press(&mut app, KeyCode::Char('2'));
    let mut snapshot = snapshot();
    snapshot.items = vec![ItemGroup {
        group: "open".to_owned(),
        items: (1..=30)
            .map(|seed| item_row(seed, &format!("Item {seed}"), ItemState::Open))
            .collect(),
    }];
    app.refresh(snapshot);
    app
}

fn selected_item(app: &App) -> Option<ItemId> {
    app.items.list.selection.get().copied()
}

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
        app.wanted().items,
        Some(ItemListing {
            grouping: crate::cli::ItemGrouping::State,
            order: crate::cli::ItemOrder::Priority,
        })
    );
    assert!(app.prompt.is_none());
    assert_eq!(app.item_groups().len(), with_items().items.len());
    press(&mut app, KeyCode::Char('?'));
    assert!(matches!(app.prompt, Some(Prompt::Keys)));
    press(&mut app, KeyCode::Esc);
    assert_eq!(press(&mut app, KeyCode::Char('q')), Some(Effect::Quit));
}

/// The column and the row `text` starts at once `app` is drawn.
fn find(app: &mut App, width: u16, height: u16, text: &str) -> (u16, u16) {
    let screen = screen(app, width, height);
    screen
        .lines()
        .enumerate()
        .find_map(|(row, line)| {
            // The backend quotes each line, which the column leaves out.
            let before = &line[1..line.find(text)?];
            let column = u16::try_from(before.chars().count()).unwrap();
            Some((column, u16::try_from(row).unwrap()))
        })
        .unwrap_or_else(|| panic!("{text:?} is not on screen:\n{screen}"))
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

/// The style of the cell at `x`, `y` once `app` is drawn.
fn style_at(app: &mut App, width: u16, height: u16, x: u16, y: u16) -> ratatui::style::Style {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| draw::draw(frame, app)).unwrap();
    terminal.backend().buffer()[(x, y)].style()
}

#[test]
fn the_part_with_the_keys_has_the_accent_and_the_other_the_dimmed_accent() {
    let mut app = app();
    app.resize(130, 24);
    let blocked = SessionId::from_bytes([12; 32]);
    for _ in 0..4 {
        press(&mut app, KeyCode::Char('j'));
    }
    let mut snapshot = snapshot();
    snapshot.in_view = Some(InView::Session(snapshot.working[1].clone()));
    app.refresh(snapshot.clone());
    // The parked coder is selected on line 9; the breadcrumb is on line 3, right of the list.
    let row = 9;
    let (crumb_x, crumb_y) = find(&mut app, 130, 14, "coder · work");
    assert_eq!(app.wanted().detail, Some(Detail::Session(blocked)));
    assert_eq!(bg_at(&mut app, 130, 14, 30, row), Some(accent()));
    assert_eq!(
        bg_at(&mut app, 130, 14, crumb_x, crumb_y),
        Some(accent_dimmed())
    );
    // In the detail, the list keeps the selection in the dimmed accent, and the breadcrumb has
    // the accent.
    press(&mut app, KeyCode::Char('l'));
    app.refresh(snapshot);
    assert_eq!(bg_at(&mut app, 130, 14, 30, row), Some(accent_dimmed()));
    assert_eq!(bg_at(&mut app, 130, 14, crumb_x, crumb_y), Some(accent()));
}

/// The background of the cell at `x`, `y` once `app` is drawn.
fn bg_at(app: &mut App, width: u16, height: u16, x: u16, y: u16) -> Option<Color> {
    style_at(app, width, height, x, y).bg
}

/// The text of the cell at `x`, `y` once `app` is drawn.
fn symbol_at(app: &mut App, width: u16, height: u16, x: u16, y: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| draw::draw(frame, app)).unwrap();
    terminal.backend().buffer()[(x, y)].symbol().to_owned()
}

#[test]
fn the_bar_and_the_background_cover_every_line_of_the_selection() {
    let mut app = app();
    press(&mut app, KeyCode::Char('j'));
    let (_, row) = find(&mut app, 60, 22, "blocker from");
    for line in [row, row + 1] {
        assert_eq!(symbol_at(&mut app, 60, 22, 0, line), "▎");
        // The bar is white, on the background of the row, with no gap to the left edge.
        assert_eq!(
            style_at(&mut app, 60, 22, 0, line).fg,
            Style::from(theme::on_accent()).fg
        );
        assert_eq!(bg_at(&mut app, 60, 22, 0, line), Some(accent()));
        assert_eq!(bg_at(&mut app, 60, 22, 10, line), Some(accent()));
    }
    // The line after the entry is the next one's.
    assert_eq!(symbol_at(&mut app, 60, 22, 0, row + 2), " ");
    assert_ne!(bg_at(&mut app, 60, 22, 10, row + 2), Some(accent()));
}

#[test]
fn the_accent_goes_to_the_breadcrumb_on_enter_and_back_on_esc() {
    let mut app = app();
    let coder = app.snapshot.working[0].clone();
    let (_, row) = find(&mut app, 60, 16, "wants:");
    assert_eq!(bg_at(&mut app, 60, 16, 10, row), Some(accent()));
    press(&mut app, KeyCode::Enter);
    let mut snapshot = snapshot();
    snapshot.in_view = Some(InView::Session(coder));
    app.refresh(snapshot);
    let (column, crumb_y) = find(&mut app, 60, 16, "coder · work");
    assert_eq!(bg_at(&mut app, 60, 16, column, crumb_y), Some(accent()));
    press(&mut app, KeyCode::Esc);
    assert_eq!(bg_at(&mut app, 60, 16, 10, row), Some(accent()));
}

/// Asserts that the highlight `text` on screen has half blocks around it in `background`, on the
/// background of the line, and `background` itself under its text.
fn assert_ends(app: &mut App, width: u16, height: u16, text: &str, background: Color) {
    let (column, row) = find(app, width, height, &format!("▐{text}▌"));
    let end = column + text.chars().count() as u16 + 1;
    for (x, symbol) in [(column, "▐"), (end, "▌")] {
        assert_eq!(symbol_at(app, width, height, x, row), symbol);
        let style = style_at(app, width, height, x, row);
        assert_eq!((style.fg, style.bg), (Some(background), Some(Color::Reset)));
    }
    assert_eq!(bg_at(app, width, height, column + 1, row), Some(background));
}

#[test]
fn the_last_part_of_the_breadcrumb_has_its_ends_in_its_accent() {
    let mut app = app();
    app.resize(130, 24);
    for _ in 0..4 {
        press(&mut app, KeyCode::Char('j'));
    }
    let mut snapshot = snapshot();
    snapshot.in_view = Some(InView::Session(snapshot.working[1].clone()));
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
    snapshot.in_view = Some(InView::Session(coder));
    app.refresh(snapshot);
    // Wide enough for the text and its end, for the text only, and for neither.
    let (column, row) = find(&mut app, 60, 16, "◆▌");
    let fits = column + 1;
    for (width, last) in [(fits + 1, "▌"), (fits, "◆"), (fits - 5, "…")] {
        assert_eq!(symbol_at(&mut app, width, 16, width - 1, row), last);
        assert_eq!(
            bg_at(&mut app, width, 16, width - 1, row) == Some(accent()),
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
    assert_eq!(symbol_at(&mut app, 60, 16, column + 9, row), " ");
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
    let label = |app: &mut App| style_at(app, 60, 16, column + 1, row);
    let ends = |app: &mut App| [column, column + 6].map(|x| style_at(app, 60, 16, x, row).fg);
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
    assert_eq!(bg_at(&mut app, 60, 16, 1, 15), Some(accent()));
}

#[test]
fn the_review_prompt_has_the_accent_on_the_whole_line() {
    let mut app = app();
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('v'));
    assert!(matches!(app.prompt, Some(Prompt::Verdict { .. })));
    let (column, row) = find(&mut app, 60, 24, "r changes");
    assert_eq!(row, 23);
    // The key and its words, and the last column of the line.
    let key = style_at(&mut app, 60, 24, column, row);
    assert_eq!(key.bg, Some(accent()));
    assert!(key.add_modifier.contains(ratatui::style::Modifier::BOLD));
    assert_eq!(bg_at(&mut app, 60, 24, column + 3, row), Some(accent()));
    assert_eq!(bg_at(&mut app, 60, 24, 59, row), Some(accent()));
}

#[test]
fn while_the_terminal_pane_is_not_focused_the_accent_is_dimmed() {
    let mut app = app();
    let (_, row) = find(&mut app, 60, 16, "wants:");
    assert!(app.terminal_focus(false));
    assert!(!app.terminal_focus(false));
    assert_eq!(bg_at(&mut app, 60, 16, 10, row), Some(accent_dimmed()));
    // The bar takes the colour of the dimmed background, so it does not show.
    let bar = style_at(&mut app, 60, 16, 0, row);
    assert_eq!(
        (bar.fg, bar.bg),
        (Some(accent_dimmed()), Some(accent_dimmed()))
    );
    assert!(app.terminal_focus(true));
    assert_eq!(bg_at(&mut app, 60, 16, 10, row), Some(accent()));
    let bar = style_at(&mut app, 60, 16, 0, row);
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

fn usage(seconds: i64, used: u64, size: u64, cost: Option<f64>) -> TranscriptEntry {
    let mut usage = json!({"sessionUpdate": "usage_update", "used": used, "size": size});
    if let Some(amount) = cost {
        usage["cost"] = json!({"amount": amount, "currency": "USD"});
    }
    update(seconds, usage)
}

/// The result of `session/new`, saying the agent runs Opus 5.5.
fn session_on_opus() -> TranscriptEntry {
    entry(
        0,
        Direction::FromAgent,
        json!({"id": "0", "result": {"sessionId": "s", "configOptions": [
            {"id": "model", "category": "model", "currentValue": "opus", "type": "select", "options": [
                {"value": "default", "name": "Default (recommended)"},
                {"value": "opus", "name": "Opus 5.5"},
            ]},
        ]}}),
    )
}

/// The coder on Opus 5.5 at 7% of its context, the parked coder at 85% on a model it did not
/// name; the reviewer runs elsewhere.
fn with_usage(app: &mut App) {
    let mut coder = vec![session_on_opus()];
    coder.extend(coder_transcript());
    coder.push(usage(5, 1000, 1_000_000, None));
    coder.push(usage(6, 68_620, 1_000_000, Some(0.4213)));
    app.transcribed(app.snapshot.working[0].id, &coder);
    app.transcribed(
        app.snapshot.working[1].id,
        &[usage(0, 170_000, 200_000, None)],
    );
}

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
        style_at(&mut app, 60, 22, x, y).fg,
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
    snapshot.in_view = Some(InView::Session(coder));
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
    snapshot.in_view = Some(InView::Session(snapshot.working[1].clone()));
    app.refresh(snapshot);
    insta::assert_snapshot!("detail_context_full", screen(&mut app, 130, 14));
    let line = screen(&mut app, 130, 14)
        .lines()
        .position(|line| line.contains("170k of 200k (85%)"))
        .unwrap();
    let x = app.list_width() + 3;
    let y = u16::try_from(line).unwrap();
    assert_eq!(
        style_at(&mut app, 130, 14, x, y).fg,
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
    snapshot.in_view = Some(InView::Session(coder));
    app.refresh(snapshot);
    insta::assert_snapshot!("detail_model", screen(&mut app, 60, 16));
}

#[test]
fn a_list_moves_only_as_far_as_keeps_the_selection_on_screen() {
    use super::draw::list_top;
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

/// What the coder of `#parse` does after [`coder_transcript`]: the tests pass, it thinks, sums up
/// in markdown and ends its turn; then the next turn starts.
fn coder_transcript_continued() -> Vec<TranscriptEntry> {
    vec![
        update(
            5,
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "2", "status": "completed",
                "content": [{"type": "content", "content": {"type": "text", "text": "```console\ntest result: ok. 12 passed\n```"}}]}),
        ),
        update(
            6,
            json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "The tests pass; the **loader** is done."}}),
        ),
        update(
            7,
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "## Done\n\n- `Config::load` is lazy now\n- a test covers the first access, which is a long line that wraps at a narrow width\n"}}),
        ),
        entry(
            8,
            Direction::FromAgent,
            json!({"id": 1, "result": {"stopReason": "end_turn"}}),
        ),
        entry(
            9,
            Direction::ToAgent,
            json!({"id": 2, "method": "session/prompt", "params": {"prompt": [{"type": "text", "text": "Now the spans."}]}}),
        ),
        update(
            10,
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Looking at the spans"}}),
        ),
    ]
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
    use super::transcript_lines::TranscriptLines;
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

/// The agent `coder` of `@ada`.
fn coder_agent() -> AgentId {
    AgentId {
        principal: wf_core::testing::Fixture::new(1).founder.key(),
        name: AgentName::new("coder").unwrap(),
    }
}

/// [`snapshot`] with the names its text can link to, and the item `#spans` open, whose text
/// mentions another item, an agent, a session, a principal and a commit.
fn with_links() -> Snapshot {
    let parse = ItemId::from_bytes([1; 32]);
    let spans = ItemId::from_bytes([2; 32]);
    let coder = SessionId::from_bytes([11; 32]);
    let mut snapshot = snapshot();
    snapshot.links = Links {
        items: [parse, spans].map(|id| (id.short(), id)).into(),
        comments: [(spans, CommentId::from_bytes([21; 32]).short())].into(),
        agents: [("@ada/coder".to_owned(), coder_agent())].into(),
        sessions: [(format!("@ada/coder/{}", coder.short()), coder)].into(),
    };
    snapshot.in_view = Some(InView::Item {
        id: spans,
        text: format!(
            "◐ #{} (task:in_progress) Fix error spans\n  review: merged a8d49484\n\nAfter #{}, ask @ada/coder.\n\n  07:00  @ada/coder/{} · commented\n  07:01  @ada · moved to review\n",
            spans.short(),
            parse.short(),
            coder.short(),
        ),
        session: None,
    });
    snapshot
}

/// The app with `#spans` open on the Now tab.
fn app_with_links() -> App {
    let mut app = app();
    app.open_item(ItemId::from_bytes([2; 32]));
    app.refresh(with_links());
    app
}

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
    assert_eq!(app.in_view(), Some(scrolled));
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
            style_at(&mut app, ROOMY, 24, column + 1, row)
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
        !style_at(&mut app, ROOMY, 24, column, row)
            .add_modifier
            .contains(Modifier::UNDERLINED)
    );
}

/// The permission request of the coder in the snapshot.
fn coders_request() -> Request {
    Request {
        session: SessionId::from_bytes([11; 32]),
        since_ms: NOW_MS - 5_000,
    }
}

fn permit(answer: Permission) -> Option<Effect> {
    Some(Effect::Permit {
        request: coders_request(),
        answer,
    })
}

/// The row of the latest event, where a tip shows.
const LATEST: u16 = 22;

/// A width that leaves room for the latest event and a tip beside it, short of a wide window.
const ROOMY: u16 = 110;

/// Asserts that `tip` shows at the right of the latest-event line, a column off the edge.
fn assert_tip(app: &mut App, tip: &str) {
    let column = ROOMY - 1 - u16::try_from(tip.chars().count()).unwrap();
    assert_eq!(find(app, ROOMY, 24, tip), (column, LATEST), "{tip}");
}

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
        let style = style_at(&mut app, 60, 18, column, row);
        assert!(!style.add_modifier.contains(Modifier::DIM), "{column}");
    }
    let border = style_at(&mut app, 60, 18, title - 1, row);
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
    let modifiers = |app: &mut App, column| style_at(app, 60, 24, column, row).add_modifier;
    let bg = |app: &mut App, column| bg_at(app, 60, 24, column, row);
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

/// The lines of the screen with `app` drawn, unquoted.
fn screen_lines(app: &mut App, width: u16, height: u16) -> Vec<String> {
    screen(app, width, height)
        .lines()
        .map(|line| line.trim_matches('"').to_owned())
        .collect()
}

#[test]
fn a_long_item_shows_how_far_down_it_is_scrolled_and_keeps_off_the_right_edge() {
    let mut app = app();
    let spans = ItemId::from_bytes([2; 32]);
    app.open_item(spans);
    let mut snapshot = snapshot();
    // A word too long for the pane, then twelve short lines.
    let mut text = format!("{}\n", "x".repeat(200));
    text.extend((1..=12).map(|line| format!("line {line}\n")));
    snapshot.in_view = Some(InView::Item {
        id: spans,
        text,
        session: None,
    });
    app.refresh(snapshot);
    // The body is on rows 4 to 6, its text 58 columns wide after the margin at the left: 16
    // lines, scrolled from 0 to 13.
    let body = |app: &mut App| -> Vec<String> { screen_lines(app, 60, 10)[4..=6].to_vec() };
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
    snapshot.in_view = Some(InView::Item {
        id: spans,
        text: format!("{full}\nnext\n"),
        session: None,
    });
    app.refresh(snapshot);
    let body = screen_lines(&mut app, 60, 10)[4..=6].to_vec();
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
    snapshot.in_view = Some(InView::Item {
        id: spans,
        text,
        session: None,
    });
    app.refresh(snapshot);
    let body = screen_lines(&mut app, 60, 10)[4..=5].to_vec();
    assert_eq!(body[0].trim(), rendered[0].trim(), "{body:#?}");
    assert_eq!(body[1].trim(), rendered[1].trim(), "{body:#?}");
}

#[test]
fn an_item_that_fits_shows_no_position() {
    let mut app = app();
    let spans = ItemId::from_bytes([2; 32]);
    app.open_item(spans);
    let mut snapshot = snapshot();
    snapshot.in_view = Some(InView::Item {
        id: spans,
        text: "line 1\nline 2\n".to_owned(),
        session: None,
    });
    app.refresh(snapshot);
    let body = screen_lines(&mut app, 60, 10)[4..=6].join("\n");
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
    let top = screen_lines(&mut app, 60, 12);
    assert!(top[8].ends_with(" ↓ 4 "), "{top:#?}");
    assert!(!top.join("").contains('↑'));
    // At the end, above the reviewer: the three waiting and the other two sessions, the
    // blocker's two lines counting once.
    for _ in 0..6 {
        press(&mut app, KeyCode::Char('j'));
    }
    let end = screen_lines(&mut app, 60, 12);
    assert!(end[3].contains("Archive by category"));
    assert!(end[3].ends_with(" ↑ 5 "), "{end:#?}");
    assert!(!end.join("").contains('↓'));
    // A click on the indicator is one on its row.
    app.click(57, 3);
    let reviewer = SessionId::from_bytes([13; 32]);
    assert_eq!(app.now.selection.get(), Some(&Row::Session(reviewer)));
}

#[test]
fn an_indicator_on_the_selection_has_its_background() {
    let mut app = app();
    press(&mut app, KeyCode::Char('j'));
    // Three rows of the list: the request, then both lines of the selected blocker.
    let lines = screen_lines(&mut app, 60, 9);
    assert!(lines[5].ends_with(" ↓ 5 "), "{lines:#?}");
    assert_eq!(bg_at(&mut app, 60, 9, 57, 5), Some(accent()));
}

/// A session of `coder` as `wf agent show` lists it, `minutes` old, or ended `minutes` ago.
fn session_view(
    seed: u8,
    agent: &str,
    item: &ItemRefView,
    status: SessionStatus,
    started_minutes_ago: u64,
) -> crate::view::SessionView {
    crate::view::SessionView {
        id: SessionId::from_bytes([seed; 32]),
        agent: format!("@ada/{agent}"),
        item: item.id,
        title: item.title.clone(),
        role: SessionRole::Work,
        status,
        started: crate::view::timestamp_ms(NOW_MS - started_minutes_ago * 60_000),
        model: None,
        commit: None,
        steps: None,
    }
}

/// How a session ended `minutes` ago.
fn ended(reason: EndReason, minutes: u64) -> SessionStatus {
    SessionStatus::Ended {
        reason,
        at: crate::view::timestamp_ms(NOW_MS - minutes * 60_000),
    }
}

/// `coder` with a live session and two that ended, instructions with markdown, hosted here or
/// only on another device.
fn coder_detail(hosted_here: bool) -> crate::view::AgentDetail {
    let host = |device: &str, sessions, this_device| crate::view::HostView {
        device: device.to_owned(),
        sessions,
        this_device,
    };
    let mut hosts = vec![host("laptop", 1, false)];
    if hosted_here {
        hosts.insert(0, host("namioka", 2, true));
    }
    crate::view::AgentDetail {
        definition: crate::view::AgentView {
            id: "@ada/coder".to_owned(),
            harness: "claude-code".to_owned(),
            harness_startable_here: true,
            model: Some("opus".to_owned()),
            capabilities: vec!["rust".to_owned(), "review".to_owned()],
            lanes: vec!["default".to_owned()],
            start: "auto".to_owned(),
            permissions: "allow".to_owned(),
            nudges: 3,
            park_timeout: "2h".to_owned(),
            max_duration: Some("1h".to_owned()),
            instructions: Some(
                "Work in **small** commits.\n\n- run `cargo test` first\n- ask before a rewrite"
                    .to_owned(),
            ),
        },
        hosts,
        stats: AgentStats {
            sessions: 14,
            live: 1,
            finished: 11,
            failed: 2,
            median_duration_ms: Some(4 * 60_000),
        },
        sessions: vec![
            session_view(
                11,
                "coder",
                &item(1, "Parse config lazily"),
                SessionStatus::Live,
                12 + 30,
            ),
            session_view(
                14,
                "coder",
                &item(4, "Dashboard"),
                ended(EndReason::Submitted { commit: None }, 120),
                120 + 30,
            ),
            session_view(
                15,
                "coder",
                &item(2, "Fix error spans"),
                ended(EndReason::TimedOut, 60 * 26),
                60 * 26 + 30,
            ),
        ],
        older_sessions: 11,
        activity: Vec::new(),
    }
}

/// The app with the agent `coder` open from the Agents tab, at `width`, showing `detail`.
fn app_with_agent(width: u16, detail: &crate::view::AgentDetail) -> App {
    let mut app = app();
    app.resize(width, 24);
    press(&mut app, KeyCode::Char('3'));
    app.refresh(with_agents());
    press(&mut app, KeyCode::Enter);
    let mut snapshot = with_agents();
    let coder = SessionId::from_bytes([11; 32]);
    snapshot.links = Links {
        items: [1, 2, 4]
            .map(|seed| {
                let id = ItemId::from_bytes([seed; 32]);
                (id.short(), id)
            })
            .into(),
        sessions: [(format!("@ada/coder/{}", coder.short()), coder)].into(),
        ..Links::default()
    };
    snapshot.in_view = Some(InView::Agent {
        id: coder_agent(),
        text: crate::render::agent_detail_for(
            detail,
            Some(usize::from(app.detail_width())),
            crate::transcript::Format::Styled,
            jiff::Timestamp::from_millisecond(NOW_MS as i64).unwrap(),
        ),
    });
    app.refresh(snapshot);
    app
}

#[test]
fn an_agent_detail_shows_its_state_config_hosts_sessions_and_instructions() {
    let mut app = app_with_agent(80, &coder_detail(true));
    insta::assert_snapshot!("agent_detail", screen(&mut app, 80, 30));
}

#[test]
fn an_agent_not_hosted_here_says_so() {
    let mut app = app_with_agent(80, &coder_detail(false));
    let lines = screen_lines(&mut app, 80, 30);
    assert!(lines[4].contains("1 live · not hosted here"), "{lines:#?}");
    assert!(lines[7].contains("hosted by laptop up to 1"), "{lines:#?}");
}

#[test]
fn in_a_narrow_window_the_agent_detail_wraps_off_the_right_edge() {
    let mut app = app_with_agent(40, &coder_detail(true));
    insta::assert_snapshot!("agent_detail_narrow", screen(&mut app, 40, 40));
    for line in &screen_lines(&mut app, 40, 40)[3..37] {
        assert!(line.ends_with(' '), "{line:?}");
    }
}

#[test]
fn a_click_on_an_item_or_a_session_of_an_agent_opens_it() {
    let mut app = app_with_agent(80, &coder_detail(true));
    let agent = Detail::Agent(coder_agent());
    let opens = [
        ("#aeaqca", Detail::Item(ItemId::from_bytes([1; 32]))),
        (
            "@ada/coder/",
            Detail::Session(SessionId::from_bytes([11; 32])),
        ),
    ];
    for (text, detail) in opens {
        let (column, row) = find(&mut app, 80, 30, text);
        assert_eq!(app.click(column + 1, row), None);
        assert_eq!(app.path(), vec![agent.clone(), detail], "{text}");
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.path(), vec![agent.clone()]);
    }
}

fn conflict(seed: u8) -> MovedBack {
    MovedBack {
        item: ItemId::from_bytes([seed; 32]),
        reason: "merging into main conflicts in README.md".to_owned(),
    }
}

/// The footer: the rule, the latest-event line and the keys.
fn footer(app: &mut App, width: u16) -> String {
    let screen = screen(app, width, 24);
    let lines: Vec<&str> = screen.lines().collect();
    lines[lines.len() - 3..].join("\n")
}

#[test]
fn a_move_back_to_open_warns_beside_the_latest_event() {
    let mut app = app();
    app.warn([conflict(1)]);
    insta::assert_snapshot!("moved_back", footer(&mut app, ROOMY));
    let (column, row) = find(&mut app, ROOMY, 24, "⚠ #aeaqca back to open");
    assert_eq!(row, LATEST);
    let style = style_at(&mut app, ROOMY, 24, column, row);
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
        app.in_view().map(|opened| opened.detail),
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

/// What `app` draws into a test terminal, cell by cell.
fn buffer(app: &mut App, width: u16, height: u16) -> ratatui::buffer::Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| draw::draw(frame, app)).unwrap();
    terminal.backend().buffer().clone()
}

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
    let buffer = buffer(&mut app, 60, 24);
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
    let buffer = buffer(&mut app, 60, 24);
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

/// A diff of two files: one changed in two places, one added.
const DIFF: &str = "\
diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,3 @@ mod config;
 use std::io;
-fn parse() {}
+fn parse_lazily() {}
 fn main() {}
@@ -20,2 +20,3 @@ fn main() {}
 // the end
+// really
 }
diff --git a/README.md b/README.md
new file mode 100644
--- /dev/null
+++ b/README.md
@@ -0,0 +1,2 @@
+# wf
+\tIndented.
";

fn work_commit() -> wf_core::session::CommitId {
    wf_core::session::CommitId::Git("a8d4948400000000000000000000000000000000".parse().unwrap())
}

/// The app with the diff of the archive item open from its review, read already.
fn diff_app() -> App {
    let mut app = app();
    let archive = ItemId::from_bytes([3; 32]);
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('D'));
    assert_eq!(app.wanted().detail, Some(Detail::Diff(archive)));
    app.diff_on_top_mut()
        .unwrap()
        .read(Ok(super::diff::ItemWork {
            commit: work_commit(),
            changes: super::diff::Changes::Files(wf_vcs::diff::parse(DIFF)),
        }));
    app
}

#[test]
fn the_diff_says_when_the_agent_has_not_made_any_commits_or_they_change_no_files() {
    let mut app = diff_app();
    let pane = app.diff_on_top_mut().unwrap();
    pane.read(Ok(super::diff::ItemWork {
        commit: work_commit(),
        changes: super::diff::Changes::NoCommits,
    }));
    let screen_text = screen(&mut app, 60, 24);
    assert!(
        screen_text.contains("the agent has not made any commits"),
        "{screen_text}"
    );
    let pane = app.diff_on_top_mut().unwrap();
    pane.read(Ok(super::diff::ItemWork {
        commit: work_commit(),
        changes: super::diff::Changes::Files(Vec::new()),
    }));
    let screen_text = screen(&mut app, 60, 24);
    assert!(
        screen_text.contains("the work changes no files"),
        "{screen_text}"
    );
}

#[test]
fn d_opens_the_diff_of_a_review_with_files_hunks_and_numbered_lines() {
    let mut app = diff_app();
    insta::assert_snapshot!(screen(&mut app, 60, 24));
}

#[test]
fn the_cursor_moves_over_files_and_lines_and_enter_folds_a_file() {
    let mut app = diff_app();
    let cursor_line = |app: &mut App| {
        screen(app, 60, 24)
            .lines()
            .find(|line| line.contains('▎'))
            .unwrap_or_default()
            .to_owned()
    };
    assert!(cursor_line(&mut app).contains("src/lib.rs"));
    // The heading of a hunk is passed over.
    press(&mut app, KeyCode::Char('j'));
    assert!(cursor_line(&mut app).contains("use std::io;"));
    press(&mut app, KeyCode::Char(']'));
    assert!(cursor_line(&mut app).contains("README.md"));
    press(&mut app, KeyCode::Char('['));
    assert!(cursor_line(&mut app).contains("src/lib.rs"));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Enter);
    let folded = screen(&mut app, 60, 24);
    assert!(cursor_line(&mut app).contains("src/lib.rs"), "{folded}");
    assert!(!folded.contains("use std::io;"), "{folded}");
    press(&mut app, KeyCode::Char('G'));
    assert!(cursor_line(&mut app).contains("Indented."));
    // `i` goes to the item, `D` back to the diff.
    press(&mut app, KeyCode::Char('i'));
    assert!(matches!(app.path().last(), Some(Detail::Item(_))));
    press(&mut app, KeyCode::Char('D'));
    assert!(matches!(app.path().last(), Some(Detail::Diff(_))));
    assert_eq!(
        press(&mut app, KeyCode::Char('p')),
        Some(Effect::PageDiff {
            item: ItemId::from_bytes([3; 32])
        })
    );
}

#[test]
fn i_in_a_diff_opened_from_the_list_replaces_it_with_its_item() {
    let mut app = diff_app();
    press(&mut app, KeyCode::Char('i'));
    assert_eq!(app.path(), [Detail::Item(ItemId::from_bytes([3; 32]))]);
}

#[test]
fn i_in_a_diff_or_session_opened_from_its_item_goes_back_to_the_item() {
    let mut app = app();
    press(&mut app, KeyCode::Char('2'));
    let parse = ItemId::from_bytes([1; 32]);
    let coder = SessionId::from_bytes([11; 32]);
    let mut snapshot = with_items();
    snapshot.in_view = Some(InView::Item {
        id: parse,
        text: (1..=40).map(|line| format!("line {line}\n")).collect(),
        session: Some(coder),
    });
    app.refresh(snapshot);
    press(&mut app, KeyCode::Char('G'));
    press(&mut app, KeyCode::Enter);
    let item_scroll = app.stack().last().unwrap().scroll.clone();
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.path(), [Detail::Item(parse), Detail::Session(coder)]);
    press(&mut app, KeyCode::Char('i'));
    assert_eq!(app.path(), [Detail::Item(parse)]);
    assert_eq!(app.stack().last().unwrap().scroll, item_scroll);
    // The diff, opened from the item.
    press(&mut app, KeyCode::Char('D'));
    assert_eq!(app.path(), [Detail::Item(parse), Detail::Diff(parse)]);
    press(&mut app, KeyCode::Char('i'));
    assert_eq!(app.path(), [Detail::Item(parse)]);
}

#[test]
fn i_in_a_diff_below_an_item_and_its_session_goes_back_to_the_item() {
    let mut app = app();
    press(&mut app, KeyCode::Char('2'));
    let parse = ItemId::from_bytes([1; 32]);
    let mut snapshot = with_items();
    snapshot.in_view = Some(InView::Item {
        id: parse,
        text: String::new(),
        session: Some(SessionId::from_bytes([11; 32])),
    });
    app.refresh(snapshot);
    press(&mut app, KeyCode::Char('G'));
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Enter);
    let mut snapshot = with_items();
    snapshot.in_view = Some(InView::Session(snapshot.working[0].clone()));
    app.refresh(snapshot);
    press(&mut app, KeyCode::Enter);
    assert!(matches!(
        app.path()[..],
        [Detail::Item(_), Detail::Session(_), Detail::Diff(_)]
    ));
    press(&mut app, KeyCode::Char('i'));
    assert_eq!(app.path(), [Detail::Item(parse)]);
}

#[test]
fn i_in_a_diff_opened_from_a_session_replaces_it_with_the_item() {
    let mut app = app();
    app.refresh(snapshot());
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Right);
    assert!(matches!(
        app.path()[..],
        [Detail::Session(_), Detail::Diff(_)]
    ));
    press(&mut app, KeyCode::Char('i'));
    assert!(matches!(
        app.path()[..],
        [Detail::Session(_), Detail::Item(_)]
    ));
}

#[test]
fn the_wheel_scrolls_the_diff_and_takes_the_cursor_along() {
    let mut app = diff_app();
    screen(&mut app, 60, 12);
    app.wheel(30, Wheel::Down);
    app.wheel(30, Wheel::Down);
    let shown = screen(&mut app, 60, 12);
    assert!(!shown.contains("src/lib.rs"), "{shown}");
    let cursor = shown.lines().find(|line| line.contains('▎'));
    assert!(cursor.is_some(), "the cursor is on screen: {shown}");
}

#[test]
fn v_selects_lines_within_their_hunk_and_esc_ends_the_selection() {
    let mut app = diff_app();
    // On `use std::io;`, the first line of the first hunk.
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('v'));
    assert!(
        app.prompt.is_none(),
        "v selects in a diff rather than review"
    );
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    insta::assert_snapshot!(screen(&mut app, 60, 16));
    // The selection stays in its hunk.
    for _ in 0..5 {
        press(&mut app, KeyCode::Char('j'));
    }
    let pane = app.diff_on_top().unwrap();
    let rows = pane.rows();
    let selected = pane.selected().unwrap();
    assert_eq!(selected.clone().count(), 4);
    assert!(
        selected
            .map(|row| rows[row])
            .all(|row| matches!(row, super::diff::DiffRow::Line { hunk: 0, .. }))
    );
    press(&mut app, KeyCode::Esc);
    assert!(app.diff_on_top().unwrap().selection.is_none());
    assert!(
        matches!(app.path().last(), Some(Detail::Diff(_))),
        "esc ended only the selection"
    );
    // `R` asks for the verdict on the review, as `v` does elsewhere.
    press(&mut app, KeyCode::Char('R'));
    assert!(matches!(app.prompt, Some(Prompt::Verdict { .. })));
}

#[test]
fn a_drag_selects_lines_and_a_click_moves_the_cursor() {
    let mut app = diff_app();
    let (column, row) = find(&mut app, 60, 24, "fn parse() {}");
    assert_eq!(app.click(column, row), None);
    let pane = app.diff_on_top().unwrap();
    assert_eq!(pane.cursor, 3, "the cursor is on the line clicked");
    assert!(pane.selection.is_none());
    let (_, below) = find(&mut app, 60, 24, "fn main() {}");
    app.drag(column, below);
    let pane = app.diff_on_top().unwrap();
    assert_eq!(pane.selected(), Some(3..=5));
    // A drag past the hunk keeps to it.
    app.drag(column, 22);
    assert_eq!(app.diff_on_top().unwrap().selected(), Some(3..=5));
}

fn type_text(app: &mut App, text: &str) {
    for character in text.chars() {
        press(app, KeyCode::Char(character));
    }
}

#[test]
fn a_change_request_on_selected_lines_is_a_draft_under_them_until_the_verdict_sends_it() {
    let mut app = diff_app();
    let archive = ItemId::from_bytes([3; 32]);
    // From `use std::io;` to `+fn parse_lazily() {}`.
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('v'));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('r'));
    type_text(&mut app, "Keep the old name, callers use it.");
    insta::assert_snapshot!("line_comment_prompt", screen(&mut app, 80, 16));
    press(&mut app, KeyCode::Enter);
    let drafts = app.drafts(archive).to_vec();
    assert_eq!(drafts.len(), 1);
    let lines = &drafts[0].lines;
    assert_eq!(drafts[0].kind, wf_core::event::CommentKind::ChangeRequest);
    assert_eq!(lines.location(), "src/lib.rs:1-2");
    assert_eq!(lines.side, wf_core::event::DiffSide::New);
    assert_eq!(lines.commit, work_commit());
    assert_eq!(
        lines.quote,
        " use std::io;\n-fn parse() {}\n+fn parse_lazily() {}"
    );
    assert!(app.diff_on_top().unwrap().selection.is_none());
    insta::assert_snapshot!("line_comment_draft", screen(&mut app, 80, 16));
    // Approving with a change request drafted is refused; requesting changes sends it.
    press(&mut app, KeyCode::Char('R'));
    assert!(screen(&mut app, 80, 16).contains("with 1 comment on lines"));
    assert_eq!(press(&mut app, KeyCode::Char('a')), None);
    assert!(matches!(app.status, Some(Status::ActionFailed { .. })));
    press(&mut app, KeyCode::Char('R'));
    assert_eq!(
        press(&mut app, KeyCode::Char('r')),
        Some(Effect::RequestChanges {
            item: archive,
            reviewed: Some(work_commit()),
            comments: drafts
        })
    );
    app.reviewed(archive);
    assert!(app.drafts(archive).is_empty());
}

#[test]
fn a_draft_under_the_cursor_is_edited_with_e_and_dropped_with_d() {
    let mut app = diff_app();
    let archive = ItemId::from_bytes([3; 32]);
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('c'));
    type_text(&mut app, "Why io?");
    press(&mut app, KeyCode::Enter);
    // The cursor stops on the draft, under its line.
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.diff_on_top().unwrap().draft_at_cursor(), Some(0));
    assert!(screen(&mut app, 80, 16).contains("e edit · d drop"));
    press(&mut app, KeyCode::Char('e'));
    type_text(&mut app, " It is unused.");
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.drafts(archive)[0].text, "Why io? It is unused.");
    assert_eq!(app.drafts(archive).len(), 1);
    press(&mut app, KeyCode::Char('d'));
    assert!(app.drafts(archive).is_empty());
    // The drafts go with the verdict, whichever it is.
    press(&mut app, KeyCode::Char('c'));
    type_text(&mut app, "Fine.");
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Char('R'));
    assert!(matches!(
        press(&mut app, KeyCode::Char('a')),
        Some(Effect::Approve { comments, .. }) if comments.len() == 1
    ));
}

#[test]
fn without_a_review_a_comment_on_lines_is_posted_at_once() {
    let mut app = app();
    let parse = ItemId::from_bytes([1; 32]);
    app.open_item(parse);
    press(&mut app, KeyCode::Char('l'));
    assert_eq!(app.wanted().detail, Some(Detail::Diff(parse)));
    app.diff_on_top_mut()
        .unwrap()
        .read(Ok(super::diff::ItemWork {
            commit: work_commit(),
            changes: super::diff::Changes::Files(wf_vcs::diff::parse(DIFF)),
        }));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('c'));
    type_text(&mut app, "Removed on purpose?");
    let Some(Effect::PostLineComment { item, comment }) = press(&mut app, KeyCode::Enter) else {
        panic!("a comment posted");
    };
    assert_eq!(item, parse);
    // A removed line alone counts on the old side.
    assert_eq!(comment.lines.side, wf_core::event::DiffSide::Old);
    assert_eq!(comment.lines.location(), "src/lib.rs:2");
    assert!(app.drafts(parse).is_empty());
}

#[test]
fn comments_in_the_log_show_under_their_lines_or_as_outdated_under_their_file() {
    let mut app = diff_app();
    let archive = ItemId::from_bytes([3; 32]);
    let posted = |commit: wf_core::session::CommitId, text: &str| super::diff::PostedComment {
        by: "@ada".to_owned(),
        comment: super::diff::LineComment {
            kind: wf_core::event::CommentKind::ChangeRequest,
            lines: wf_core::event::LineAnchor {
                commit,
                path: "src/lib.rs".to_owned(),
                side: wf_core::event::DiffSide::New,
                first: 21,
                last: 21,
                quote: "+// really".to_owned(),
            },
            text: text.to_owned(),
        },
        resolved: false,
    };
    let older = wf_core::session::CommitId::Git(
        "0000000000000000000000000000000000000001".parse().unwrap(),
    );
    app.refresh_detail(Some(InView::Diff {
        id: archive,
        comments: vec![
            posted(work_commit(), "Say why."),
            posted(older, "Was this here before?"),
        ],
    }));
    insta::assert_snapshot!(screen(&mut app, 80, 24));
}

#[test]
fn a_refresh_clears_only_the_failure_of_reading_the_workforce() {
    let mut app = app();
    app.refresh_failed(&anyhow::anyhow!("daemon gone"));
    app.refresh(snapshot());
    assert!(matches!(app.status, Some(Status::RefreshFailed(_))));
    app.refreshed();
    assert_eq!(app.status, None);
    // A transcript that could not be read stays for the next refresh to find, until a key.
    app.transcript_failed(&anyhow::anyhow!("malformed line"));
    app.refreshed();
    app.refresh(snapshot());
    assert!(matches!(app.status, Some(Status::TranscriptFailed { .. })));
    assert!(screen(&mut app, 60, 16).contains("error: malformed line"));
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.status, None);
}

#[test]
fn a_failure_of_a_refresh_does_not_replace_the_failure_of_an_action() {
    let mut app = app();
    app.action_failed(&anyhow::anyhow!("no such item"), NOW_MS);
    app.refresh_failed(&anyhow::anyhow!("daemon gone"));
    app.transcript_failed(&anyhow::anyhow!("malformed line"));
    assert!(matches!(app.status, Some(Status::ActionFailed { .. })));
}

#[test]
fn keys_ignore_what_is_in_view_for_another_detail() {
    let mut app = app();
    let parse = ItemId::from_bytes([1; 32]);
    let spans = ItemId::from_bytes([2; 32]);
    let session = SessionId::from_bytes([11; 32]);
    app.open_item(spans);
    // The snapshot still has the item that was open before.
    let mut snapshot = snapshot();
    snapshot.in_view = Some(InView::Item {
        id: parse,
        text: "previous\n".to_owned(),
        session: Some(session),
    });
    app.refresh(snapshot);
    assert_eq!(app.in_view_of(&Detail::Item(spans)), None);
    press(&mut app, KeyCode::Char('l'));
    assert_eq!(app.wanted().detail, Some(Detail::Diff(spans)));
}

#[test]
fn a_selection_key_that_does_not_move_keeps_the_details_beside_the_list() {
    let mut app = app();
    app.resize(130, 24);
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Esc);
    assert!(app.list_has_keys());
    let opened = app.stack().clone();
    assert!(!opened.is_empty());
    // At the top, k goes nowhere.
    press(&mut app, KeyCode::Char('k'));
    assert_eq!(app.stack(), &opened);
    press(&mut app, KeyCode::Char('j'));
    assert!(app.stack().is_empty());
}

#[test]
fn a_verdict_carries_the_commit_the_diff_on_screen_shows() {
    let mut app = diff_app();
    let archive = ItemId::from_bytes([3; 32]);
    press(&mut app, KeyCode::Char('R'));
    assert!(matches!(
        press(&mut app, KeyCode::Char('a')),
        Some(Effect::Approve { reviewed, .. }) if reviewed == Some(work_commit())
    ));
    // Work without commits is reviewed as submitted without a commit.
    app.diff_on_top_mut()
        .unwrap()
        .read(Ok(super::diff::ItemWork {
            commit: work_commit(),
            changes: super::diff::Changes::NoCommits,
        }));
    press(&mut app, KeyCode::Char('R'));
    assert_eq!(
        press(&mut app, KeyCode::Char('a')),
        Some(Effect::Approve {
            item: archive,
            reviewed: None,
            comments: Vec::new()
        })
    );
}

#[test]
fn drafts_that_were_posted_before_a_failure_are_not_drafts_any_more() {
    let mut app = diff_app();
    let archive = ItemId::from_bytes([3; 32]);
    let draft = |text: &str| super::diff::LineComment {
        kind: wf_core::event::CommentKind::Plain,
        lines: wf_core::event::LineAnchor {
            commit: work_commit(),
            path: "src/lib.rs".to_owned(),
            side: wf_core::event::DiffSide::New,
            first: 1,
            last: 1,
            quote: String::new(),
        },
        text: text.to_owned(),
    };
    for text in ["one", "two", "three"] {
        app.drafted(archive, draft(text), None);
    }
    app.drafts_posted(archive, 2);
    let left: Vec<&str> = app
        .drafts(archive)
        .iter()
        .map(|draft| draft.text.as_str())
        .collect();
    assert_eq!(left, ["three"]);
}

#[test]
fn the_dashboard_waits_on_as_many_things_as_wf_status_and_shows_the_machine_faults() {
    use super::snapshot::{Sources, Wanted};
    use wf_core::testing::*;
    use wf_daemon::{HarnessAtFault, MachineFaults, OutOfSpace};

    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let mut events = Vec::new();
    let mut apply = |state: &mut wf_core::state::State, body| {
        let event = fixture.author(state, 0, 2_000, body);
        state
            .receive(event.clone(), wf_core::state::Trust::Untrusted)
            .unwrap();
        events.push(event.clone());
        event
    };
    let item: ItemId = apply(&mut state, create("task", "Held")).id().into();
    apply(&mut state, blocker(item, "which database?"));
    let faults = MachineFaults {
        out_of_space: Some(OutOfSpace {
            since_ms: 1_000,
            what: "the event log".to_owned(),
        }),
        harnesses: [(
            "claude".parse().unwrap(),
            HarnessAtFault {
                fault: wf_core::session::HarnessFault::LoginLapsed,
                since_ms: 1_000,
                last_ms: 1_000,
                what: "not logged in".to_owned(),
            },
        )]
        .into(),
    };
    let sources = Sources {
        state: &state,
        events: &events,
        own: fixture.devices[0].key(),
        now_ms: 3_000,
        detail_width: 80,
    };
    let snapshot = Snapshot::take(
        &sources,
        &[],
        &faults,
        &Default::default(),
        Wanted::default(),
    );
    let in_inbox = crate::view::inbox_views(&state, &[], &faults).len();
    assert_eq!(snapshot.waiting.len(), in_inbox);
    assert_eq!(snapshot.waiting.len(), 3);
    let titles: Vec<&str> = snapshot
        .waiting
        .iter()
        .filter_map(|entry| match entry {
            Waiting::MachineFault { title, .. } => Some(title.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        titles,
        ["The disk is full", "The login of claude has lapsed"]
    );
}

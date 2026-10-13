//! The dashboard drawn into a test terminal, from snapshots made by hand.

use jiff::tz::TimeZone;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use serde_json::json;
use wf_core::graph::Readiness;
use wf_core::id::{CommentId, ItemId, SessionId};
use wf_core::item::Priority;
use wf_core::name::{AgentName, StateName};
use wf_core::policy::StateCategory;
use wf_harness::{Direction, Permission, TranscriptEntry};

use super::app::{AgentRow, App, Choice, Effect, Prompt, Row, Tab};
use super::draw;
use super::snapshot::{
    AgentInfo, Counts, Detail, InView, ItemGroup, ItemRef, ItemRow, Place, SessionInfo,
    SessionStatus, Snapshot, Waiting,
};
use wf_core::identity::AgentId;
use wf_core::session::EndReason;
use wf_core::stats::{AgentStats, ProjectStats};

/// 2026-10-14 07:00 UTC.
const NOW_MS: u64 = 1_792_047_600_000;

fn screen(app: &mut App, width: u16, height: u16) -> String {
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
        items: Vec::new(),
        agents: Vec::new(),
        stats: ProjectStats::default(),
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
    insta::assert_snapshot!(screen(&mut app, 60, 22));
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
    app.resize(130);
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
fn a_review_asks_for_the_verdict_and_offers_the_diff_first() {
    let mut app = app();
    let archive = ItemId::from_bytes([3; 32]);
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(press(&mut app, KeyCode::Char('v')), None);
    insta::assert_snapshot!(screen(&mut app, 60, 8));
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
    assert!(screen(&mut app, 60, 8).contains("✓ allowed: rm -rf target"));
    app.refresh(snapshot());
    assert!(
        screen(&mut app, 60, 8).contains("✓ allowed"),
        "a refresh keeps it"
    );
    press(&mut app, KeyCode::Char('j'));
    assert!(!screen(&mut app, 60, 8).contains("✓ allowed"));
}

fn item_row(seed: u8, title: &str, category: StateCategory, state: &str) -> ItemRow {
    ItemRow {
        id: ItemId::from_bytes([seed; 32]),
        title: title.to_owned(),
        state: state.to_owned(),
        state_category: Some(category),
        priority: None,
        readiness: None,
        steps: None,
    }
}

/// The snapshot while the Items tab is on screen.
fn with_items() -> Snapshot {
    let mut snapshot = snapshot();
    snapshot.items = vec![
        ItemGroup {
            name: "open".to_owned(),
            items: vec![
                ItemRow {
                    priority: Some(Priority::P1),
                    readiness: Some(Readiness::Ready),
                    ..item_row(4, "Dashboard", StateCategory::Open, "open")
                },
                ItemRow {
                    readiness: Some(Readiness::Blocked),
                    ..item_row(5, "Session subscription", StateCategory::Open, "open")
                },
            ],
        },
        ItemGroup {
            name: "in_progress".to_owned(),
            items: vec![ItemRow {
                steps: Some((2, 5)),
                ..item_row(
                    1,
                    "Parse config lazily",
                    StateCategory::Active,
                    "in_progress",
                )
            }],
        },
    ];
    snapshot
}

#[test]
fn the_items_tab_lists_items_by_state_and_filters_them() {
    let mut app = app();
    press(&mut app, KeyCode::Char('2'));
    assert_eq!(app.wanted().items, Some(crate::cli::ItemGrouping::State));
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
    assert_eq!(app.wanted().items, Some(crate::cli::ItemGrouping::Lane));
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
fn item_keys_act_on_the_selected_item() {
    let mut app = app();
    press(&mut app, KeyCode::Char('2'));
    app.refresh(with_items());
    let dashboard = ItemId::from_bytes([4; 32]);
    assert_eq!(
        press(&mut app, KeyCode::Char('s')),
        Some(Effect::ChooseAgent { item: dashboard })
    );
    app.choose(Choice::Agent {
        item: dashboard,
        agents: vec![
            AgentName::new("coder").unwrap(),
            AgentName::new("reviewer").unwrap(),
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
        states: vec![
            (StateName::new("open").unwrap(), StateCategory::Open),
            (StateName::new("done").unwrap(), StateCategory::Done),
        ],
        index: 0,
    });
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(
        press(&mut app, KeyCode::Enter),
        Some(Effect::Move {
            item: dashboard,
            state: StateName::new("done").unwrap()
        })
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

/// The snapshot while the Agents tab is on screen.
fn with_agents() -> Snapshot {
    let mut snapshot = snapshot();
    let principal = wf_core::testing::Fixture::new(1).founder.key();
    let ended = SessionInfo {
        status: SessionStatus::Ended {
            reason: EndReason::Submitted { commit: None },
            at_ms: NOW_MS - 2 * 60 * 60_000,
        },
        ..session(14, "coder", item(4, "Dashboard"), 300)
    };
    snapshot.agents = vec![
        AgentInfo {
            id: AgentId {
                principal,
                name: AgentName::new("coder").unwrap(),
            },
            runs_on: "claude-code · opus".to_owned(),
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
            runs_on: "claude-code".to_owned(),
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
            session: SessionId::from_bytes([11; 32]),
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
    // So is a session opened from the Now tab.
    press(&mut app, KeyCode::Char('1'));
    app.refresh(snapshot());
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Right);
    assert_eq!(app.path().len(), 1);
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.path(), Vec::new());
    // The grouping and filters of the items stay as they are under an item, while the keys of
    // the dashboard still work.
    press(&mut app, KeyCode::Char('2'));
    app.refresh(with_items());
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Char('o'));
    press(&mut app, KeyCode::Char('F'));
    press(&mut app, KeyCode::Char('/'));
    assert_eq!(app.wanted().items, Some(crate::cli::ItemGrouping::State));
    assert!(app.prompt.is_none());
    assert_eq!(app.item_groups().len(), with_items().items.len());
    press(&mut app, KeyCode::Char('?'));
    assert!(matches!(app.prompt, Some(Prompt::Keys)));
    press(&mut app, KeyCode::Esc);
    assert_eq!(press(&mut app, KeyCode::Char('q')), Some(Effect::Quit));
}

/// The style of the cell at `x`, `y` once `app` is drawn.
fn style_at(app: &mut App, width: u16, height: u16, x: u16, y: u16) -> ratatui::style::Style {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| draw::draw(frame, app)).unwrap();
    terminal.backend().buffer()[(x, y)].style()
}

#[test]
fn the_part_with_the_keys_stands_out() {
    use ratatui::style::{Color, Modifier};
    let mut app = app();
    app.resize(130);
    let blocked = SessionId::from_bytes([12; 32]);
    for _ in 0..4 {
        press(&mut app, KeyCode::Char('j'));
    }
    let mut snapshot = snapshot();
    snapshot.in_view = Some(InView::Session(snapshot.working[1].clone()));
    app.refresh(snapshot.clone());
    // The parked coder is selected on line 9; the breadcrumb is on line 3, right of the list.
    let (row, crumb_y) = (9, 3);
    let crumb_x = app.list_width() + 3;
    assert_eq!(app.wanted().detail, Some(Detail::Session(blocked)));
    assert_eq!(
        style_at(&mut app, 130, 14, 30, row).bg,
        Some(Color::DarkGray)
    );
    assert!(
        style_at(&mut app, 130, 14, crumb_x, crumb_y)
            .add_modifier
            .contains(Modifier::DIM)
    );
    // In the detail, the list lets go of the selection's background, and the breadcrumb lights up.
    press(&mut app, KeyCode::Char('l'));
    app.refresh(snapshot);
    assert_ne!(
        style_at(&mut app, 130, 14, 30, row).bg,
        Some(Color::DarkGray)
    );
    // Where `coder · work` starts: the backend quotes each line, which the column leaves out.
    let line = screen(&mut app, 130, 14)
        .lines()
        .nth(usize::from(crumb_y))
        .unwrap()
        .to_owned();
    let before = &line[1..line.find("coder ·").unwrap()];
    let column = u16::try_from(before.chars().count()).unwrap();
    assert!(
        style_at(&mut app, 130, 14, column, crumb_y)
            .add_modifier
            .contains(Modifier::BOLD)
    );
}

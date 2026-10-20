//! What the dashboard tests share: snapshots and apps made by hand, and the drawn screen.

use crate::dashboard::app::{App, Effect, Opened, Request, Scroll};
use crate::dashboard::draw;
use crate::dashboard::snapshot::{
    AgentInfo, Counts, Detail, InView, InboxEntry, ItemListing, Links, Listing, MovedBack, Place,
    SessionInfo, Snapshot,
};
use crate::theme;
use crate::view::{Flag, ItemGroup, ItemRefView, ItemSummary, SessionStatus};
use jiff::tz::TimeZone;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Color, Style};
use serde_json::json;
use wf_core::graph::Readiness;
use wf_core::id::{CommentId, HashId, ItemId, SessionId};
use wf_core::identity::AgentId;
use wf_core::item::{ItemState, Priority};
use wf_core::name::AgentName;
use wf_core::session::EndReason;
use wf_core::session::SessionRole;
use wf_core::stats::{AgentStats, ProjectStats};
use wf_harness::{Direction, Permission, TranscriptEntry};

/// 2026-10-14 07:00 UTC.
pub(super) const NOW_MS: u64 = 1_792_047_600_000;

/// `app` drawn once into a test terminal: the text of the screen and each of its cells, which
/// are read as often as the test wants without drawing again.
pub(super) struct Drawn {
    /// The screen as the test backend prints it, each line quoted.
    text: String,
    buffer: Buffer,
}

impl Drawn {
    pub(super) fn of(app: &mut App, width: u16, height: u16) -> Self {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw::draw(frame, app)).unwrap();
        Self {
            text: terminal.backend().to_string(),
            buffer: terminal.backend().buffer().clone(),
        }
    }

    /// The lines of the screen, unquoted.
    pub(super) fn lines(&self) -> Vec<String> {
        self.text
            .lines()
            .map(|line| line.trim_matches('"').to_owned())
            .collect()
    }

    pub(super) fn buffer(&self) -> &Buffer {
        &self.buffer
    }

    /// The column and the row `text` starts at.
    pub(super) fn find(&self, text: &str) -> (u16, u16) {
        self.text
            .lines()
            .enumerate()
            .find_map(|(row, line)| {
                // The backend quotes each line, which the column leaves out.
                let before = &line[1..line.find(text)?];
                let column = u16::try_from(before.chars().count()).unwrap();
                Some((column, u16::try_from(row).unwrap()))
            })
            .unwrap_or_else(|| panic!("{text:?} is not on screen:\n{}", self.text))
    }

    pub(super) fn style_at(&self, x: u16, y: u16) -> Style {
        self.buffer[(x, y)].style()
    }

    pub(super) fn bg_at(&self, x: u16, y: u16) -> Option<Color> {
        self.style_at(x, y).bg
    }

    pub(super) fn symbol_at(&self, x: u16, y: u16) -> &str {
        self.buffer[(x, y)].symbol()
    }
}

/// The screen of `app` as one look at it: the text of [`Drawn`].
pub(super) fn screen(app: &mut App, width: u16, height: u16) -> String {
    Drawn::of(app, width, height).text
}

/// The column and the row `text` starts at once `app` is drawn: [`Drawn::find`] for one look.
pub(super) fn find(app: &mut App, width: u16, height: u16, text: &str) -> (u16, u16) {
    Drawn::of(app, width, height).find(text)
}

pub(super) fn item(seed: u8, title: &str) -> ItemRefView {
    ItemRefView {
        id: ItemId::from_bytes([seed; 32]),
        item_type: "task".to_owned(),
        title: title.to_owned(),
        state: ItemState::Open,
        priority: None,
    }
}

pub(super) fn session(seed: u8, agent: &str, item: ItemRefView, minutes: u64) -> SessionInfo {
    SessionInfo {
        view: session_view(seed, agent, &item, SessionStatus::Live, minutes),
        agent_name: AgentName::new(agent).unwrap(),
        place: Place::Here,
    }
}

/// The item `id` as `wf item show` prints it, `text`, in view with its latest `session`.
pub(super) fn item_in_view(id: ItemId, text: String, session: Option<SessionId>) -> InView {
    InView::Item { id, text, session }
}

/// The session `info` in view, while it lives or without a session of its item after it.
pub(super) fn session_in_view(info: SessionInfo) -> InView {
    InView::Session {
        info: Box::new(info),
        successor: None,
    }
}

/// `info` with another status.
pub(super) fn with_status(info: SessionInfo, status: SessionStatus) -> SessionInfo {
    SessionInfo {
        view: crate::view::SessionView {
            status,
            ..info.view
        },
        ..info
    }
}

pub(super) fn accent() -> Color {
    theme::accent().into()
}

pub(super) fn accent_dimmed() -> Color {
    theme::accent_dimmed().into()
}

/// The style of the label of the tab on screen, focused and not.
pub(super) fn tab_active() -> Style {
    theme::tab_active().into()
}

pub(super) fn tab_active_dimmed() -> Style {
    theme::tab_active_dimmed().into()
}

pub(super) fn snapshot() -> Snapshot {
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
            InboxEntry::Permission {
                session: coder.id,
                since_ms: NOW_MS - 5_000,
                agent: "coder".to_owned(),
                item: Some(parse),
                what: "rm -rf target".to_owned(),
            },
            InboxEntry::Blocker {
                item: spans,
                comment: CommentId::from_bytes([21; 32]),
                by: "coder".to_owned(),
                text: "Should the span live on ParseError or Location?".to_owned(),
            },
            InboxEntry::Review {
                item: archive,
                commit: None,
                stale: None,
            },
        ],
        working: vec![coder, parked, reviewer],
        flagged: vec![Flag::Stranded {
            item: item(4, "Watch").id,
            title: "Watch".to_owned(),
        }],
        awaiting_commits: Vec::new(),
        checkout_target: None,
        in_view: None,
        items: Vec::new(),
        agents: Vec::new(),
        stats: ProjectStats::default(),
        links: Links::default(),
    }
}

pub(super) fn app() -> App {
    let mut app = App::new(snapshot(), TimeZone::UTC);
    app.resize(60, 24);
    app.logged([
        "06:59  #aibaea  @ada · resolved a91d\n         Fix the formatting of the body, nothing else\n".to_owned(),
        "07:04  #ambqga  @ada/coder/pp74s3 · submitted 3e8a1c → in_review\n".to_owned(),
    ]);
    app
}

/// How the items are listed, while the Items tab is on screen.
pub(super) fn item_listing(app: &App) -> Option<ItemListing> {
    match app.wanted().listing {
        Some(Listing::Items(listing)) => Some(listing),
        Some(Listing::Agents) | None => None,
    }
}

pub(super) fn press(app: &mut App, code: KeyCode) -> Option<Effect> {
    app.key(KeyEvent::from(code))
}

/// `ctrl+o`: back to where a link was followed.
pub(super) fn jump_back(app: &mut App) -> Option<Effect> {
    app.key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL))
}

pub(super) fn entry(
    seconds: i64,
    direction: Direction,
    message: serde_json::Value,
) -> TranscriptEntry {
    TranscriptEntry {
        at: jiff::Timestamp::from_second(seconds).unwrap(),
        direction,
        message,
    }
}

pub(super) fn update(seconds: i64, update: serde_json::Value) -> TranscriptEntry {
    entry(
        seconds,
        Direction::FromAgent,
        json!({"method": "session/update", "params": {"update": update}}),
    )
}

/// What the coder of `#parse` has done so far: read a file, and started the tests.
pub(super) fn coder_transcript() -> Vec<TranscriptEntry> {
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

/// [`snapshot`] with the names its text can link to, and the item `#spans` open, whose text
/// mentions another item, an agent, a session, a principal and a commit.
pub(super) fn with_links() -> Snapshot {
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
    snapshot.in_view = Some(item_in_view(
        spans,
        format!(
            "◐ #{} (task:in_progress) Fix error spans\n  review: merged a8d49484\n\nAfter #{}, ask @ada/coder.\n\n  07:00  @ada/coder/{} · commented\n  07:01  @ada · moved to review\n",
            spans.short(),
            parse.short(),
            coder.short(),
        ),
        None,
    ));
    snapshot
}

/// The app with `#spans` open on the Now tab.
pub(super) fn app_with_links() -> App {
    let mut app = app();
    app.open_item(ItemId::from_bytes([2; 32]));
    app.refresh(with_links());
    app
}

/// The permission request of the coder in the snapshot.
pub(super) fn coders_request() -> Request {
    Request {
        session: SessionId::from_bytes([11; 32]),
        since_ms: NOW_MS - 5_000,
    }
}

/// A diff of two files: one changed in two places, one added.
pub(super) const DIFF: &str = "\
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

pub(super) fn agent_view(name: &str, harness: &str, model: Option<&str>) -> crate::view::AgentView {
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
pub(super) fn with_agents() -> Snapshot {
    let mut snapshot = snapshot();
    let principal = wf_core::testing::Fixture::new(1).founder.key();
    let ended = with_status(
        session(14, "coder", item(4, "Dashboard"), 300),
        SessionStatus::Ended {
            reason: EndReason::Submitted,
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

/// Asserts that the highlight `text` on screen has half blocks around it in `background`, on the
/// background of the line, and `background` itself under its text.
pub(super) fn assert_ends(app: &mut App, width: u16, height: u16, text: &str, background: Color) {
    let drawn = Drawn::of(app, width, height);
    let (column, row) = drawn.find(&format!("▐{text}▌"));
    let end = column + text.chars().count() as u16 + 1;
    for (x, symbol) in [(column, "▐"), (end, "▌")] {
        assert_eq!(drawn.symbol_at(x, row), symbol);
        let style = drawn.style_at(x, row);
        assert_eq!((style.fg, style.bg), (Some(background), Some(Color::Reset)));
    }
    assert_eq!(drawn.bg_at(column + 1, row), Some(background));
}

pub(super) fn conflict(seed: u8) -> MovedBack {
    MovedBack {
        item: ItemId::from_bytes([seed; 32]),
        reason: "merging into main conflicts in README.md".to_owned(),
    }
}

/// The footer: the rule, the latest-event line and the keys.
pub(super) fn footer(app: &mut App, width: u16) -> String {
    let screen = screen(app, width, 24);
    let lines: Vec<&str> = screen.lines().collect();
    lines[lines.len() - 3..].join("\n")
}

pub(super) fn item_row(seed: u8, title: &str, state: ItemState) -> ItemSummary {
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
pub(super) fn with_items() -> Snapshot {
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

/// The Items tab with its last item, scrolled two lines down from where it opened.
pub(super) fn scrolled_item(width: u16) -> (App, Opened) {
    let mut app = app();
    app.resize(width, 24);
    press(&mut app, KeyCode::Char('2'));
    let parse = ItemId::from_bytes([1; 32]);
    let mut snapshot = with_items();
    snapshot.in_view = Some(item_in_view(
        parse,
        (1..=40).map(|line| format!("line {line}\n")).collect(),
        None,
    ));
    app.refresh(snapshot);
    press(&mut app, KeyCode::Char('G'));
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    let scrolled = Opened {
        detail: Detail::Item(parse),
        scroll: Scroll::At(2),
    };
    assert_eq!(app.on_screen(), Some(scrolled.clone()));
    (app, scrolled)
}

/// How a session ended `minutes` ago.
pub(super) fn ended(reason: EndReason, minutes: u64) -> SessionStatus {
    SessionStatus::Ended {
        reason,
        at: crate::view::timestamp_ms(NOW_MS - minutes * 60_000),
    }
}

/// `coder` with a live session and two that ended, instructions with markdown, hosted here or
/// only on another device.
pub(super) fn coder_detail(hosted_here: bool) -> crate::view::AgentDetail {
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
                ended(EndReason::Submitted, 120),
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
pub(super) fn app_with_agent(width: u16, detail: &crate::view::AgentDetail) -> App {
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

/// The Items tab in a wide window too short for its thirty items, the first selected.
pub(super) fn long_item_list() -> App {
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

pub(super) fn selected_item(app: &App) -> Option<ItemId> {
    app.items.list.selection.get().copied()
}

/// A session of `coder` as `wf agent show` lists it, `minutes` old, or ended `minutes` ago.
pub(super) fn session_view(
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

pub(super) fn work_commit() -> wf_core::session::CommitId {
    wf_core::session::CommitId::Git("a8d4948400000000000000000000000000000000".parse().unwrap())
}

/// The app with the diff of the archive item open from its review, read already.
pub(super) fn diff_app() -> App {
    let mut app = app();
    let archive = ItemId::from_bytes([3; 32]);
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('D'));
    assert_eq!(app.wanted().detail, Some(Detail::Diff(archive)));
    app.diff_on_top_mut()
        .unwrap()
        .read(Ok(crate::dashboard::diff::ItemWork {
            commit: work_commit(),
            changes: crate::dashboard::diff::Changes::Files(wf_vcs::diff::parse(DIFF)),
            uncommitted: None,
        }));
    app
}

pub(super) fn type_text(app: &mut App, text: &str) {
    for character in text.chars() {
        press(app, KeyCode::Char(character));
    }
}

pub(super) fn permit(answer: Permission) -> Option<Effect> {
    Some(Effect::Permit {
        request: coders_request(),
        answer,
    })
}

/// The row of the latest event, where a tip shows.
pub(super) const LATEST: u16 = 22;

/// A width that leaves room for the latest event and a tip beside it, short of a wide window.
pub(super) const ROOMY: u16 = 110;

/// Asserts that `tip` shows at the right of the latest-event line, a column off the edge.
pub(super) fn assert_tip(app: &mut App, tip: &str) {
    let column = ROOMY - 1 - u16::try_from(tip.chars().count()).unwrap();
    assert_eq!(find(app, ROOMY, 24, tip), (column, LATEST), "{tip}");
}

pub(super) fn usage(seconds: i64, used: u64, size: u64, cost: Option<f64>) -> TranscriptEntry {
    let mut usage = json!({"sessionUpdate": "usage_update", "used": used, "size": size});
    if let Some(amount) = cost {
        usage["cost"] = json!({"amount": amount, "currency": "USD"});
    }
    update(seconds, usage)
}

/// The result of `session/new`, saying the agent runs Opus 5.5.
pub(super) fn session_on_opus() -> TranscriptEntry {
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
pub(super) fn with_usage(app: &mut App) {
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

/// What the coder of `#parse` does after [`coder_transcript`]: the tests pass, it thinks, sums up
/// in markdown and ends its turn; then the next turn starts.
pub(super) fn coder_transcript_continued() -> Vec<TranscriptEntry> {
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

/// The agent `coder` of `@ada`.
pub(super) fn coder_agent() -> AgentId {
    AgentId {
        principal: wf_core::testing::Fixture::new(1).founder.key(),
        name: AgentName::new("coder").unwrap(),
    }
}

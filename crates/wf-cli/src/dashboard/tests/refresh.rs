//! Dashboard tests: refresh.

use super::support::*;
use crate::dashboard::app::{Effect, Status};
use crate::dashboard::snapshot::{Detail, InboxEntry, Snapshot};
use ratatui::crossterm::event::KeyCode;
use wf_core::id::{ItemId, SessionId};

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
    snapshot.in_view = Some(item_in_view(parse, "previous\n".to_owned(), Some(session)));
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
        .read(Ok(crate::dashboard::diff::ItemWork {
            commit: work_commit(),
            changes: crate::dashboard::diff::Changes::NoCommits,
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
    let draft = |text: &str| crate::dashboard::diff::LineComment {
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
    use crate::dashboard::snapshot::{Sources, Wanted};
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
                retry_at_ms: None,
                what: "not logged in".to_owned(),
            },
        )]
        .into(),
    };
    let graph = state.dependency_graph();
    let sources = Sources {
        state: &state,
        graph: &graph,
        events: &events,
        event: &|id| events.iter().find(|event| event.id() == id).cloned(),
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
    assert_eq!(
        crate::view::inbox_count(&state, &[], &faults),
        in_inbox,
        "the count is of the entries"
    );
    assert_eq!(snapshot.waiting.len(), 3);
    let titles: Vec<&str> = snapshot
        .waiting
        .iter()
        .filter_map(|entry| {
            let InboxEntry::MachineFault { title, .. } = entry else {
                return None;
            };
            Some(title.as_str())
        })
        .collect();
    assert_eq!(
        titles,
        ["The disk is full", "The login of claude has lapsed"]
    );
}

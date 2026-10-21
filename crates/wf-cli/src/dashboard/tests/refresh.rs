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
            uncommitted: None,
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
fn the_dashboard_waits_on_as_many_things_as_wf_status_and_shows_the_machine_faults_and_lost_pushes()
{
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
    // Two pushes from the same tip: the later one lost.
    let commit = |byte: u8| -> wf_core::session::CommitId {
        let text: String = std::iter::repeat_n(format!("{byte:02x}"), 20).collect();
        wf_core::session::CommitId::Git(text.parse().unwrap())
    };
    let daemon = wf_core::identity::Actor::Daemon(fixture.founder.key());
    for (now_ms, pushed) in [(2_100, 11), (2_200, 12)] {
        let event = fixture.author_as(
            &state,
            0,
            daemon.clone(),
            now_ms,
            target_moved(commit(pushed), commit(10)),
        );
        state
            .receive(event.clone(), wf_core::state::Trust::Untrusted)
            .unwrap();
        events.push(event);
    }
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
                retry: wf_daemon::Retry::At(2_000),
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
    let snapshot = Snapshot::take(&sources, &[], &faults, &[], None, None, Wanted::default());
    let own = fixture.devices[0].key();
    let in_inbox = crate::view::inbox_views(&state, own, &[], &faults).len();
    assert_eq!(snapshot.waiting.len(), in_inbox);
    assert_eq!(
        crate::view::inbox_count(&state, own, &[], &faults),
        in_inbox,
        "the count is of the entries"
    );
    assert_eq!(snapshot.waiting.len(), 4);
    let lost: Vec<_> = snapshot
        .waiting
        .iter()
        .filter_map(|entry| {
            let InboxEntry::LostPush { commit, .. } = entry else {
                return None;
            };
            Some(*commit)
        })
        .collect();
    assert_eq!(lost, [commit(12)]);
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

#[test]
fn the_items_list_holds_the_items_found_in_titles_bodies_and_comments() {
    use crate::dashboard::snapshot::{ItemListing, Listing, Sources, Wanted};
    use wf_core::search::Query;
    use wf_core::testing::*;

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
    apply(&mut state, create("task", "Nudges in the title"));
    apply(&mut state, create_with_body("task", "Body", "about nudges"));
    let remarked: ItemId = apply(&mut state, create("task", "Remarked")).id().into();
    apply(&mut state, comment(remarked, "nudges here"));
    apply(&mut state, create("task", "Unrelated"));
    let graph = state.dependency_graph();
    let sources = Sources {
        state: &state,
        graph: &graph,
        events: &events,
        event: &|_| None,
        own: fixture.devices[0].key(),
        now_ms: 3_000,
        detail_width: 80,
    };
    let titles = |search: &str| {
        let wanted = Wanted {
            detail: None,
            listing: Some(Listing::Items(ItemListing {
                grouping: crate::cli::ItemGrouping::State,
                order: crate::cli::ItemOrder::Priority,
                search: Query::parse(search),
            })),
        };
        let snapshot = Snapshot::take(&sources, &[], &Default::default(), &[], None, None, wanted);
        let mut titles: Vec<String> = snapshot
            .items
            .iter()
            .flat_map(|group| group.items.iter().map(|item| item.title.clone()))
            .collect();
        titles.sort();
        titles
    };
    assert_eq!(
        titles("NUDGES"),
        ["Body", "Nudges in the title", "Remarked"]
    );
    assert_eq!(titles("nudges here"), ["Remarked"]);
    assert_eq!(titles("").len(), 4);
}

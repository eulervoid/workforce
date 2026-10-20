//! Pins the shape of every `--json` view. A deliberate change means bumping `SCHEMA_VERSION`
//! and reviewing the snapshot diff.

use std::collections::HashMap;

use wf_cli::view::{self, Envelope};
use wf_core::event::{Change, Event, EventBody, Reviewed, SessionEnded};
use wf_core::id::{EventId, HashId, ItemId};
use wf_core::item::{ItemEntry, Priority};
use wf_core::session::{CommitId, EndReason, Verdict};
use wf_core::state::State;
use wf_core::testing::*;

struct Scenario {
    state: State,
    events: HashMap<EventId, Event>,
    task: ItemId,
    dependency: ItemId,
    done: ItemId,
}

fn scenario() -> Scenario {
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let mut events = HashMap::from([(fixture.genesis.id(), fixture.genesis.clone())]);
    let mut now = 2_000;
    let mut apply = |state: &mut State, body| {
        now += 1_000;
        let event = fixture.apply(state, 0, now, body);
        events.insert(event.id(), event.clone());
        event
    };

    apply(&mut state, define_agent("coder"));
    let dependency: ItemId = apply(&mut state, create("task", "Pick a hash for event ids"))
        .id()
        .into();
    let task: ItemId = apply(
        &mut state,
        create_with_body(
            "task",
            "Implement feed sync",
            &format!("blocked on #{}, ask @coder", dependency.short()),
        ),
    )
    .id()
    .into();
    let done: ItemId = apply(&mut state, create("task", "Write spec")).id().into();
    apply(&mut state, edit(task, Change::Priority(Some(Priority::P1))));
    apply(&mut state, depends_on(task, dependency));
    apply(
        &mut state,
        comment(
            task,
            &format!("see #{}, @ada please check", dependency.short()),
        ),
    );
    apply(&mut state, moved(done, ItemState::Done));
    apply(&mut state, archive(done));
    Scenario {
        state,
        events,
        task,
        dependency,
        done,
    }
}

#[test]
fn item_lists() {
    let scenario = scenario();
    let state = &scenario.state;
    let graph = state.dependency_graph();
    let all: Vec<_> = state
        .items()
        .map(|view| view::summary(state, &graph, view))
        .collect();
    insta::assert_json_snapshot!("item_list", Envelope::new(&all));
}

#[test]
fn item_search() {
    let scenario = scenario();
    let state = &scenario.state;
    let events: Vec<_> = scenario.events.values().cloned().collect();
    let query = wf_core::search::Query::parse("check feed").unwrap();
    let hits = wf_core::search::search(
        state,
        &query,
        &view::comment_texts(&events),
        wf_core::search::SearchScope::LiveAndArchived,
    );
    let graph = state.dependency_graph();
    let found: Vec<_> = hits
        .iter()
        .map(|hit| view::search_hit_view(state, &graph, hit))
        .collect();
    assert_eq!(found.len(), 1);
    insta::assert_json_snapshot!("item_search", Envelope::new(&found));
}

#[test]
fn live_item_detail() {
    let scenario = scenario();
    let state = &scenario.state;
    let ItemEntry::Live(item) = state.item(scenario.task).unwrap() else {
        panic!()
    };
    let events: Vec<_> = scenario.events.values().cloned().collect();
    let detail = view::item_detail(state, &state.dependency_graph(), item, &events);
    insta::assert_json_snapshot!("item_detail", Envelope::new(&detail));
}

#[test]
fn dependency_detail_shows_backlinks_and_dependents() {
    let scenario = scenario();
    let state = &scenario.state;
    let ItemEntry::Live(item) = state.item(scenario.dependency).unwrap() else {
        panic!()
    };
    let events: Vec<_> = scenario.events.values().cloned().collect();
    let detail = view::item_detail(state, &state.dependency_graph(), item, &events);
    insta::assert_json_snapshot!("dependency_detail", Envelope::new(&detail));
}

#[test]
fn events_and_status() {
    let scenario = scenario();
    let mut ordered: Vec<&Event> = scenario.events.values().collect();
    ordered.sort_by_key(|event| (event.content.hlc, event.id()));
    let events: Vec<_> = ordered
        .iter()
        .map(|event| view::event_view(&scenario.state, event, true, &HashMap::new()))
        .collect();
    insta::assert_json_snapshot!("log", Envelope::new(&events));
    insta::assert_json_snapshot!(
        "status",
        Envelope::new(view::status(
            &scenario.state,
            &scenario.state.dependency_graph(),
            view::StatusContext {
                machine_faults: &Default::default(),
                own: None,
                last_seen: &Default::default(),
                daemon: false,
                waiting_on_you: 0,
                refolded: false,
                caches: None,
            }
        ))
    );
    insta::assert_json_snapshot!("agents", Envelope::new(view::agent_views(&scenario.state)));
    let _ = scenario.done;
}

/// A session that released with a note, and a verdict with the comment that says why; and how
/// the item went.
#[test]
fn outcomes_in_the_log() {
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let mut events = Vec::new();
    let mut now = 2_000;
    let mut apply = |state: &mut State, body| {
        now += 1_000;
        let event = fixture.apply(state, 0, now, body);
        events.push(event.clone());
        event
    };
    apply(&mut state, define_agent("coder"));
    let item: ItemId = apply(&mut state, create("task", "Implement feed sync"))
        .id()
        .into();
    let session = apply(&mut state, start_session("coder", item)).id().into();
    let note = apply(&mut state, comment(item, "the relay is down"))
        .id()
        .into();
    apply(
        &mut state,
        EventBody::SessionEnded(SessionEnded {
            session,
            reason: EndReason::Released,
            because: Some(note),
            head: None,
        }),
    );
    let commit = CommitId::Git("00112233445566778899aabbccddeeff00112233".parse().unwrap());
    let why = apply(&mut state, comment(item, "needs a test")).id().into();
    apply(
        &mut state,
        EventBody::Reviewed(Reviewed {
            item,
            commit: Some(commit),
            verdict: Verdict::RequestChanges,
            because: Some(why),
        }),
    );
    let comments = view::comment_texts(&events);
    let views: Vec<_> = events
        .iter()
        .rev()
        .take(4)
        .rev()
        .filter(|event| !matches!(event.content.body, EventBody::Commented(_)))
        .map(|event| view::event_view(&state, event, true, &comments))
        .collect();
    insta::assert_json_snapshot!("log_outcomes", Envelope::new(&views));
    insta::assert_json_snapshot!("item_stats", Envelope::new(view::item_stats_views(&state)));
}

#[test]
fn sessions() {
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    fixture.apply(&mut state, 0, 2_000, define_agent("coder"));
    let item: ItemId = fixture
        .apply(&mut state, 0, 3_000, create("task", "Implement feed sync"))
        .id()
        .into();
    let known = fixture.apply(&mut state, 0, 4_000, start_session("coder", item));
    fixture.apply(&mut state, 0, 5_000, start_session("coder", item));
    let sessions = view::session_views(&state, |session| {
        (session == known.id().into()).then(|| "opus".to_string())
    });
    insta::assert_json_snapshot!("sessions", Envelope::new(&sessions));
}

#[test]
fn agent_detail() {
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let mut events = Vec::new();
    let mut apply = |state: &mut State, now, body| {
        events.push(fixture.apply(state, 0, now, body));
    };
    apply(&mut state, 2_000, define_agent("coder"));
    apply(&mut state, 3_000, define_agent_with("coder", &["rust"]));
    let item: ItemId = fixture
        .apply(&mut state, 0, 4_000, create("task", "Implement feed sync"))
        .id()
        .into();
    apply(&mut state, 5_000, start_session("coder", item));
    let agent = state.agents().next().unwrap().0.clone();
    let detail = view::agent_detail(&state, &agent, &events, Some(fixture.devices[0].key()));
    insta::assert_json_snapshot!("agent_detail", Envelope::new(detail.unwrap()));
}

//! Pins the shape of every `--json` view. A deliberate change means bumping `SCHEMA_VERSION`
//! and reviewing the snapshot diff.

use std::collections::HashMap;

use wf_cli::view::{self, Envelope};
use wf_core::event::{Change, Event};
use wf_core::id::{EventId, HashId, ItemId};
use wf_core::item::{ItemView, Priority};
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
    apply(&mut state, moved(done, "done"));
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
fn live_item_detail() {
    let scenario = scenario();
    let state = &scenario.state;
    let ItemView::Live(item) = state.item(scenario.task).unwrap() else {
        panic!()
    };
    let events: Vec<_> = scenario.events.values().cloned().collect();
    let detail = view::item_detail(state, item, &events);
    insta::assert_json_snapshot!("item_detail", Envelope::new(&detail));
}

#[test]
fn dependency_detail_shows_backlinks_and_dependents() {
    let scenario = scenario();
    let state = &scenario.state;
    let ItemView::Live(item) = state.item(scenario.dependency).unwrap() else {
        panic!()
    };
    let events: Vec<_> = scenario.events.values().cloned().collect();
    let detail = view::item_detail(state, item, &events);
    insta::assert_json_snapshot!("dependency_detail", Envelope::new(&detail));
}

#[test]
fn events_and_status() {
    let scenario = scenario();
    let mut ordered: Vec<&Event> = scenario.events.values().collect();
    ordered.sort_by_key(|event| (event.content.hlc, event.id()));
    let events: Vec<_> = ordered
        .iter()
        .map(|event| view::event_view(&scenario.state, event, true))
        .collect();
    insta::assert_json_snapshot!("log", Envelope::new(&events));
    insta::assert_json_snapshot!(
        "status",
        Envelope::new(view::status(
            &scenario.state,
            view::StatusContext {
                own: None,
                last_seen: &Default::default(),
                daemon: false,
                waiting_on_you: 0,
                refolded: false,
            }
        ))
    );
    insta::assert_json_snapshot!("agents", Envelope::new(view::agent_views(&scenario.state)));
    let _ = scenario.done;
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

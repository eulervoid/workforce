//! Dashboard tests: what the feed keeps of the log, and that it shows what the whole log did.

use wf_core::event::Event;
use wf_core::id::{HashId, ItemId};
use wf_core::state::State;
use wf_core::testing::{Fixture, comment, create, create_with_body};

use crate::dashboard::snapshot::{Detail, InView, Sources};
use crate::dashboard::{EventsOf, log_lines};

/// A log of two items, where the second refers to the first: the first item and its log.
fn log() -> (Fixture, State, ItemId, Vec<Event>) {
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let mut events = Vec::new();
    let mut apply = |state: &mut State, body| {
        let event = fixture.apply(state, 0, 2_000 + events.len() as u64, body);
        events.push(event.clone());
        event
    };
    let item: ItemId = apply(&mut state, create("task", "Parse")).id().into();
    apply(&mut state, comment(item, "first remark"));
    let other: ItemId = apply(
        &mut state,
        create_with_body("task", "Lex", &format!("Needed by #{}", item.short())),
    )
    .id()
    .into();
    apply(&mut state, comment(other, "unrelated"));
    apply(&mut state, comment(item, "second remark"));
    (fixture, state, item, events)
}

/// The events of `item`, as the workspace gives them.
fn item_events(state: &State, item: ItemId, events: &[Event]) -> Vec<Event> {
    events
        .iter()
        .filter(|event| state.can_be_activity_of(item, event))
        .cloned()
        .collect()
}

#[test]
fn the_log_tab_reads_new_events_against_the_events_of_their_items_as_against_the_whole_log() {
    let (_, state, _, events) = log();
    let new = &events[2..];
    let of_items = |item| item_events(&state, item, &events);
    let read: Vec<(ItemId, Vec<Event>)> = new
        .iter()
        .filter_map(|event| state.item_of(event))
        .map(|item| (item, of_items(item)))
        .collect();
    let by_item = |item| {
        read.iter()
            .find(|(read, _)| *read == item)
            .map_or(&[][..], |(_, events)| events.as_slice())
    };
    let lines = log_lines(&state, &by_item, new, 100);
    assert_eq!(lines, log_lines(&state, &|_| &events, new, 100));
    assert_eq!(lines.len(), 3, "{lines:#?}");
}

#[test]
fn an_item_kept_up_with_since_it_was_opened_shows_what_the_whole_log_did() {
    let (fixture, state, item, events) = log();
    let of = EventsOf::Item(item);
    // Opened after the first remark; the rest came in at refreshes.
    let mut kept = item_events(&state, item, &events[..2]);
    kept.extend(
        events[2..]
            .iter()
            .filter(|event| of.concerns(&state, event))
            .cloned(),
    );
    assert_eq!(kept, item_events(&state, item, &events));
    assert!(
        kept.len() < events.len(),
        "the other item's own are left out"
    );

    let graph = state.dependency_graph();
    let detail = |detail_events: &[Event]| {
        let sources = Sources {
            state: &state,
            graph: &graph,
            detail_events,
            comment_texts: &Default::default(),
            event: &|_| None,
            own: fixture.devices[0].key(),
            now_ms: 9_000,
            detail_width: 80,
        };
        match InView::take(&sources, &[], Detail::Item(item)) {
            Some(InView::Item { text, .. }) => text,
            other => panic!("{other:?}"),
        }
    };
    let text = detail(&kept);
    assert_eq!(text, detail(&events));
    for shown in ["first remark", "second remark", "Lex"] {
        assert!(text.contains(shown), "{shown} in {text}");
    }
}

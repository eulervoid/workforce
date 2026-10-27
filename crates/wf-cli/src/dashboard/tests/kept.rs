//! Dashboard tests: what the feed keeps of the log, and that it shows what the whole log did.

use wf_core::event::Event;
use wf_core::id::{HashId, ItemId};
use wf_core::state::State;
use wf_core::testing::{Fixture, comment, create, create_with_body};

use crate::dashboard::snapshot::{Detail, InView, Sources};
use crate::dashboard::{ITEMS_KEPT, KeptEvents, log_lines};

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

/// The events of any of `items`, as the workspace gives them.
fn item_events(state: &State, items: &[ItemId], events: &[Event]) -> Vec<Event> {
    events
        .iter()
        .filter(|event| {
            items
                .iter()
                .any(|item| state.can_be_activity_of(*item, event))
        })
        .cloned()
        .collect()
}

#[test]
fn the_log_tab_reads_new_events_against_the_events_of_their_items_as_against_the_whole_log() {
    let (_, state, _, events) = log();
    let new = &events[2..];
    // The items the new events are about, read in one go.
    let touched: Vec<ItemId> = new
        .iter()
        .filter_map(|event| state.item_of(event))
        .collect();
    let mut kept = KeptEvents::new(ITEMS_KEPT);
    kept.keep(&state, &touched, &item_events(&state, &touched, &events));
    let lines = log_lines(&state, &|item| kept.of(item), new, 100);
    assert_eq!(lines, log_lines(&state, &|_| &events, new, 100));
    assert_eq!(lines.len(), 3, "{lines:#?}");
}

#[test]
fn an_item_kept_up_with_since_it_was_opened_shows_what_the_whole_log_did() {
    let (fixture, state, item, events) = log();
    let mut kept = KeptEvents::new(ITEMS_KEPT);
    // Opened when the feed had read two events and the log had three; the rest, the third among
    // them, came in at a refresh.
    kept.keep(&state, &[item], &item_events(&state, &[item], &events[..3]));
    kept.keep_up(&state, &events[2..]);
    assert_eq!(kept.of(item), item_events(&state, &[item], &events));
    assert!(
        kept.of(item).len() < events.len(),
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
    let text = detail(kept.of(item));
    assert_eq!(text, detail(&events));
    for shown in ["first remark", "second remark", "Lex"] {
        assert!(text.contains(shown), "{shown} in {text}");
    }
}

#[test]
fn only_the_items_opened_or_touched_last_are_kept() {
    let (_, state, item, events) = log();
    let other: ItemId = events[2].id().into();
    let mut kept = KeptEvents::new(1);
    kept.keep(&state, &[item], &item_events(&state, &[item], &events));
    kept.keep(&state, &[other], &item_events(&state, &[other], &events));
    kept.trim();
    assert_eq!(kept.of(item), &[][..], "let go of");
    assert_eq!(kept.of(other), item_events(&state, &[other], &events));
    // New events of an item let go of are left to be read with it.
    kept.keep_up(&state, &events);
    assert_eq!(kept.of(item), &[][..]);
}

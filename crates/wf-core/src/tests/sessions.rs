use crate::event::Event;
use crate::id::{ItemId, SessionId};
use crate::session::EndReason;
use crate::state::{State, Trust};
use crate::testing::*;
use crate::validate::Invalid;

fn item_id(event: &Event) -> ItemId {
    event.id().into()
}

fn session_id(event: &Event) -> SessionId {
    event.id().into()
}

/// A state with an agent `coder` and one task.
fn with_task() -> (Fixture, State, ItemId) {
    let fixture = Fixture::new(2);
    let mut state = fixture.state();
    fixture.apply(&mut state, 0, 2_000, define_agent("coder"));
    let task = item_id(&fixture.apply(&mut state, 0, 2_001, create("task", "work")));
    (fixture, state, task)
}

#[test]
fn a_session_is_live_until_it_ends() {
    let (fixture, mut state, task) = with_task();
    let started = fixture.apply(&mut state, 0, 3_000, start_session("coder", task));
    let session = session_id(&started);
    {
        let folded = state.session(session).unwrap();
        assert!(folded.is_live());
        assert_eq!(folded.item(), task);
        assert_eq!(folded.agent().name.as_str(), "coder");
        assert_eq!(folded.device(), fixture.devices[0].key());
    }
    assert_eq!(state.live_sessions().count(), 1);

    fixture.apply(
        &mut state,
        0,
        3_001,
        end_session(session, EndReason::Released),
    );
    let ended = state.session(session).unwrap().ended().unwrap();
    assert_eq!(ended.reason, EndReason::Released);
    assert_eq!(state.live_sessions().count(), 0);
}

#[test]
fn the_earliest_end_counts() {
    let (fixture, mut state, task) = with_task();
    let session = session_id(&fixture.apply(&mut state, 0, 3_000, start_session("coder", task)));
    let cancelled = fixture.author(&state, 1, 3_001, end_session(session, EndReason::Cancelled));
    let failed = fixture.author(&state, 0, 3_002, end_session(session, EndReason::Failed));
    let mut forward = state.clone();
    let mut backward = state;
    for event in [&cancelled, &failed] {
        forward.receive((*event).clone(), Trust::Untrusted).unwrap();
    }
    for event in [&failed, &cancelled] {
        backward
            .receive((*event).clone(), Trust::Untrusted)
            .unwrap();
    }
    assert_eq!(forward, backward);
    let reason = forward.session(session).unwrap().ended().unwrap().reason;
    let earlier = if cancelled.content.hlc < failed.content.hlc {
        EndReason::Cancelled
    } else {
        EndReason::Failed
    };
    assert_eq!(reason, earlier);
}

#[test]
fn the_claim_belongs_to_the_earliest_live_work_session() {
    let (fixture, mut state, task) = with_task();
    let first = fixture.author(&state, 0, 3_000, start_session("coder", task));
    let second = fixture.author(&state, 1, 3_001, start_session("coder", task));
    let mut forward = state.clone();
    for event in [&first, &second] {
        forward.receive((*event).clone(), Trust::Untrusted).unwrap();
    }
    for event in [&second, &first] {
        state.receive((*event).clone(), Trust::Untrusted).unwrap();
    }
    assert_eq!(forward, state);

    let earlier = if first.content.hlc < second.content.hlc {
        &first
    } else {
        &second
    };
    let later = if std::ptr::eq(earlier, &first) {
        &second
    } else {
        &first
    };
    assert_eq!(state.claim(task).unwrap().id(), session_id(earlier));

    fixture.apply(
        &mut state,
        0,
        4_000,
        end_session(session_id(earlier), EndReason::Cancelled),
    );
    assert_eq!(state.claim(task).unwrap().id(), session_id(later));
}

#[test]
fn sessions_need_a_defined_agent_and_a_known_item() {
    let (fixture, mut state, task) = with_task();
    let unknown_agent = fixture.author(&state, 0, 3_000, start_session("ghost", task));
    assert_eq!(
        state.receive(unknown_agent, Trust::Untrusted).unwrap_err(),
        Invalid::UnknownAgent
    );
    let missing = ItemId::from(crate::id::EventId::of(b"missing"));
    let unknown_item = fixture.author(&state, 0, 3_000, start_session("coder", missing));
    assert_eq!(
        state.receive(unknown_item, Trust::Untrusted).unwrap_err(),
        Invalid::UnknownItem(missing)
    );
}

#[test]
fn ending_an_unknown_session_is_refused() {
    let (fixture, mut state, _) = with_task();
    let missing = SessionId::from(crate::id::EventId::of(b"missing"));
    let event = fixture.author(&state, 0, 3_000, end_session(missing, EndReason::Failed));
    assert_eq!(
        state.receive(event, Trust::Untrusted).unwrap_err(),
        Invalid::UnknownSession(missing)
    );
}

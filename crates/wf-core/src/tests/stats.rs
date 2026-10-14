use crate::event::Event;
use crate::id::{ItemId, SessionId};
use crate::identity::AgentId;
use crate::item::ItemState;
use crate::name::AgentName;
use crate::session::EndReason;
use crate::stats::RECENT_MS;
use crate::testing::*;

fn item_id(event: &Event) -> ItemId {
    event.id().into()
}

fn session_id(event: &Event) -> SessionId {
    event.id().into()
}

const HOUR: u64 = 60 * 60 * 1000;

#[test]
fn stats_count_what_was_done_lately_and_how_attempts_ended() {
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let start = 10 * RECENT_MS;
    fixture.apply(&mut state, 0, start, define_agent("coder"));
    let old = item_id(&fixture.apply(&mut state, 0, start, create("task", "old")));
    let new = item_id(&fixture.apply(&mut state, 0, start, create("task", "new")));
    // `old` was done long ago.
    let first = session_id(&fixture.apply(&mut state, 0, start, start_session("coder", old)));
    fixture.apply(
        &mut state,
        0,
        start + HOUR,
        end_session(first, EndReason::Released),
    );
    fixture.apply(&mut state, 0, start + HOUR, moved(old, ItemState::Done));
    // `new` took a failed attempt and a cancelled one, then two hours of work to be done.
    let now = start + 2 * RECENT_MS;
    let failed =
        session_id(&fixture.apply(&mut state, 0, now - 5 * HOUR, start_session("coder", new)));
    fixture.apply(
        &mut state,
        0,
        now - 4 * HOUR,
        end_session(failed, EndReason::Failed),
    );
    let stopped =
        session_id(&fixture.apply(&mut state, 0, now - 4 * HOUR, start_session("coder", new)));
    fixture.apply(
        &mut state,
        0,
        now - 3 * HOUR,
        end_session(stopped, EndReason::Cancelled),
    );
    let done =
        session_id(&fixture.apply(&mut state, 0, now - 3 * HOUR, start_session("coder", new)));
    fixture.apply(
        &mut state,
        0,
        now - HOUR,
        end_session(done, EndReason::Submitted { commit: None }),
    );
    fixture.apply(&mut state, 0, now - HOUR, moved(new, ItemState::Done));
    let live = item_id(&fixture.apply(&mut state, 0, now, create("task", "live")));
    fixture.apply(&mut state, 0, now, start_session("coder", live));

    let project = state.project_stats(now);
    assert_eq!(project.done_lately, 1);
    // From the first work session of `new` to its move to done.
    assert_eq!(project.median_lead_ms, Some(4 * HOUR));
    // The released, the failed and the submitted session; not the cancelled one.
    assert_eq!((project.failed_attempts, project.attempts), (2, 3));

    let coder = AgentId {
        principal: fixture.founder.key(),
        name: AgentName::new("coder").unwrap(),
    };
    let agent = state.agent_stats(&coder);
    assert_eq!(agent.sessions, 5);
    assert_eq!(agent.live, 1);
    assert_eq!((agent.finished, agent.failed), (1, 2));
    // One, one, one and two hours.
    assert_eq!(agent.median_duration_ms, Some(HOUR));
}

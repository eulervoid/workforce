//! Times the state queries the dispatcher and the inbox run, on a large generated state:
//! `cargo bench -p wf-core --features testing`.

use std::hint::black_box;
use std::time::Instant;

use wf_core::autonomy::Autonomy;
use wf_core::event::{AgentDefined, EventBody};
use wf_core::id::SessionId;
use wf_core::identity::AgentId;
use wf_core::name::{AgentName, HarnessName, LaneName};
use wf_core::session::{CommitId, EndReason, Verdict};
use wf_core::testing::*;

const ITEMS: usize = 2_000;

fn commit() -> CommitId {
    CommitId::Git("00112233445566778899aabbccddeeff00112233".parse().unwrap())
}

fn time<T>(label: &str, runs: u32, mut query: impl FnMut() -> T) {
    let started = Instant::now();
    for _ in 0..runs {
        black_box(query());
    }
    println!("{label:<24}{:>12.2?} per run", started.elapsed() / runs);
}

fn main() {
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let mut clock = 2_000;
    let mut apply = |state: &mut _, body: EventBody| {
        clock += 1;
        fixture.apply(state, 0, clock, body)
    };
    apply(
        &mut state,
        EventBody::AgentDefined(AgentDefined {
            name: AgentName::new("coder").unwrap(),
            harness: HarnessName::new("opencode").unwrap(),
            model: None,
            instructions: None,
            capabilities: vec![],
            lanes: vec![LaneName::new("coding").unwrap()],
            autonomy: Autonomy::default(),
        }),
    );
    for index in 0..ITEMS {
        let body = "- [ ] first step\n- [ ] second step\n- [ ] third step";
        let item = apply(
            &mut state,
            create_with_body("task", &format!("item {index}"), body),
        )
        .id()
        .into();
        match index % 4 {
            // Never started.
            0 => {}
            // Released twice.
            1 => {
                for _ in 0..2 {
                    let session: SessionId =
                        apply(&mut state, start_session("coder", item)).id().into();
                    apply(&mut state, end_session(session, EndReason::Released));
                }
            }
            // Submitted and awaiting review.
            2 => {
                let session: SessionId =
                    apply(&mut state, start_session("coder", item)).id().into();
                apply(&mut state, submit_session(session, commit()));
                apply(&mut state, moved(item, ItemState::Review));
            }
            // Sent back by review.
            _ => {
                let session: SessionId =
                    apply(&mut state, start_session("coder", item)).id().into();
                apply(&mut state, submit_session(session, commit()));
                apply(&mut state, moved(item, ItemState::Review));
                apply(&mut state, verdict(item, commit(), Verdict::RequestChanges));
                apply(&mut state, blocker(item, "which way?"));
            }
        }
    }
    let agent = AgentId {
        principal: fixture.founder.key(),
        name: AgentName::new("coder").unwrap(),
    };
    let device = fixture.devices[0].key();
    println!("{ITEMS} items, {} sessions", state.sessions().count());
    time("inbox", 5, || state.inbox());
    time("dispatchable", 5, || {
        state.dispatchable(device, &agent, &Default::default())
    });
    time("open blockers", 5, || state.open_blockers());
}

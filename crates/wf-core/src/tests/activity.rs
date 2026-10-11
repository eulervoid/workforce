use crate::activity::Activity;
use crate::event::{Event, EventBody, ItemMoved};
use crate::id::{HashId, ItemId, SessionId};
use crate::name::StateName;
use crate::session::EndReason;
use crate::state::State;
use crate::testing::*;

struct Log {
    fixture: Fixture,
    state: State,
    events: Vec<Event>,
    clock: u64,
}

impl Log {
    fn new() -> Self {
        let fixture = Fixture::new(1);
        let state = fixture.state();
        let mut log = Self {
            fixture,
            state,
            events: Vec::new(),
            clock: 2_000,
        };
        log.apply(define_agent("coder"));
        log
    }

    fn apply(&mut self, body: EventBody) -> Event {
        self.clock += 1;
        let event = self.fixture.apply(&mut self.state, 0, self.clock, body);
        self.events.push(event.clone());
        event
    }

    fn activity(&self, item: ItemId) -> Vec<Activity> {
        self.state
            .activity(item, &self.events)
            .into_iter()
            .map(|entry| entry.activity)
            .collect()
    }
}

fn moved_for(item: ItemId, to: &str, session: SessionId) -> EventBody {
    EventBody::ItemMoved(ItemMoved {
        item,
        to: StateName::new(to).unwrap(),
        by_session: Some(session),
        because: None,
    })
}

#[test]
fn a_session_shows_the_moves_it_caused_at_its_start_and_end() {
    let mut log = Log::new();
    let task: ItemId = log.apply(create("task", "work")).id().into();
    let session: SessionId = log.apply(start_session("coder", task)).id().into();
    log.apply(moved_for(task, "in_progress", session));
    log.apply(moved_for(task, "done", session));
    log.apply(end_session(session, EndReason::Submitted { commit: None }));
    log.apply(moved(task, "open"));

    assert_eq!(
        log.activity(task),
        [
            Activity::Created,
            Activity::SessionStarted {
                session,
                role: crate::session::SessionRole::Work
            },
            Activity::SessionEnded {
                session,
                reason: EndReason::Submitted { commit: None },
                moved_to: Some(StateName::new("done").unwrap()),
            },
            Activity::Moved {
                to: StateName::new("open").unwrap(),
                because: None
            },
        ]
    );
}

#[test]
fn what_other_items_do_shows_where_it_changes_this_one() {
    let mut log = Log::new();
    let task: ItemId = log.apply(create("task", "work")).id().into();
    let first: ItemId = log.apply(create("task", "first")).id().into();
    let later: ItemId = log.apply(create("task", "later")).id().into();
    log.apply(depends_on(task, first));
    log.apply(depends_on(later, task));
    log.apply(comment(later, &format!("after #{}", task.short())));
    log.apply(moved(first, "in_progress"));
    log.apply(moved(first, "done"));

    let activity = log.activity(task);
    assert_eq!(
        activity[1..],
        [
            Activity::DependsOn(crate::crdt::SetChange::Add(first)),
            Activity::Dependent(crate::crdt::SetChange::Add(later)),
            Activity::ReferencedIn(later),
            Activity::DependencyFinished {
                item: first,
                to: StateName::new("done").unwrap(),
                category: crate::policy::StateCategory::Done,
            },
        ]
    );
}

#[test]
fn a_body_edit_that_lost_to_a_concurrent_one_is_kept() {
    let fixture = Fixture::new(2);
    let mut state = fixture.state();
    let created = fixture.apply(&mut state, 0, 2_000, create("task", "work"));
    let task: ItemId = created.id().into();
    let body = |text: &str| edit(task, crate::event::Change::Body(text.into()));
    let first = fixture.author(&state, 0, 3_000, body("- [ ] mine"));
    let second = fixture.author(&state, 1, 3_001, body("- [ ] theirs"));
    for event in [&first, &second] {
        state
            .receive(event.clone(), crate::state::Trust::Untrusted)
            .unwrap();
    }
    let events = [created, first, second];
    let activity: Vec<Activity> = state
        .activity(task, &events)
        .into_iter()
        .map(|entry| entry.activity)
        .collect();
    assert_eq!(
        activity[1..],
        [
            Activity::BodyOverwritten {
                body: "- [ ] mine".into()
            },
            Activity::Edited(crate::event::Change::Body("- [ ] theirs".into())),
        ]
    );
}

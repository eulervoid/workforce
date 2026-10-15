use crate::activity::Activity;
use crate::event::{Event, EventBody, ItemMoved};
use crate::id::{CommentId, HashId, ItemId, SessionId};
use crate::item::ItemState;
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

fn moved_for(item: ItemId, to: ItemState, session: SessionId) -> EventBody {
    EventBody::ItemMoved(ItemMoved {
        item,
        to: to.into(),
        by_session: Some(session),
        because: None,
    })
}

#[test]
fn a_session_shows_the_moves_it_caused_at_its_start_and_end() {
    let mut log = Log::new();
    let task: ItemId = log.apply(create("task", "work")).id().into();
    let session: SessionId = log.apply(start_session("coder", task)).id().into();
    log.apply(moved_for(task, ItemState::Active, session));
    log.apply(moved_for(task, ItemState::Done, session));
    log.apply(end_session(session, EndReason::Submitted { commit: None }));
    log.apply(moved(task, ItemState::Open));

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
                moved_to: Some(ItemState::Done),
                because: None,
            },
            Activity::Moved {
                to: ItemState::Open,
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
    log.apply(moved(first, ItemState::Active));
    log.apply(moved(first, ItemState::Done));

    let activity = log.activity(task);
    assert_eq!(
        activity[1..],
        [
            Activity::DependsOn(crate::crdt::SetChange::Add(first)),
            Activity::Dependent(crate::crdt::SetChange::Add(later)),
            Activity::ReferencedIn {
                item: later,
                comment: None
            },
            Activity::DependencyFinished {
                item: first,
                to: ItemState::Done,
            },
        ]
    );
}

#[test]
fn a_reference_to_a_comment_names_the_comment() {
    let mut log = Log::new();
    let task: ItemId = log.apply(create("task", "work")).id().into();
    let said: CommentId = log.apply(comment(task, "use humantime")).id().into();
    let other: ItemId = log.apply(create("task", "other")).id().into();
    log.apply(comment(
        other,
        &format!("see #{t}/{c} and #{t}", t = task.short(), c = said.short()),
    ));

    let activity = log.activity(task);
    assert_eq!(
        activity[2..],
        [
            Activity::ReferencedIn {
                item: other,
                comment: None
            },
            Activity::ReferencedIn {
                item: other,
                comment: Some(said)
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

#[test]
fn the_items_created_around_one_are_not_its_creation_but_may_refer_to_it() {
    let mut log = Log::new();
    let before: ItemId = log.apply(create("task", "before")).id().into();
    let task: ItemId = log.apply(create("task", "work")).id().into();
    let mentioning = format!("after #{}", task.short());
    let after: ItemId = log
        .apply(create_with_body("task", "after", &mentioning))
        .id()
        .into();
    log.apply(comment(before, &format!("not #{}", after.short())));
    log.apply(comment(before, &mentioning));
    log.apply(edit(task, crate::event::Change::Body("- [ ] one".into())));

    assert_eq!(
        log.activity(task),
        [
            Activity::Created,
            Activity::ReferencedIn {
                item: after,
                comment: None
            },
            Activity::ReferencedIn {
                item: before,
                comment: None
            },
            Activity::Edited(crate::event::Change::Body("- [ ] one".into())),
        ]
    );
}

#[test]
fn an_archived_item_still_shows_its_creation() {
    let mut log = Log::new();
    log.apply(create("task", "other"));
    let task: ItemId = log.apply(create("task", "work")).id().into();
    log.apply(archive(task));

    assert_eq!(log.activity(task)[0], Activity::Created);
}

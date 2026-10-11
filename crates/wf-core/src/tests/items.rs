use crate::crdt::SetChange;
use crate::event::Change;
use crate::graph::Readiness;
use crate::id::{HashId, ItemId};
use crate::item::{ItemView, Origin, Standing};
use crate::name::{Label, StateName};
use crate::policy::{DEFAULT_POLICY_TOML, StateCategory};
use crate::refs::Mention;
use crate::resolve::{MentionTarget, Source};
use crate::state::State;
use crate::testing::*;

fn item_id(event: &crate::event::Event) -> ItemId {
    event.id().into()
}

fn state_category(state: &State, item: ItemId) -> Option<StateCategory> {
    let policy = &state.policy().unwrap().policy;
    state.item(item).unwrap().standing(policy).state_category()
}

fn ready_ids(state: &State) -> Vec<ItemId> {
    state.ready_items().iter().map(|item| item.id()).collect()
}

#[test]
fn new_items_start_in_the_initial_state_of_their_type() {
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let task = item_id(&fixture.apply(&mut state, 0, 2_000, create("task", "Sync feeds")));
    let decision = item_id(&fixture.apply(&mut state, 0, 2_001, create("decision", "Hash")));
    assert_eq!(state_category(&state, task), Some(StateCategory::Open));
    assert_eq!(
        state_category(&state, decision),
        Some(StateCategory::Blocked)
    );
    let policy = &state.policy().unwrap().policy;
    let ItemView::Live(item) = state.item(task).unwrap() else {
        panic!("live")
    };
    assert!(matches!(
        item.standing(policy),
        Standing::Known { state, .. } if state.as_str() == "open"
    ));
}

#[test]
fn a_task_waits_for_the_task_it_depends_on() {
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let task = item_id(&fixture.apply(&mut state, 0, 2_000, create("task", "Implement feed sync")));
    let first = item_id(&fixture.apply(&mut state, 0, 2_001, create("task", "Pick a hash")));
    assert_eq!(ready_ids(&state).len(), 2);

    fixture.apply(&mut state, 0, 2_002, depends_on(task, first));
    assert_eq!(ready_ids(&state), vec![first]);
    let graph = state.dependency_graph();
    let blockers = state.blockers(&graph, task);
    assert_eq!(blockers.len(), 1);
    assert_eq!(blockers[0].item, first);
    assert_eq!(blockers[0].state_category, Some(StateCategory::Open));
    assert_eq!(graph.dependents(first).collect::<Vec<_>>(), vec![task]);

    fixture.apply(&mut state, 0, 2_003, moved(first, "done"));
    assert_eq!(ready_ids(&state), vec![task]);
}

#[test]
fn only_open_items_are_ready() {
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let task = item_id(&fixture.apply(&mut state, 0, 2_000, create("task", "work")));
    let expectations = [
        ("backlog", StateCategory::Inactive, false),
        ("open", StateCategory::Open, true),
        ("in_progress", StateCategory::Active, false),
        ("in_review", StateCategory::Blocked, false),
        ("done", StateCategory::Done, false),
        ("dropped", StateCategory::Cancelled, false),
    ];
    for (offset, (target, expected_state_category, ready)) in expectations.into_iter().enumerate() {
        fixture.apply(&mut state, 0, 3_000 + offset as u64, moved(task, target));
        assert_eq!(
            state_category(&state, task),
            Some(expected_state_category),
            "{target}"
        );
        assert_eq!(ready_ids(&state).contains(&task), ready, "{target}");
    }
}

fn readiness(state: &State, item: ItemId) -> Option<Readiness> {
    let ItemView::Live(live) = state.item(item).unwrap() else {
        panic!("live")
    };
    state.readiness(&state.dependency_graph(), live)
}

#[test]
fn done_satisfies_dependents_but_cancelled_strands_them() {
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let part = item_id(&fixture.apply(&mut state, 0, 2_000, create("task", "part")));
    let whole = item_id(&fixture.apply(&mut state, 0, 2_001, create("task", "whole")));
    assert_eq!(readiness(&state, whole), Some(Readiness::Ready));
    fixture.apply(&mut state, 0, 2_002, depends_on(whole, part));
    assert_eq!(readiness(&state, whole), Some(Readiness::Blocked));

    fixture.apply(&mut state, 0, 2_003, moved(part, "dropped"));
    let graph = state.dependency_graph();
    let blockers = state.blockers(&graph, whole);
    assert_eq!(blockers.len(), 1);
    assert_eq!(blockers[0].state_category, Some(StateCategory::Cancelled));
    assert!(!ready_ids(&state).contains(&whole));
    assert_eq!(readiness(&state, whole), Some(Readiness::Stranded));
    assert_eq!(readiness(&state, part), None);

    fixture.apply(&mut state, 0, 2_004, moved(part, "done"));
    assert!(ready_ids(&state).contains(&whole));
    assert_eq!(readiness(&state, whole), Some(Readiness::Ready));
}

#[test]
fn archiving_leaves_a_stub_and_keeps_dependencies_resolving() {
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let part = item_id(&fixture.apply(&mut state, 0, 2_000, create("task", "part")));
    let whole = item_id(&fixture.apply(&mut state, 0, 2_001, create("task", "whole")));
    fixture.apply(&mut state, 0, 2_002, depends_on(whole, part));
    fixture.apply(&mut state, 0, 2_003, moved(part, "done"));
    fixture.apply(&mut state, 0, 2_004, archive(part));

    let ItemView::Archived(stub) = state.item(part).unwrap() else {
        panic!("archived items are stubs")
    };
    assert_eq!(stub.title(), "part");
    assert_eq!(state.live_items().count(), 1);
    assert!(
        ready_ids(&state).contains(&whole),
        "done dependency still counts"
    );

    fixture.apply(&mut state, 0, 2_005, unarchive(part));
    assert!(matches!(state.item(part).unwrap(), ItemView::Live(_)));
}

#[test]
fn archive_rules_are_checked_by_the_author() {
    use crate::validate::PolicyViolation;
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let task = item_id(&fixture.apply(&mut state, 0, 2_000, create("task", "work")));
    assert_eq!(
        state.check_policy(&archive(task)),
        Err(PolicyViolation::ArchiveRequiresTerminal {
            state_category: Some(StateCategory::Open)
        })
    );
    assert_eq!(
        state.check_policy(&unarchive(task)),
        Err(PolicyViolation::NotArchived(task))
    );
    fixture.apply(&mut state, 0, 2_001, moved(task, "dropped"));
    assert_eq!(state.check_policy(&archive(task)), Ok(()));
    fixture.apply(&mut state, 0, 2_002, archive(task));
    assert_eq!(
        state.check_policy(&archive(task)),
        Err(PolicyViolation::AlreadyArchived(task))
    );
}

#[test]
fn superseding_moves_the_target_decision_to_superseded() {
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let old = item_id(&fixture.apply(&mut state, 0, 2_000, create("decision", "use sha1")));
    let new = item_id(&fixture.apply(&mut state, 0, 2_001, create("decision", "use blake3")));
    fixture.apply(&mut state, 0, 2_002, moved(old, "ratified"));
    assert_eq!(state.check_policy(&supersedes(new, old)), Ok(()));
    fixture.apply(&mut state, 0, 2_003, supersedes(new, old));
    let policy = &state.policy().unwrap().policy;
    let ItemView::Live(old_item) = state.item(old).unwrap() else {
        panic!()
    };
    assert!(matches!(
        old_item.standing(policy),
        Standing::Known { state, state_category: StateCategory::Done } if state.as_str() == "superseded"
    ));
    let ItemView::Live(new_item) = state.item(new).unwrap() else {
        panic!()
    };
    assert_eq!(new_item.supersedes(), Some(old));
}

#[test]
fn supersedes_is_for_decisions_only() {
    use crate::validate::PolicyViolation;
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let task = item_id(&fixture.apply(&mut state, 0, 2_000, create("task", "a")));
    let decision = item_id(&fixture.apply(&mut state, 0, 2_001, create("decision", "b")));
    assert_eq!(
        state.check_policy(&supersedes(task, decision)),
        Err(PolicyViolation::SupersedesRequiresDecisions)
    );
}

#[test]
fn removed_states_are_flagged_not_lost() {
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let custom = format!(
        "{DEFAULT_POLICY_TOML}\n[types.bug]\nlane = \"default\"\ninitial = \"new\"\n\
         states = [{{ name = \"new\", category = \"open\" }}, {{ name = \"triaged\", category = \"open\" }}, {{ name = \"fixed\", category = \"done\" }}]\n"
    );
    fixture.apply(
        &mut state,
        0,
        2_000,
        fixture.policy_set(&fixture.founder, &custom),
    );
    let bug = item_id(&fixture.apply(&mut state, 0, 2_001, create("bug", "crash")));
    fixture.apply(&mut state, 0, 2_002, moved(bug, "triaged"));
    assert_eq!(state_category(&state, bug), Some(StateCategory::Open));

    let shrunk = custom.replace(r#"{ name = "triaged", category = "open" }, "#, "");
    fixture.apply(
        &mut state,
        0,
        2_003,
        fixture.policy_set(&fixture.founder, &shrunk),
    );
    let policy = &state.policy().unwrap().policy;
    let ItemView::Live(item) = state.item(bug).unwrap() else {
        panic!()
    };
    assert!(matches!(
        item.standing(policy),
        Standing::Unknown { state: Some(state) } if state.as_str() == "triaged"
    ));
    assert!(!ready_ids(&state).contains(&bug));
}

#[test]
fn author_checks_follow_the_policy() {
    use crate::validate::PolicyViolation;
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let task = item_id(&fixture.apply(&mut state, 0, 2_000, create("task", "a")));
    let decision = item_id(&fixture.apply(&mut state, 0, 2_002, create("decision", "c")));

    assert!(matches!(
        state.check_policy(&create("nonsense", "x")),
        Err(PolicyViolation::UnknownType(_))
    ));
    assert!(matches!(
        state.check_policy(&moved(task, "answered")),
        Err(PolicyViolation::UnknownState { .. })
    ));
    assert!(matches!(
        state.check_policy(&depends_on(decision, task)),
        Err(PolicyViolation::DependencyNotAllowed { .. })
    ));
    assert_eq!(state.check_policy(&depends_on(task, decision)), Ok(()));
    assert!(matches!(
        state.check_policy(&edit(
            decision,
            Change::Priority(Some(crate::item::Priority::P1))
        )),
        Err(PolicyViolation::FieldNotAllowed { .. })
    ));
}

#[test]
fn types_with_priorities_need_one() {
    use crate::event::EventBody;
    use crate::validate::PolicyViolation;
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let task = item_id(&fixture.apply(&mut state, 0, 2_000, create("task", "a")));
    let EventBody::ItemCreated(mut created) = create("task", "b") else {
        unreachable!()
    };
    created.priority = None;
    assert!(matches!(
        state.check_policy(&EventBody::ItemCreated(created)),
        Err(PolicyViolation::PriorityRequired { .. })
    ));
    assert!(matches!(
        state.check_policy(&edit(task, Change::Priority(None))),
        Err(PolicyViolation::PriorityRequired { .. })
    ));
    assert_eq!(
        state.check_policy(&edit(
            task,
            Change::Priority(Some(crate::item::Priority::P0))
        )),
        Ok(())
    );
    assert_eq!(
        state.check_policy(&create("decision", "no priority")),
        Ok(())
    );
}

#[test]
fn a_local_dependency_cycle_is_refused() {
    use crate::validate::PolicyViolation;
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let first = item_id(&fixture.apply(&mut state, 0, 2_000, create("task", "a")));
    let second = item_id(&fixture.apply(&mut state, 0, 2_001, create("task", "b")));
    fixture.apply(&mut state, 0, 2_002, depends_on(first, second));
    assert_eq!(
        state.check_policy(&depends_on(second, first)),
        Err(PolicyViolation::DependencyCycle {
            from: second,
            to: first
        })
    );
}

#[test]
fn concurrent_links_forming_a_cycle_resolve_the_same_way_everywhere() {
    let fixture = Fixture::new(2);
    let mut left = fixture.state();
    let first = item_id(&fixture.apply(&mut left, 0, 2_000, create("task", "a")));
    let second = item_id(&fixture.apply(&mut left, 0, 2_001, create("task", "b")));
    let mut right = left.clone();

    let a_on_b = fixture.author(&left, 0, 3_000, depends_on(first, second));
    let b_on_a = fixture.author(&right, 1, 3_001, depends_on(second, first));
    for (state, events) in [
        (&mut left, [&a_on_b, &b_on_a]),
        (&mut right, [&b_on_a, &a_on_b]),
    ] {
        for event in events {
            state
                .receive((*event).clone(), crate::state::Trust::Untrusted)
                .unwrap();
        }
    }
    assert_eq!(left, right);
    let graph = left.dependency_graph();
    assert_eq!(graph.ignored().len(), 1);
    // The edge with the greatest (hlc, id) is the one that is ignored.
    assert!(graph.is_ignored(second, first));
    assert!(!graph.is_ignored(first, second));
    assert!(!graph.reaches(second, first));
}

#[test]
fn concurrent_label_add_beats_remove() {
    let fixture = Fixture::new(2);
    let mut left = fixture.state();
    let task = item_id(&fixture.apply(&mut left, 0, 2_000, create("task", "a")));
    let label = Label::new("urgent").unwrap();
    fixture.apply(
        &mut left,
        0,
        2_001,
        edit(task, Change::Labels(SetChange::Add(label.clone()))),
    );
    let mut right = left.clone();

    let removal = {
        let ItemView::Live(item) = left.item(task).unwrap() else {
            panic!()
        };
        item.labels().removal(&label)
    };
    let remove = fixture.author(&left, 0, 3_000, edit(task, Change::Labels(removal)));
    let add_again = fixture.author(
        &right,
        1,
        3_001,
        edit(task, Change::Labels(SetChange::Add(label.clone()))),
    );
    for event in [&remove, &add_again] {
        left.receive(event.clone(), crate::state::Trust::Untrusted)
            .unwrap();
    }
    for event in [&add_again, &remove] {
        right
            .receive(event.clone(), crate::state::Trust::Untrusted)
            .unwrap();
    }
    assert_eq!(left, right);
    let ItemView::Live(item) = left.item(task).unwrap() else {
        panic!()
    };
    assert!(item.labels().contains(&label));
}

#[test]
fn concurrent_edits_resolve_by_hlc_then_event_id() {
    let fixture = Fixture::new(2);
    let mut left = fixture.state();
    let task = item_id(&fixture.apply(&mut left, 0, 2_000, create("task", "a")));
    let mut right = left.clone();
    let first = fixture.author(&left, 0, 3_000, edit(task, Change::Title("first".into())));
    let second = fixture.author(&right, 1, 3_500, edit(task, Change::Title("second".into())));
    for event in [&first, &second] {
        left.receive(event.clone(), crate::state::Trust::Untrusted)
            .unwrap();
    }
    for event in [&second, &first] {
        right
            .receive(event.clone(), crate::state::Trust::Untrusted)
            .unwrap();
    }
    assert_eq!(left, right);
    assert_eq!(left.item(task).unwrap().title(), "second");
}

#[test]
fn references_mentions_and_backlinks_resolve_against_the_state() {
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    fixture.apply(&mut state, 0, 1_500, define_agent("coder"));
    let question = item_id(&fixture.apply(&mut state, 0, 2_000, create("task", "Pick a hash")));
    let task = item_id(&fixture.apply(
        &mut state,
        0,
        2_001,
        create_with_body(
            "task",
            "Sync",
            &format!(
                "blocked on #{} and `#{}` or #zzzz",
                question.short(),
                question.short()
            ),
        ),
    ));
    let note = fixture.apply(
        &mut state,
        0,
        2_002,
        comment(
            task,
            &format!(
                "see #{}, @coder please keep it small, cc @joshua @nobody",
                question.short()
            ),
        ),
    );

    let ItemView::Live(task_item) = state.item(task).unwrap() else {
        panic!()
    };
    assert_eq!(state.references_from(task_item), [question].into());

    let backlinks = state.backlinks(question);
    assert_eq!(
        backlinks,
        vec![
            Source {
                item: task,
                origin: Origin::Body
            },
            Source {
                item: task,
                origin: Origin::Comment(note.id().into())
            },
        ]
    );

    let mentioned = state.mentions_in(task_item);
    let joshua = fixture.founder.key();
    assert!(mentioned.contains(&MentionTarget::Principal(joshua)));
    assert!(mentioned.iter().any(
        |target| matches!(target, MentionTarget::Agent(agent) if agent.name.as_str() == "coder")
    ));
    assert_eq!(mentioned.len(), 2, "@nobody does not resolve");

    let sources = state.mentions_of(&MentionTarget::Principal(joshua));
    assert_eq!(
        sources,
        vec![Source {
            item: task,
            origin: Origin::Comment(note.id().into())
        }]
    );

    let resolver = state.resolver();
    assert_eq!(
        resolver.mention(&Mention::Agent {
            principal: "joshua".into(),
            agent: "coder".into()
        }),
        resolver.mention(&Mention::Short("coder".into()))
    );
    assert_eq!(resolver.mention(&Mention::Short("ghost".into())), None);
}

#[test]
fn item_prefixes_resolve_like_git() {
    use crate::id::PrefixError;
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let task = item_id(&fixture.apply(&mut state, 0, 2_000, create("task", "a")));
    let resolver = state.resolver();
    assert_eq!(resolver.item(&task.to_base32()[..6]), Ok(task));
    assert_eq!(resolver.item(&task.to_base32()), Ok(task));
    assert!(matches!(resolver.item("ab"), Err(PrefixError::TooShort(_))));
    assert!(matches!(
        resolver.item("2222222"),
        Err(PrefixError::NotFound(_))
    ));
    let _ = StateName::new("open");
}

fn done_steps(state: &State, item: ItemId) -> Vec<bool> {
    let ItemView::Live(item) = state.item(item).unwrap() else {
        panic!("live")
    };
    item.body().steps().map(|step| step.done).collect()
}

#[test]
fn ticks_change_one_step_and_a_later_body_edit_wins_over_earlier_ticks() {
    use crate::body::StepId;
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let body = "plan #abcd\n\n- [ ] write parser\n- [ ] add tests\n";
    let task = item_id(&fixture.apply(
        &mut state,
        0,
        2_000,
        create_with_body("task", "Steps", body),
    ));
    assert_eq!(done_steps(&state, task), [false, false]);

    let tick = |done| Change::StepDone {
        step: StepId::of("add tests"),
        done,
    };
    fixture.apply(&mut state, 0, 2_001, edit(task, tick(true)));
    assert_eq!(done_steps(&state, task), [false, true]);
    fixture.apply(&mut state, 0, 2_002, edit(task, tick(false)));
    assert_eq!(done_steps(&state, task), [false, false]);

    fixture.apply(&mut state, 0, 2_003, edit(task, tick(true)));
    fixture.apply(
        &mut state,
        0,
        2_004,
        edit(
            task,
            Change::Body("- [x] write parser\n- [ ] add tests".into()),
        ),
    );
    assert_eq!(done_steps(&state, task), [true, false]);
}

#[test]
fn ticks_do_not_depend_on_arrival_order() {
    use crate::body::StepId;
    let fixture = Fixture::new(1);
    let mut author = fixture.state();
    let created = fixture.apply(
        &mut author,
        0,
        2_000,
        create_with_body("task", "Steps", "- [ ] a\n- [ ] b"),
    );
    let task = item_id(&created);
    let tick = fixture.apply(
        &mut author,
        0,
        2_001,
        edit(
            task,
            Change::StepDone {
                step: StepId::of("b"),
                done: true,
            },
        ),
    );
    let rewrite = fixture.apply(
        &mut author,
        0,
        2_002,
        edit(task, Change::Body("- [ ] a\n- [ ] b\n- [ ] c".into())),
    );
    assert_eq!(done_steps(&author, task), [false, false, false]);

    let mut peer = State::new();
    for event in [fixture.genesis.clone(), rewrite, tick, created] {
        peer.receive(event, crate::state::Trust::Stored).unwrap();
    }
    assert_eq!(done_steps(&peer, task), [false, false, false]);
}

#[test]
fn authors_cannot_write_duplicate_steps_or_tick_missing_ones() {
    use crate::body::StepId;
    use crate::validate::PolicyViolation;
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let task = item_id(&fixture.apply(
        &mut state,
        0,
        2_000,
        create_with_body("task", "Steps", "- [ ] a"),
    ));
    assert!(matches!(
        state.check_policy(&edit(task, Change::Body("- [ ] x\n- [x] x".into()))),
        Err(PolicyViolation::Body(_))
    ));
    assert!(matches!(
        state.check_policy(&create_with_body("task", "Dup", "- [ ] x\n- [ ] x")),
        Err(PolicyViolation::Body(_))
    ));
    assert!(matches!(
        state.check_policy(&create_with_body("task", "Numbered", "- [ ] 2. x")),
        Err(PolicyViolation::Body(_))
    ));
    let missing = StepId::of("not there");
    assert_eq!(
        state.check_policy(&edit(
            task,
            Change::StepDone {
                step: missing,
                done: true
            }
        )),
        Err(PolicyViolation::UnknownStep(missing))
    );
}

#[test]
fn a_peer_body_with_duplicate_steps_folds_as_plain_markdown() {
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let task = item_id(&fixture.apply(
        &mut state,
        0,
        2_000,
        create_with_body("task", "Dup", "- [ ] x\n- [ ] x"),
    ));
    assert_eq!(done_steps(&state, task), Vec::<bool>::new());
}

#[test]
fn the_review_level_of_an_item_is_a_register() {
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let task = item_id(&fixture.apply(&mut state, 0, 2_000, create("task", "work")));
    let review = |item: &crate::item::Item| item.review().map(ToString::to_string);
    let item_of = |state: &State| match state.item(task).unwrap() {
        ItemView::Live(item) => review(item),
        ItemView::Archived(_) => panic!("live"),
    };
    assert_eq!(item_of(&state), None);

    let level = |name: Option<&str>| Change::Review(name.map(|name| name.parse().unwrap()));
    fixture.apply(&mut state, 0, 2_001, edit(task, level(Some("standard"))));
    assert_eq!(item_of(&state), Some("standard".to_string()));
    fixture.apply(&mut state, 0, 2_002, edit(task, level(None)));
    assert_eq!(item_of(&state), None);
}

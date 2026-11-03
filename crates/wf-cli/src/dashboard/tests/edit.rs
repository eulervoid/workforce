//! Dashboard tests: editing the fields of an item in place, from the keys to the edit written.

use super::support::*;
use crate::dashboard::act;
use crate::dashboard::app::{App, Choice, Effect, Field, Prompt};
use crate::view::{ItemGroup, ItemSummary};
use ratatui::crossterm::event::KeyCode;
use wf_core::crdt::SetChange;
use wf_core::event::{Change, EventBody, ItemEdited};
use wf_core::id::ItemId;
use wf_core::item::{Item, ItemEntry, ItemState, Priority};
use wf_core::name::{Label, LaneName};
use wf_core::state::State;
use wf_core::testing::{Fixture, create, define_agent_with, edit};

/// A workforce with `#dashboard`, labelled `ui`, another item labelled `cli`, a coder with `rust`
/// in the default lane and an agent with `gpu` in the lane `meta`; and the dashboard on its Items
/// tab with `#dashboard` selected.
struct Workforce {
    fixture: Fixture,
    state: State,
    item: ItemId,
    app: App,
}

impl Workforce {
    fn new() -> Self {
        let fixture = Fixture::new(1);
        let mut state = fixture.state();
        let mut apply = |body| fixture.apply(&mut state, 0, 1_000, body);
        apply(agent_in("coder", "default", "rust"));
        apply(agent_in("renderer", "meta", "gpu"));
        let item: ItemId = apply(create("task", "Dashboard")).id().into();
        apply(edit(item, Change::Labels(SetChange::Add(label("ui")))));
        let other: ItemId = apply(create("task", "Other")).id().into();
        apply(edit(other, Change::Labels(SetChange::Add(label("cli")))));
        let mut app = app();
        press(&mut app, KeyCode::Char('2'));
        let mut snapshot = snapshot();
        snapshot.items = vec![ItemGroup {
            group: "open".to_owned(),
            items: vec![ItemSummary {
                id: item,
                ..item_row(4, "Dashboard", ItemState::Open)
            }],
        }];
        app.refresh(snapshot);
        Self {
            fixture,
            state,
            item,
            app,
        }
    }

    /// Presses `code`, and carries out what it asks as the loop does: opens what it asks for, or
    /// writes the edit, which it returns.
    fn press(&mut self, code: KeyCode) -> Option<EventBody> {
        let effect = press(&mut self.app, code)?;
        self.carry_out(effect)
    }

    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "the keys of these tests ask only for edits"
    )]
    fn carry_out(&mut self, effect: Effect) -> Option<EventBody> {
        match effect {
            Effect::ChooseField { item } => {
                let menu = act::fields(&self.state, item).unwrap();
                self.app.ask(Prompt::Choose(menu));
                None
            }
            Effect::Edit { item, field } => {
                let prompt = act::asked(&self.state, item, field).unwrap();
                self.app.ask(prompt.expect("a field asked for in place"));
                None
            }
            Effect::Change { item, changes } => {
                let body = EventBody::ItemEdited(ItemEdited { item, changes });
                self.fixture.apply(&mut self.state, 0, 2_000, body.clone());
                Some(body)
            }
            other => panic!("not an edit: {other:?}"),
        }
    }

    fn live(&self) -> &Item {
        match self.state.item(self.item) {
            Some(ItemEntry::Live(item)) => item,
            other => panic!("not live: {other:?}"),
        }
    }

    fn labels(&self) -> Vec<String> {
        self.live()
            .labels()
            .values()
            .map(ToString::to_string)
            .collect()
    }
}

fn agent_in(name: &str, lane: &str, capability: &str) -> EventBody {
    let EventBody::AgentDefined(mut defined) = define_agent_with(name, &[capability]) else {
        unreachable!("`define_agent_with` makes an `AgentDefined`")
    };
    defined.lanes = vec![LaneName::new(lane).unwrap()];
    EventBody::AgentDefined(defined)
}

fn label(name: &str) -> Label {
    Label::new(name).unwrap()
}

#[test]
fn e_4_and_a_pick_write_the_lane_change() {
    let mut workforce = Workforce::new();
    workforce.press(KeyCode::Char('E'));
    insta::assert_snapshot!("edit_menu", screen(&mut workforce.app, 60, 16));
    assert_eq!(workforce.press(KeyCode::Char('4')), None);
    let screen = screen(&mut workforce.app, 60, 16);
    assert!(screen.contains("lane of #"), "{screen}");
    // It starts on the lane the item is in, of its type.
    assert!(screen.contains("▐default▌"), "{screen}");
    workforce.press(KeyCode::Char('j'));
    let written = workforce.press(KeyCode::Enter);
    let meta = LaneName::new("meta").unwrap();
    assert_eq!(
        written,
        Some(EventBody::ItemEdited(ItemEdited {
            item: workforce.item,
            changes: vec![Change::Lane(Some(meta.clone()))],
        }))
    );
    assert_eq!(workforce.live().lane(), Some(&meta));
    assert!(workforce.app.prompt.is_none());
}

#[test]
fn the_edit_menu_picks_with_j_k_and_enter_and_with_a_click_and_esc_closes_it() {
    let mut workforce = Workforce::new();
    let item = workforce.item;
    workforce.press(KeyCode::Char('E'));
    press(&mut workforce.app, KeyCode::Char('j'));
    press(&mut workforce.app, KeyCode::Char('j'));
    assert_eq!(
        press(&mut workforce.app, KeyCode::Enter),
        Some(Effect::Edit {
            item,
            field: Field::Priority
        })
    );
    workforce.press(KeyCode::Char('E'));
    let (column, row) = find(&mut workforce.app, 60, 16, "review");
    assert_eq!(
        workforce.app.click(column, row),
        Some(Effect::Edit {
            item,
            field: Field::Review
        })
    );
    assert!(workforce.app.prompt.is_none());
    workforce.press(KeyCode::Char('E'));
    assert_eq!(press(&mut workforce.app, KeyCode::Esc), None);
    assert!(workforce.app.prompt.is_none());
    // The first line is the editor, as `e` is.
    workforce.press(KeyCode::Char('E'));
    assert_eq!(
        press(&mut workforce.app, KeyCode::Char('1')),
        Some(Effect::Edit {
            item,
            field: Field::Text
        })
    );
}

#[test]
fn e_m_and_p_act_on_the_item_at_once_and_p_pages_a_diff() {
    let mut workforce = Workforce::new();
    let item = workforce.item;
    for (key, field) in [
        ('e', Field::Text),
        ('m', Field::State),
        ('p', Field::Priority),
    ] {
        assert_eq!(
            press(&mut workforce.app, KeyCode::Char(key)),
            Some(Effect::Edit { item, field })
        );
    }
    let mut app = diff_app();
    assert_eq!(
        press(&mut app, KeyCode::Char('p')),
        Some(Effect::PageDiff {
            item: ItemId::from_bytes([3; 32])
        })
    );
}

#[test]
fn p_writes_the_priority_starting_on_the_current_one() {
    let mut workforce = Workforce::new();
    workforce.press(KeyCode::Char('p'));
    let Some(Prompt::Choose(choice @ Choice::Priority { .. })) = &workforce.app.prompt else {
        panic!("no choice of priorities: {:?}", workforce.app.prompt);
    };
    assert_eq!(choice.index(), 2, "a task starts on P2");
    insta::assert_snapshot!("choose_priority", screen(&mut workforce.app, 60, 14));
    workforce.press(KeyCode::Char('k'));
    let written = workforce.press(KeyCode::Enter);
    assert_eq!(
        written,
        Some(EventBody::ItemEdited(ItemEdited {
            item: workforce.item,
            changes: vec![Change::Priority(Some(Priority::P1))],
        }))
    );
    assert_eq!(workforce.live().priority(), Some(Priority::P1));
}

#[test]
fn review_and_recovery_offer_the_types_level() {
    let mut workforce = Workforce::new();
    workforce.press(KeyCode::Char('E'));
    workforce.press(KeyCode::Char('5'));
    let screen = screen(&mut workforce.app, 60, 16);
    assert!(screen.contains("▐the type's: "), "{screen}");
    workforce.press(KeyCode::Char('j'));
    let Some(EventBody::ItemEdited(edited)) = workforce.press(KeyCode::Enter) else {
        panic!("nothing written");
    };
    assert!(matches!(edited.changes[..], [Change::ReviewLevel(Some(_))]));
    workforce.press(KeyCode::Char('E'));
    workforce.press(KeyCode::Char('5'));
    workforce.press(KeyCode::Char('k'));
    let written = workforce.press(KeyCode::Enter);
    assert_eq!(
        written,
        Some(EventBody::ItemEdited(ItemEdited {
            item: workforce.item,
            changes: vec![Change::ReviewLevel(None)],
        })),
        "the first is the type's"
    );
    assert_eq!(workforce.live().review_level(), None);
}

#[test]
fn the_set_input_adds_removes_and_creates_labels_as_one_edit() {
    let mut workforce = Workforce::new();
    workforce.press(KeyCode::Char('E'));
    workforce.press(KeyCode::Char('7'));
    insta::assert_snapshot!("set_labels", screen(&mut workforce.app, 60, 12));
    // `cli`, then `ui`, which the item has.
    workforce.press(KeyCode::Char(' '));
    workforce.press(KeyCode::Down);
    workforce.press(KeyCode::Char(' '));
    type_text(&mut workforce.app, "jobs");
    let screen = screen(&mut workforce.app, 60, 12);
    assert!(screen.contains("enter adds jobs"), "{screen}");
    assert!(!screen.contains("[x] cli"), "typing filters: {screen}");
    assert_eq!(workforce.press(KeyCode::Enter), None, "enter adds it");
    assert!(super::support::screen(&mut workforce.app, 60, 12).contains("[x] jobs"));
    let Some(EventBody::ItemEdited(edited)) = workforce.press(KeyCode::Enter) else {
        panic!("nothing written");
    };
    assert_eq!(edited.item, workforce.item);
    assert!(matches!(
        &edited.changes[..],
        [
            Change::Labels(SetChange::Add(cli)),
            Change::Labels(SetChange::Remove { value: ui, .. }),
            Change::Labels(SetChange::Add(jobs)),
        ] if *cli == label("cli") && *ui == label("ui") && *jobs == label("jobs")
    ));
    assert_eq!(workforce.labels(), ["cli", "jobs"]);
}

#[test]
fn the_set_input_changes_nothing_on_esc() {
    let mut workforce = Workforce::new();
    workforce.press(KeyCode::Char('E'));
    workforce.press(KeyCode::Char('7'));
    workforce.press(KeyCode::Char(' '));
    type_text(&mut workforce.app, "new");
    workforce.press(KeyCode::Enter);
    assert_eq!(workforce.press(KeyCode::Esc), None);
    assert!(workforce.app.prompt.is_none());
    assert_eq!(workforce.labels(), ["ui"]);
}

#[test]
fn a_requirement_no_agent_of_the_lane_has_is_marked() {
    let mut workforce = Workforce::new();
    workforce.press(KeyCode::Char('E'));
    workforce.press(KeyCode::Char('8'));
    let drawn = Drawn::of(&mut workforce.app, 70, 12);
    let line = |value: &str| {
        drawn
            .lines()
            .into_iter()
            .find(|line| line.contains(value))
            .unwrap_or_else(|| panic!("{value} is not listed"))
    };
    assert!(line("[ ] gpu").contains("no agent of its lane has it"));
    assert!(!line("[ ] rust").contains("no agent"));
    // A requirement typed in is marked as it is typed.
    type_text(&mut workforce.app, "linux");
    let screen = screen(&mut workforce.app, 70, 12);
    assert!(
        screen.contains("enter adds linux  no agent of its lane has it"),
        "{screen}"
    );
    workforce.press(KeyCode::Enter);
    let written = workforce.press(KeyCode::Enter);
    assert!(matches!(
        written,
        Some(EventBody::ItemEdited(ItemEdited { changes, .. }))
            if matches!(&changes[..], [Change::Requires(SetChange::Add(_))])
    ));
}

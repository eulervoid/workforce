//! Dashboard tests: items.

use super::support::*;
use crate::dashboard::app::{App, Choice, Effect, NowRow, Opened, Prompt, Scroll, Split, Starter};
use crate::dashboard::snapshot::Detail;
use crate::theme;
use ratatui::crossterm::event::KeyCode;
use ratatui::style::{Color, Modifier, Style};
use wf_core::id::{ItemId, SessionId};
use wf_core::item::{ItemState, Priority};
use wf_core::name::AgentName;

#[test]
fn the_items_tab_lists_items_by_state_and_filters_them() {
    let mut app = app();
    press(&mut app, KeyCode::Char('2'));
    assert_eq!(
        item_listing(&app).map(|listing| listing.grouping),
        Some(crate::cli::ItemGrouping::State)
    );
    app.refresh(with_items());
    insta::assert_snapshot!("items", screen(&mut app, 60, 14));
    press(&mut app, KeyCode::Char('F'));
    assert_eq!(app.item_groups().len(), 1, "only the ready item");
    press(&mut app, KeyCode::Char('F'));
    press(&mut app, KeyCode::Char('F'));
    press(&mut app, KeyCode::Char('F'));
    press(&mut app, KeyCode::Char('/'));
    for character in "parse".chars() {
        press(&mut app, KeyCode::Char(character));
    }
    press(&mut app, KeyCode::Enter);
    let titles: Vec<&str> = app
        .item_groups()
        .into_iter()
        .flat_map(|(_, items)| items.into_iter().map(|item| item.title.as_str()))
        .collect();
    assert_eq!(titles, ["Parse config lazily"]);
    press(&mut app, KeyCode::Char('o'));
    assert_eq!(
        item_listing(&app).map(|listing| listing.grouping),
        Some(crate::cli::ItemGrouping::Lane)
    );
}

#[test]
fn the_items_tab_cycles_its_order_and_shows_it_in_its_title() {
    use crate::cli::ItemOrder;
    let mut app = app();
    press(&mut app, KeyCode::Char('2'));
    app.refresh(with_items());
    let order = |app: &App| item_listing(app).map(|listing| listing.order);
    assert_eq!(order(&app), Some(ItemOrder::Priority));
    assert!(screen(&mut app, 60, 14).contains("by state · sorted by priority"));
    press(&mut app, KeyCode::Char('O'));
    assert_eq!(order(&app), Some(ItemOrder::Created));
    assert!(screen(&mut app, 60, 14).contains("sorted by creation"));
    press(&mut app, KeyCode::Char('O'));
    assert_eq!(order(&app), Some(ItemOrder::Merged));
    assert!(screen(&mut app, 60, 14).contains("sorted by merge"));
    press(&mut app, KeyCode::Char('O'));
    assert_eq!(order(&app), Some(ItemOrder::Priority));
}

#[test]
fn the_items_filter_matches_the_start_of_an_id_with_or_without_a_hash() {
    let mut app = app();
    press(&mut app, KeyCode::Char('2'));
    app.refresh(with_items());
    for filter in ["aeaqca", "#AEAQ"] {
        press(&mut app, KeyCode::Char('/'));
        for character in filter.chars() {
            press(&mut app, KeyCode::Char(character));
        }
        press(&mut app, KeyCode::Enter);
        let titles: Vec<&str> = app
            .item_groups()
            .into_iter()
            .flat_map(|(_, items)| items.into_iter().map(|item| item.title.as_str()))
            .collect();
        assert_eq!(titles, ["Parse config lazily"], "filtered by {filter}");
        press(&mut app, KeyCode::Esc);
    }
}

#[test]
fn an_item_row_shows_its_priority_in_the_style_of_wf_item_list() {
    let mut app = app();
    press(&mut app, KeyCode::Char('2'));
    app.refresh(with_items());
    let text = screen(&mut app, 60, 14);
    let (y, line) = text
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains("Parse config lazily"))
        .unwrap();
    let column = |text: &str| u16::try_from(line[1..line.find(text).unwrap()].chars().count());
    let y = u16::try_from(y).unwrap();
    let look = |style: Style| (style.fg, style.add_modifier);
    let priority = Drawn::of(&mut app, 60, 14).style_at(column("P0").unwrap(), y);
    let p0 = Style::from(theme::priority(Priority::P0));
    assert_eq!(look(priority), look(p0));
    let steps = Drawn::of(&mut app, 60, 14).style_at(column("2/5").unwrap(), y);
    assert_eq!(look(steps), (Some(Color::Reset), Modifier::DIM));
}

#[test]
fn an_item_opens_on_its_detail_and_enter_goes_on_to_its_session() {
    let mut app = app();
    press(&mut app, KeyCode::Char('2'));
    app.refresh(with_items());
    press(&mut app, KeyCode::Char('G'));
    press(&mut app, KeyCode::Enter);
    let parse = ItemId::from_bytes([1; 32]);
    assert_eq!(app.wanted().detail, Some(Detail::Item(parse)));
    let coder = SessionId::from_bytes([11; 32]);
    let mut snapshot = with_items();
    snapshot.in_view = Some(item_in_view(
        parse,
        "◐ #aeaqca (task:in_progress) Parse config lazily\n".to_owned(),
        Some(coder),
    ));
    app.refresh(snapshot);
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.wanted().detail, Some(Detail::Session(coder)));
    // Its tab keeps its own details: the Now tab is as it was.
    press(&mut app, KeyCode::Char('1'));
    assert_eq!(app.wanted().detail, None);
    press(&mut app, KeyCode::Char('2'));
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.wanted().detail, None);
}

#[test]
fn a_move_of_the_selection_takes_in_only_the_new_detail() {
    let mut app = app();
    app.resize(130, 24);
    press(&mut app, KeyCode::Char('2'));
    app.refresh(with_items());
    press(&mut app, KeyCode::Char('j'));
    let subscription = ItemId::from_bytes([5; 32]);
    assert_eq!(app.wanted().detail, Some(Detail::Item(subscription)));
    let in_view = item_in_view(
        subscription,
        "#aukcuq Session subscription\n".to_owned(),
        None,
    );
    app.refresh_detail(Some(in_view.clone()));
    assert_eq!(app.snapshot.in_view, Some(in_view));
    let titles: Vec<&str> = app
        .item_groups()
        .into_iter()
        .flat_map(|(_, items)| items.into_iter().map(|item| item.title.as_str()))
        .collect();
    assert_eq!(
        titles,
        ["Dashboard", "Session subscription", "Parse config lazily"]
    );
    assert_eq!(app.snapshot.flagged, snapshot().flagged);
    assert_eq!(app.snapshot.waiting, snapshot().waiting);
    assert_eq!(app.snapshot.working, snapshot().working);
    assert_eq!(app.wanted().detail, Some(Detail::Item(subscription)));
}

#[test]
fn going_back_to_the_list_keeps_the_item_where_it_was_scrolled_to() {
    let (mut app, scrolled) = scrolled_item(130);
    press(&mut app, KeyCode::Char('h'));
    assert!(app.list_has_keys());
    assert_eq!(app.on_screen(), Some(scrolled.clone()));
    press(&mut app, KeyCode::Char('l'));
    assert!(!app.list_has_keys());
    assert_eq!(app.on_screen(), Some(scrolled.clone()));
    // Away from the item and back, it starts at its top.
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('k'));
    press(&mut app, KeyCode::Char('j'));
    let at_top = Opened {
        scroll: Scroll::At(0),
        ..scrolled
    };
    assert_eq!(app.on_screen(), Some(at_top.clone()));
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.on_screen(), Some(at_top));
}

#[test]
fn in_a_narrow_window_the_list_comes_back_to_the_item_where_it_was() {
    let (mut app, scrolled) = scrolled_item(60);
    press(&mut app, KeyCode::Left);
    assert!(app.list_has_keys());
    assert_eq!(app.on_screen(), None);
    assert_eq!(app.path(), Vec::new());
    press(&mut app, KeyCode::Right);
    assert_eq!(app.on_screen(), Some(scrolled.clone()));
    // Away from the item and back, it starts at its top.
    press(&mut app, KeyCode::Char('h'));
    press(&mut app, KeyCode::Char('k'));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('l'));
    assert_eq!(
        app.on_screen(),
        Some(Opened {
            scroll: Scroll::At(0),
            ..scrolled
        })
    );
}

#[test]
fn with_one_pane_the_list_comes_back_to_the_item_where_it_was() {
    let (mut app, scrolled) = scrolled_item(130);
    press(&mut app, KeyCode::Char('L'));
    assert_eq!(app.split, Split::One);
    press(&mut app, KeyCode::Char('h'));
    assert!(app.list_has_keys());
    assert_eq!(app.on_screen(), None);
    press(&mut app, KeyCode::Char('l'));
    assert_eq!(app.on_screen(), Some(scrolled));
}

#[test]
fn going_back_from_a_session_opened_on_an_item_leaves_the_item_as_it_was() {
    let mut app = app();
    press(&mut app, KeyCode::Char('2'));
    let parse = ItemId::from_bytes([1; 32]);
    let coder = SessionId::from_bytes([11; 32]);
    let mut snapshot = with_items();
    snapshot.in_view = Some(item_in_view(
        parse,
        (1..=40).map(|line| format!("line {line}\n")).collect(),
        Some(coder),
    ));
    app.refresh(snapshot);
    press(&mut app, KeyCode::Char('G'));
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.path(), [Detail::Item(parse), Detail::Session(coder)]);
    press(&mut app, KeyCode::Esc);
    assert!(!app.list_has_keys());
    assert_eq!(
        app.on_screen(),
        Some(Opened {
            detail: Detail::Item(parse),
            scroll: Scroll::At(1),
        })
    );
}

#[test]
fn item_keys_act_on_the_selected_item() {
    let mut app = app();
    press(&mut app, KeyCode::Char('2'));
    app.refresh(with_items());
    let dashboard = ItemId::from_bytes([4; 32]);
    assert_eq!(
        press(&mut app, KeyCode::Char('s')),
        Some(Effect::ChooseStart { item: dashboard })
    );
    app.choose(Choice::Start {
        item: dashboard,
        starters: vec![
            Starter::Agent(AgentName::new("coder").unwrap()),
            Starter::Agent(AgentName::new("reviewer").unwrap()),
        ],
        index: 0,
    });
    insta::assert_snapshot!("choose_agent", screen(&mut app, 60, 12));
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(
        press(&mut app, KeyCode::Enter),
        Some(Effect::StartSession {
            item: dashboard,
            agent: AgentName::new("reviewer").unwrap()
        })
    );
    assert_eq!(
        press(&mut app, KeyCode::Char('m')),
        Some(Effect::ChooseState { item: dashboard })
    );
    app.choose(Choice::State {
        item: dashboard,
        states: vec![ItemState::Draft, ItemState::Done, ItemState::Cancelled],
        index: 0,
    });
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(
        press(&mut app, KeyCode::Enter),
        Some(Effect::Move {
            item: dashboard,
            state: ItemState::Done
        })
    );
    app.choose(Choice::Start {
        item: dashboard,
        starters: vec![
            Starter::Dispatcher,
            Starter::Agent(AgentName::new("coder").unwrap()),
        ],
        index: 0,
    });
    insta::assert_snapshot!("choose_release", screen(&mut app, 60, 12));
    assert_eq!(
        press(&mut app, KeyCode::Enter),
        Some(Effect::Move {
            item: dashboard,
            state: ItemState::Open
        }),
        "s enter releases a draft"
    );
    assert_eq!(press(&mut app, KeyCode::Char('c')), None);
    for character in "Start with the Now tab".chars() {
        press(&mut app, KeyCode::Char(character));
    }
    assert_eq!(
        press(&mut app, KeyCode::Enter),
        Some(Effect::Comment {
            item: dashboard,
            text: "Start with the Now tab".to_owned()
        })
    );
    assert_eq!(
        press(&mut app, KeyCode::Char('e')),
        Some(Effect::Edit { item: dashboard })
    );
    // An item has no session to stop.
    assert_eq!(press(&mut app, KeyCode::Char('x')), None);
    assert!(app.prompt.is_none());
}

#[test]
fn a_session_is_stopped_only_when_confirmed() {
    let mut app = app();
    let coder = SessionId::from_bytes([11; 32]);
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.now.selection.get(), Some(&NowRow::Session(coder)));
    assert_eq!(press(&mut app, KeyCode::Char('x')), None);
    insta::assert_snapshot!("stop", screen(&mut app, 60, 6));
    assert_eq!(press(&mut app, KeyCode::Char('n')), None);
    assert!(app.prompt.is_none());
    press(&mut app, KeyCode::Char('x'));
    assert_eq!(
        press(&mut app, KeyCode::Char('y')),
        Some(Effect::StopSession { session: coder })
    );
}

#[test]
fn the_keys_show_on_a_question_mark() {
    let mut app = app();
    press(&mut app, KeyCode::Char('?'));
    assert!(matches!(app.prompt, Some(Prompt::Keys)));
    insta::assert_snapshot!("keys", screen(&mut app, 60, 18));
    // Any key closes it, and does nothing else.
    assert_eq!(press(&mut app, KeyCode::Char('q')), None);
    assert!(app.prompt.is_none());
}

//! Dashboard tests: agents.

use super::support::*;
use crate::dashboard::app::{AgentRow, Effect, NowRow, Opened, Scroll, Wheel};
use crate::dashboard::snapshot::{Detail, Listing};
use ratatui::crossterm::event::KeyCode;
use wf_core::id::{ItemId, SessionId};
use wf_harness::Permission;

#[test]
fn the_agents_tab_lists_each_agent_with_its_sessions() {
    let mut app = app();
    press(&mut app, KeyCode::Char('3'));
    assert_eq!(app.wanted().listing, Some(Listing::Agents));
    app.refresh(with_agents());
    insta::assert_snapshot!("agents", screen(&mut app, 60, 16));
    // An agent opens on its definition, a session on its transcript.
    press(&mut app, KeyCode::Enter);
    assert!(
        matches!(app.wanted().detail, Some(Detail::Agent(agent)) if agent.name.as_str() == "coder")
    );
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(
        app.agents.selection.get(),
        Some(&AgentRow::Session(SessionId::from_bytes([11; 32])))
    );
    // The keys of a session work here too.
    assert_eq!(
        press(&mut app, KeyCode::Char('a')),
        Some(Effect::Permit {
            request: coders_request(),
            answer: Permission::Allow
        })
    );
    press(&mut app, KeyCode::Char('G'));
    press(&mut app, KeyCode::Char('k'));
    // A session that ended cannot be stopped.
    assert_eq!(press(&mut app, KeyCode::Char('x')), None);
    assert!(app.prompt.is_none());
}

#[test]
fn the_wheel_moves_what_is_under_the_pointer() {
    let mut app = app();
    app.resize(130, 24);
    let item = Detail::Item(ItemId::from_bytes([4; 32]));
    let blocked = Detail::Item(ItemId::from_bytes([5; 32]));
    // Over the detail beside the list, the wheel scrolls it and leaves the keys with the list.
    app.wheel(100, Wheel::Down);
    assert_eq!(
        app.stack(),
        &vec![Opened {
            detail: Detail::Session(SessionId::from_bytes([11; 32])),
            scroll: Scroll::END,
        }]
    );
    assert!(app.list_has_keys());
    press(&mut app, KeyCode::Char('2'));
    app.refresh(with_items());
    app.wheel(100, Wheel::Down);
    app.wheel(100, Wheel::Down);
    assert_eq!(
        app.stack(),
        &vec![Opened {
            detail: item.clone(),
            scroll: Scroll::At(6),
        }]
    );
    assert!(app.list_has_keys());
    // Over the list, the wheel scrolls it; the keys, the selection and the detail stay.
    app.wheel(10, Wheel::Down);
    assert!(app.list_has_keys());
    assert_eq!(app.stack().len(), 1);
    assert_eq!(app.wanted().detail, Some(item.clone()));
    // The keys move the selection, whose detail takes the place of the one opened.
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.stack().len(), 0);
    assert_eq!(app.wanted().detail, Some(blocked));
    press(&mut app, KeyCode::Char('k'));
    assert_eq!(app.wanted().detail, Some(item.clone()));
    // Over the detail of a flag, the wheel scrolls the detail it opens.
    press(&mut app, KeyCode::Char('1'));
    app.refresh(snapshot());
    press(&mut app, KeyCode::Char('G'));
    assert!(matches!(app.now.selection.get(), Some(NowRow::Flag(_))));
    app.wheel(100, Wheel::Down);
    assert_eq!(app.stack().len(), 1);
    assert_eq!(app.wanted().detail, Some(item.clone()));
    press(&mut app, KeyCode::Esc);
    assert!(app.list_has_keys());
    press(&mut app, KeyCode::Char('2'));
    app.refresh(with_items());
    // In a narrow window the wheel moves what is on screen, wherever the pointer is.
    app.resize(60, 24);
    app.wheel(50, Wheel::Down);
    assert!(app.list_has_keys());
    press(&mut app, KeyCode::Char('k'));
    press(&mut app, KeyCode::Enter);
    app.wheel(10, Wheel::Down);
    assert_eq!(
        app.stack().last().map(|opened| opened.scroll),
        Some(Scroll::At(3))
    );
}

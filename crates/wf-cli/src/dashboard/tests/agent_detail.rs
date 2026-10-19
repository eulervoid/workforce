//! Dashboard tests: agent_detail.

use super::support::*;
use crate::dashboard::snapshot::Detail;
use wf_core::id::{ItemId, SessionId};

#[test]
fn an_agent_detail_shows_its_state_config_hosts_sessions_and_instructions() {
    let mut app = app_with_agent(80, &coder_detail(true));
    insta::assert_snapshot!("agent_detail", screen(&mut app, 80, 30));
}

#[test]
fn an_agent_not_hosted_here_says_so() {
    let mut app = app_with_agent(80, &coder_detail(false));
    let lines = Drawn::of(&mut app, 80, 30).lines();
    assert!(lines[4].contains("1 live · not hosted here"), "{lines:#?}");
    assert!(lines[7].contains("hosted by laptop up to 1"), "{lines:#?}");
}

#[test]
fn in_a_narrow_window_the_agent_detail_wraps_off_the_right_edge() {
    let mut app = app_with_agent(40, &coder_detail(true));
    insta::assert_snapshot!("agent_detail_narrow", screen(&mut app, 40, 40));
    for line in &Drawn::of(&mut app, 40, 40).lines()[3..37] {
        assert!(line.ends_with(' '), "{line:?}");
    }
}

#[test]
fn a_click_on_an_item_or_a_session_of_an_agent_opens_it_in_place_and_ctrl_o_comes_back() {
    let mut app = app_with_agent(80, &coder_detail(true));
    let agent = Detail::Agent(coder_agent());
    let opens = [
        ("#aeaqca", Detail::Item(ItemId::from_bytes([1; 32]))),
        (
            "@ada/coder/",
            Detail::Session(SessionId::from_bytes([11; 32])),
        ),
    ];
    for (text, detail) in opens {
        let (column, row) = find(&mut app, 80, 30, text);
        assert_eq!(app.click(column + 1, row), None);
        assert_eq!(app.path(), vec![detail], "{text}");
        jump_back(&mut app);
        assert_eq!(app.path(), vec![agent.clone()]);
    }
}

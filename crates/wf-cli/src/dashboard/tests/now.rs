//! Dashboard tests: now.

use super::support::*;
use crate::dashboard::app::{App, Effect, NowRow, STATUS_LASTS_MS, Scroll, Split, Tab};
use crate::dashboard::snapshot::{Detail, InView, InboxEntry, Snapshot};
use crate::render;
use crate::view::Flag;
use jiff::tz::TimeZone;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use wf_core::id::{CommentId, ItemId, SessionId};
use wf_harness::Permission;

#[test]
fn the_now_tab_shows_what_waits_what_works_and_what_is_flagged() {
    let mut app = app();
    let coder = app.snapshot.working[0].id;
    app.transcribed(coder, &coder_transcript());
    insta::assert_snapshot!(screen(&mut app, 60, 22));
}

#[test]
fn a_flag_about_an_item_opens_the_item() {
    let mut app = app();
    press(&mut app, KeyCode::Char('G'));
    assert!(matches!(app.now.selection.get(), Some(NowRow::Flag(_))));
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.wanted().detail, Some(Detail::Item(item(4, "Watch").id)));
}

#[test]
fn a_flag_about_no_item_opens_in_full_with_its_command_on_its_own_line() {
    let flag = Flag::UnreadablePolicy {
        error: "unknown field `review_levels` in the policy of 2026-10-14, expected one of `types`, `lanes`, `recovery`".to_owned(),
    };
    let mut flagged = snapshot();
    flagged.flagged = vec![flag.clone()];
    let mut app = App::new(flagged.clone(), TimeZone::UTC);
    app.resize(44, 24);
    press(&mut app, KeyCode::Char('G'));
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.wanted().detail, Some(Detail::Flag(flag.clone())));
    flagged.in_view = Some(InView::Flag {
        text: render::flag_detail_for(&flag, Some(42)),
        flag,
    });
    app.refresh(flagged);
    let shown = screen(&mut app, 44, 24);
    assert!(shown.contains("┃ wf policy edit"), "{shown}");
    insta::assert_snapshot!(shown);
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.wanted().detail, None);
}

#[test]
fn keys_switch_tabs_and_quit() {
    let mut app = app();
    assert_eq!(press(&mut app, KeyCode::Char('3')), None);
    assert_eq!(app.tab, Tab::Agents);
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.tab, Tab::Log);
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.tab, Tab::Now);
    press(&mut app, KeyCode::BackTab);
    assert_eq!(app.tab, Tab::Log);
    assert_eq!(press(&mut app, KeyCode::Char('q')), Some(Effect::Quit));
}

#[test]
fn the_selection_stays_on_its_row_when_rows_come_and_go() {
    let mut app = app();
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    let review = app.now.selection.get().cloned();
    assert!(matches!(review, Some(NowRow::Waiting(_))));
    let mut answered = snapshot();
    answered.waiting.remove(0);
    app.refresh(answered);
    assert_eq!(app.now.selection.get().cloned(), review);
    let mut reviewed = snapshot();
    reviewed.waiting.clear();
    app.refresh(reviewed);
    // The row is gone: the selection stays where it was.
    assert_eq!(
        app.now.selection.get(),
        Some(&NowRow::Session(SessionId::from_bytes([12; 32])))
    );
}

#[test]
fn a_session_opens_on_its_transcript_and_esc_goes_back() {
    let mut app = app();
    let coder = app.snapshot.working[0].clone();
    app.transcribed(coder.id, &coder_transcript());
    // The permission request of the coder is selected first; it opens the coder's session.
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.wanted().detail, Some(Detail::Session(coder.id)));
    let mut snapshot = snapshot();
    snapshot.in_view = Some(session_in_view(coder));
    app.refresh(snapshot);
    insta::assert_snapshot!(screen(&mut app, 60, 16));
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.wanted().detail, None);
}

#[test]
fn a_wide_window_shows_the_selection_beside_the_list() {
    let mut app = app();
    app.resize(130, 24);
    let blocked = SessionId::from_bytes([12; 32]);
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.wanted().detail, Some(Detail::Session(blocked)));
    let mut snapshot = snapshot();
    snapshot.in_view = Some(session_in_view(snapshot.working[1].clone()));
    app.refresh(snapshot);
    insta::assert_snapshot!(screen(&mut app, 130, 14));
}

#[test]
fn h_and_l_fold_the_list_or_the_detail_to_a_strip() {
    use ratatui::crossterm::event::KeyModifiers;
    let mut app = app();
    app.resize(130, 24);
    for _ in 0..4 {
        press(&mut app, KeyCode::Char('j'));
    }
    let mut snapshot = snapshot();
    snapshot.in_view = Some(session_in_view(snapshot.working[1].clone()));
    app.refresh(snapshot);
    // The list takes the window; the detail is a strip at the right.
    press(&mut app, KeyCode::Char('H'));
    assert!(app.list_has_keys());
    assert_eq!(app.wanted().detail, None);
    insta::assert_snapshot!("list_only", screen(&mut app, 130, 14));
    // H again goes no further; L brings both back, the list keeping the keys.
    press(&mut app, KeyCode::Char('H'));
    insta::assert_snapshot!("list_only", screen(&mut app, 130, 14));
    press(&mut app, KeyCode::Char('L'));
    assert!(app.list_has_keys());
    insta::assert_snapshot!(
        "a_wide_window_shows_the_selection_beside_the_list",
        screen(&mut app, 130, 14)
    );
    // Shift → gives the detail the window, with the list a strip at the left.
    app.key(KeyEvent::new(KeyCode::Right, KeyModifiers::SHIFT));
    assert!(!app.list_has_keys());
    assert_eq!(app.detail_width(), 127);
    insta::assert_snapshot!("detail_only", screen(&mut app, 130, 14));
    // Esc goes back to the list in the whole window, as in a narrow one.
    press(&mut app, KeyCode::Esc);
    assert!(app.list_has_keys());
    insta::assert_snapshot!("list_only", screen(&mut app, 130, 14));
    // Enter opens the detail in the whole window again; shift ← brings both back.
    press(&mut app, KeyCode::Enter);
    app.key(KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT));
    assert!(!app.list_has_keys());
    insta::assert_snapshot!("both_with_the_detail_keys", screen(&mut app, 130, 14));
}

#[test]
fn a_click_on_the_strip_brings_both_panes_back() {
    let mut app = app();
    app.resize(130, 24);
    press(&mut app, KeyCode::Char('L'));
    screen(&mut app, 130, 14);
    // The list folded away to the left: a click in the detail does nothing to the panes.
    app.click(60, 8);
    assert_eq!(app.split, Split::One);
    app.click(0, 8);
    assert_eq!(app.split, Split::Both);
    assert!(!app.list_has_keys());
    // Neither H nor L does anything on the Log tab, or in a narrow window.
    press(&mut app, KeyCode::Char('4'));
    press(&mut app, KeyCode::Char('H'));
    assert_eq!(app.split, Split::Both);
    press(&mut app, KeyCode::Char('1'));
    app.resize(60, 24);
    press(&mut app, KeyCode::Char('H'));
    assert_eq!(app.split, Split::Both);
}

#[test]
fn an_item_detail_shows_the_item_as_wf_item_show_prints_it() {
    let mut app = app();
    let spans = ItemId::from_bytes([2; 32]);
    app.open_item(spans);
    let mut snapshot = snapshot();
    snapshot.in_view = Some(item_in_view(
        spans,
        "◐ #cicicicicici (task:in_progress) Fix error spans\n\nThe span is lost.\n".to_owned(),
        Some(SessionId::from_bytes([12; 32])),
    ));
    app.refresh(snapshot);
    insta::assert_snapshot!(screen(&mut app, 60, 10));
}

#[test]
fn enter_on_what_waits_opens_the_item_with_its_notice_in_view() {
    let held = item(4, "Held proposal");
    let what = "proposed: accept it with `wf item done 4`, reject it with `wf item cancel 4`";
    let mut snapshot = snapshot();
    snapshot.waiting = vec![InboxEntry::Notice {
        item: held.clone(),
        kind: wf_core::inbox::InboxKind::Proposal,
        what: what.to_owned(),
    }];
    let mut app = App::new(snapshot.clone(), TimeZone::UTC);
    app.resize(60, 24);
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.wanted().detail, Some(Detail::Item(held.id)));
    assert_eq!(
        app.on_screen().map(|opened| opened.scroll),
        Some(Scroll::At(0))
    );
    // As `wf item show` renders it, with a long body under the notice.
    let bar = "\x1b[33m┃\x1b[0m";
    let mut text = format!(
        "◎ #aqcaqcaqcaqc (task:review) Held proposal\n\n  {bar} \x1b[1;33mproposal\x1b[0m\n"
    );
    for line in textwrap::wrap(what, 50) {
        text.push_str(&format!("  {bar} {line}\n"));
    }
    text.push_str(&format!("  {bar} Nobody has answered it for a day.\n\n"));
    for line in 1..=30 {
        text.push_str(&format!("  body line {line}\n"));
    }
    snapshot.in_view = Some(item_in_view(held.id, text, None));
    app.refresh(snapshot);
    let shown = screen(&mut app, 60, 16);
    assert!(shown.contains("┃ proposal"), "{shown}");
    assert!(shown.contains("┃ it with `wf item cancel 4`"), "{shown}");
    assert!(shown.contains("answered it for a day."), "{shown}");
}

#[test]
fn scrolling_stops_at_the_end_of_a_detail() {
    let mut app = app();
    let spans = ItemId::from_bytes([2; 32]);
    app.open_item(spans);
    let mut snapshot = snapshot();
    let text: String = (1..=12).map(|line| format!("line {line}\n")).collect();
    snapshot.in_view = Some(item_in_view(spans, text, None));
    app.refresh(snapshot);
    // Drawn after each key, as the dashboard is.
    for _ in 0..20 {
        press(&mut app, KeyCode::Char('j'));
        screen(&mut app, 60, 10);
    }
    let at_end = screen(&mut app, 60, 10);
    assert!(at_end.contains("line 12"));
    press(&mut app, KeyCode::Char('k'));
    assert_ne!(screen(&mut app, 60, 10), at_end);
    press(&mut app, KeyCode::PageDown);
    assert_eq!(screen(&mut app, 60, 10), at_end);
    // Above the top of the log, which starts at its end.
    press(&mut app, KeyCode::Char('4'));
    let at_end = screen(&mut app, 60, 8);
    for _ in 0..10 {
        press(&mut app, KeyCode::Char('k'));
        screen(&mut app, 60, 8);
    }
    let at_top = screen(&mut app, 60, 8);
    assert_ne!(at_top, at_end);
    press(&mut app, KeyCode::PageUp);
    assert_eq!(screen(&mut app, 60, 8), at_top);
    press(&mut app, KeyCode::Char('j'));
    assert_ne!(screen(&mut app, 60, 8), at_top);
    // At its end, the log follows what is added.
    press(&mut app, KeyCode::PageDown);
    screen(&mut app, 60, 8);
    app.logged(["07:09  #ambqga  @ada · resolved b72c\n".to_owned()]);
    assert!(screen(&mut app, 60, 8).contains("b72c"));
}

#[test]
fn scrolled_up_text_holds_still_and_follows_again_at_its_end() {
    let mut app = app();
    press(&mut app, KeyCode::Char('4'));
    let lines = |from: usize, to: usize| -> Vec<String> {
        (from..=to)
            .map(|line| format!("07:{line:02}  @ada · line {line}\n"))
            .collect()
    };
    app.logged(lines(10, 30));
    screen(&mut app, 60, 10);
    press(&mut app, KeyCode::Char('k'));
    press(&mut app, KeyCode::Char('k'));
    // The body, without the latest event under it, nor the position at its right end, which
    // moves up as the text grows.
    let body = |app: &mut App| -> Vec<String> {
        screen(app, 60, 10)
            .lines()
            .skip(3)
            .take(4)
            .map(|line| line.chars().take(50).collect())
            .collect()
    };
    let scrolled = body(&mut app);
    // What is added below does not move what is being read.
    app.logged(lines(31, 35));
    assert_eq!(body(&mut app), scrolled);
    // Back at the end, the log follows again.
    for _ in 0..10 {
        press(&mut app, KeyCode::Char('j'));
        screen(&mut app, 60, 10);
    }
    app.logged(lines(36, 36));
    assert!(screen(&mut app, 60, 10).contains("line 36"));
}

#[test]
fn the_log_tab_follows_the_stream_and_filters_it() {
    let mut app = app();
    press(&mut app, KeyCode::Char('4'));
    insta::assert_snapshot!("log", screen(&mut app, 60, 9));
    press(&mut app, KeyCode::Char('/'));
    for character in "ambq".chars() {
        press(&mut app, KeyCode::Char(character));
    }
    // The filter applies while it is typed, and `q` is text, not quitting.
    assert_eq!(press(&mut app, KeyCode::Char('q')), None);
    press(&mut app, KeyCode::Backspace);
    insta::assert_snapshot!("log_filtered", screen(&mut app, 60, 9));
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.log_lines().count(), 1);
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.log_lines().count(), 2);
}

#[test]
fn a_permission_request_is_answered_where_it_is_listed() {
    let mut app = app();
    let coder = SessionId::from_bytes([11; 32]);
    assert_eq!(
        press(&mut app, KeyCode::Char('a')),
        Some(Effect::Permit {
            request: coders_request(),
            answer: Permission::Allow
        })
    );
    // On the coder's working session as well.
    press(&mut app, KeyCode::Char('G'));
    press(&mut app, KeyCode::Char('k'));
    press(&mut app, KeyCode::Char('k'));
    press(&mut app, KeyCode::Char('k'));
    assert_eq!(app.now.selection.get(), Some(&NowRow::Session(coder)));
    assert_eq!(
        press(&mut app, KeyCode::Char('d')),
        Some(Effect::Permit {
            request: coders_request(),
            answer: Permission::Deny
        })
    );
    // Nothing else waits on the parked coder.
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(press(&mut app, KeyCode::Char('a')), None);
}

#[test]
fn a_session_with_a_request_shows_it_under_its_transcript() {
    let mut app = app();
    let coder = app.snapshot.working[0].clone();
    app.transcribed(coder.id, &coder_transcript());
    press(&mut app, KeyCode::Enter);
    let mut snapshot = snapshot();
    snapshot.in_view = Some(session_in_view(coder));
    app.refresh(snapshot);
    insta::assert_snapshot!(screen(&mut app, 60, 12));
}

#[test]
fn a_blocker_is_answered_in_the_footer() {
    let mut app = app();
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(press(&mut app, KeyCode::Char('r')), None);
    for character in "Keep it on ParseError".chars() {
        // Letters that are keys elsewhere are text here.
        assert_eq!(press(&mut app, KeyCode::Char(character)), None);
    }
    insta::assert_snapshot!(screen(&mut app, 60, 8));
    assert_eq!(
        press(&mut app, KeyCode::Enter),
        Some(Effect::Resolve {
            item: ItemId::from_bytes([2; 32]),
            blocker: CommentId::from_bytes([21; 32]),
            text: "Keep it on ParseError".to_owned(),
        })
    );
    assert!(app.prompt.is_none());
    // An empty answer resolves nothing.
    press(&mut app, KeyCode::Char('r'));
    assert_eq!(press(&mut app, KeyCode::Enter), None);
}

#[test]
fn pasted_text_goes_into_the_prompt_with_line_breaks_as_spaces() {
    let mut app = app();
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('r'));
    assert!(app.paste("a\nx\r\ny"));
    // Nothing of it ran as keys: the prompt is open and nothing was answered.
    assert!(app.prompt.is_some());
    assert_eq!(
        press(&mut app, KeyCode::Enter),
        Some(Effect::Resolve {
            item: ItemId::from_bytes([2; 32]),
            blocker: CommentId::from_bytes([21; 32]),
            text: "a x y".to_owned(),
        })
    );
}

#[test]
fn pasted_text_without_a_prompt_is_ignored() {
    let mut app = app();
    assert!(!app.paste("a\nx\ny\n"));
    assert!(app.prompt.is_none());
}

#[test]
fn a_prompt_scrolls_by_cell_width_and_keeps_the_cursor_in_place() {
    let mut app = app();
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('r'));
    app.paste("日本語日本語日本語日本語日本語日本語日本語日本語日本語日本語end");
    let screen = screen(&mut app, 40, 8);
    let footer = screen
        .lines()
        .rev()
        .find(|line| line.contains("end"))
        .unwrap();
    // The end shows whole, and its start is cut at a character, not in the middle of one.
    // (The test backend prints a blank after each wide character.)
    let shown: String = footer.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(shown.contains("日本語日本語日本語end"), "{footer}");
}

#[test]
fn ctrl_g_carries_what_is_typed_into_the_editor() {
    use ratatui::crossterm::event::KeyModifiers;
    let ctrl_g = KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL);
    let mut app = app();
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('r'));
    for character in "Keep it".chars() {
        press(&mut app, KeyCode::Char(character));
    }
    // A plain g is text.
    assert_eq!(press(&mut app, KeyCode::Char('g')), None);
    assert_eq!(
        app.key(ctrl_g),
        Some(Effect::ResolveInEditor {
            item: ItemId::from_bytes([2; 32]),
            blocker: CommentId::from_bytes([21; 32]),
            text: "Keep itg".to_owned(),
        })
    );
    assert!(app.prompt.is_none());
    assert_eq!(
        press(&mut app, KeyCode::Char('R')),
        Some(Effect::ResolveInEditor {
            item: ItemId::from_bytes([2; 32]),
            blocker: CommentId::from_bytes([21; 32]),
            text: String::new(),
        }),
        "R starts from nothing"
    );
    press(&mut app, KeyCode::Char('c'));
    for character in "Longer than a line".chars() {
        press(&mut app, KeyCode::Char(character));
    }
    insta::assert_snapshot!(screen(&mut app, 80, 8));
    assert_eq!(
        app.key(ctrl_g),
        Some(Effect::CommentInEditor {
            item: ItemId::from_bytes([2; 32]),
            text: "Longer than a line".to_owned(),
        })
    );
    assert!(app.prompt.is_none());
}

#[test]
fn a_review_asks_for_the_verdict_and_offers_the_diff_first() {
    let mut app = app();
    let archive = ItemId::from_bytes([3; 32]);
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(press(&mut app, KeyCode::Char('v')), None);
    insta::assert_snapshot!(screen(&mut app, 60, 8));
    assert_eq!(press(&mut app, KeyCode::Char('D')), None);
    assert!(app.prompt.is_none(), "the diff takes the keys");
    assert_eq!(app.wanted().detail, Some(Detail::Diff(archive)));
    // The verdict is asked for again from the diff, where `v` selects lines.
    press(&mut app, KeyCode::Char('R'));
    assert_eq!(
        press(&mut app, KeyCode::Char('a')),
        Some(Effect::Approve {
            item: archive,
            reviewed: None,
            comments: Vec::new()
        })
    );
    press(&mut app, KeyCode::Char('R'));
    assert_eq!(
        press(&mut app, KeyCode::Char('r')),
        Some(Effect::RequestChanges {
            item: archive,
            reviewed: None,
            comments: Vec::new()
        })
    );
}

#[test]
fn what_an_action_did_shows_until_the_next_key() {
    let mut app = app();
    app.did("allowed: rm -rf target".to_owned(), NOW_MS);
    assert!(screen(&mut app, 60, 8).contains("✓ allowed: rm -rf target"));
    app.refresh(snapshot());
    assert!(
        screen(&mut app, 60, 8).contains("✓ allowed"),
        "a refresh keeps it"
    );
    press(&mut app, KeyCode::Char('j'));
    assert!(!screen(&mut app, 60, 8).contains("✓ allowed"));
}

#[test]
fn an_action_error_goes_away_after_its_time() {
    let mut app = app();
    app.action_failed(&anyhow::anyhow!("the daemon refused"), NOW_MS);
    let later = |ms| Snapshot {
        now_ms: NOW_MS + ms,
        ..snapshot()
    };
    app.refresh(later(STATUS_LASTS_MS - 1));
    assert!(screen(&mut app, 60, 8).contains("error: the daemon refused"));
    app.refresh(later(STATUS_LASTS_MS));
    assert!(!screen(&mut app, 60, 8).contains("error"));

    app.did("allowed: rm -rf target".to_owned(), NOW_MS);
    app.refresh(later(STATUS_LASTS_MS));
    assert!(!screen(&mut app, 60, 8).contains("✓ allowed"));
}

#[test]
fn the_bar_of_a_selected_row_of_two_lines_looks_the_same_on_both() {
    let mut app = app();
    press(&mut app, KeyCode::Char('j'));
    let drawn = Drawn::of(&mut app, 60, 22);
    let (_, first) = drawn.find("⧗ #aibaea blocker");
    let lines = drawn.lines();
    let shown: String = (first..first + 2)
        .map(|y| {
            let line = &lines[usize::from(y)];
            let (bar, text) = (drawn.style_at(0, y), drawn.style_at(3, y));
            format!("{line:?}\n  bar:  {bar:?}\n  text: {text:?}\n")
        })
        .collect();
    assert_eq!(drawn.style_at(0, first), drawn.style_at(0, first + 1));
    insta::assert_snapshot!(shown);
}

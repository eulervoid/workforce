//! Dashboard tests: the entries of a transcript, opened and closed one at a time.

use super::support::*;
use crate::dashboard::app::App;
use ratatui::crossterm::event::KeyCode;
use serde_json::json;
use wf_harness::{Direction, TranscriptEntry};

/// A call `id` of Bash whose output is 30 lines, `name 1` to `name 30`, done at `seconds`.
fn long_call(seconds: i64, id: &str, name: &str) -> Vec<TranscriptEntry> {
    let output: String = (1..=30).map(|line| format!("{name} {line}\n")).collect();
    vec![
        update(
            seconds,
            json!({"sessionUpdate": "tool_call", "toolCallId": id, "name": "Bash", "title": format!("print {name}")}),
        ),
        update(
            seconds + 1,
            json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": "completed",
                "content": [{"type": "content", "content": {"type": "text", "text": output}}]}),
        ),
    ]
}

/// The coder's session open on a transcript of a prompt and two calls with long output.
fn transcript_app() -> App {
    let mut app = app();
    let coder = app.snapshot.working[0].clone();
    let mut entries = vec![entry(
        0,
        Direction::ToAgent,
        json!({"id": 1, "method": "session/prompt", "params": {"prompt": [{"type": "text", "text": "Print both."}]}}),
    )];
    entries.extend(long_call(1, "1", "first"));
    entries.extend(long_call(3, "2", "second"));
    app.transcribed(coder.id, &entries);
    open_through_item(&mut app, coder.id);
    let mut snapshot = snapshot();
    snapshot.in_view = Some(session_in_view(coder));
    app.refresh(snapshot);
    app
}

/// The screen, tall enough for both calls in full.
fn tall(app: &mut App) -> String {
    screen(app, 80, 90)
}

/// The line of the screen with `text`.
fn line_with(app: &mut App, text: &str) -> String {
    let shown = tall(app);
    shown
        .lines()
        .find(|line| line.contains(text))
        .unwrap_or_else(|| panic!("{text} on screen: {shown}"))
        .to_owned()
}

#[test]
fn a_long_output_is_abridged_and_enter_opens_and_closes_only_the_entry_under_the_cursor() {
    let mut app = transcript_app();
    let shown = tall(&mut app);
    assert!(
        shown.contains("first 1") && shown.contains("first 30"),
        "{shown}"
    );
    assert!(!shown.contains("first 15"), "abridged: {shown}");
    assert!(shown.contains("20 lines hidden"), "{shown}");
    assert!(line_with(&mut app, "print first").contains("▸ "));
    // The cursor starts on the last entry, and goes up to the first call.
    assert!(line_with(&mut app, "print second").contains('▎'));
    press(&mut app, KeyCode::Char('k'));
    assert!(line_with(&mut app, "print first").contains('▎'));
    assert!(!line_with(&mut app, "print second").contains('▎'));

    press(&mut app, KeyCode::Enter);
    let shown = tall(&mut app);
    assert!(shown.contains("first 15"), "in full: {shown}");
    assert!(
        !shown.contains("second 15"),
        "the other stays abridged: {shown}"
    );
    assert!(line_with(&mut app, "print first").contains("▾ "));
    assert!(line_with(&mut app, "print second").contains("▸ "));

    press(&mut app, KeyCode::Enter);
    let shown = tall(&mut app);
    assert!(!shown.contains("first 15"), "abridged again: {shown}");
    assert!(line_with(&mut app, "print first").contains("▸ "));
}

#[test]
fn f_opens_every_entry_and_one_closed_by_hand_after_keeps_its_own_state() {
    let mut app = transcript_app();
    tall(&mut app);
    press(&mut app, KeyCode::Char('f'));
    let shown = tall(&mut app);
    assert!(
        shown.contains("first 15") && shown.contains("second 15"),
        "{shown}"
    );
    // The cursor is on the last entry again; closing it leaves the first open.
    press(&mut app, KeyCode::Enter);
    let shown = tall(&mut app);
    assert!(shown.contains("first 15"), "{shown}");
    assert!(!shown.contains("second 15"), "{shown}");
    // `f` sets them all again, whatever was closed by hand.
    press(&mut app, KeyCode::Char('f'));
    press(&mut app, KeyCode::Char('f'));
    let shown = tall(&mut app);
    assert!(
        shown.contains("first 15") && shown.contains("second 15"),
        "{shown}"
    );
}

#[test]
fn an_entry_opened_stays_open_as_the_transcript_grows_and_the_end_is_followed_from_the_last() {
    let mut app = transcript_app();
    tall(&mut app);
    press(&mut app, KeyCode::Char('k'));
    press(&mut app, KeyCode::Enter);
    let coder = app.snapshot.working[0].id;
    app.transcribed(coder, &long_call(5, "3", "third"));
    let mut snapshot = snapshot();
    snapshot.in_view = Some(session_in_view(app.snapshot.working[0].clone()));
    app.refresh(snapshot);
    let shown = tall(&mut app);
    assert!(shown.contains("first 15"), "still open: {shown}");
    assert!(!shown.contains("third 15"), "{shown}");
    assert!(line_with(&mut app, "print first").contains('▎'));
    // On the last entry the cursor goes along with what is added, and the screen with it.
    press(&mut app, KeyCode::Char('G'));
    assert!(line_with(&mut app, "print third").contains('▎'));
    app.transcribed(coder, &long_call(7, "4", "fourth"));
    let shown = screen(&mut app, 80, 16);
    assert!(shown.contains("fourth 30"), "the end is followed: {shown}");
    assert!(line_with(&mut app, "print fourth").contains('▎'));
}

#[test]
fn a_click_on_the_first_line_of_an_entry_opens_it() {
    let mut app = transcript_app();
    let (column, row) = find(&mut app, 80, 90, "print first");
    app.click(column, row);
    let shown = tall(&mut app);
    assert!(shown.contains("first 15"), "{shown}");
    assert!(line_with(&mut app, "print first").contains('▎'));
    // A click on another line of it only moves the cursor there.
    let (column, row) = find(&mut app, 80, 90, "second 1");
    app.click(column, row);
    let shown = tall(&mut app);
    assert!(!shown.contains("second 15"), "{shown}");
    assert!(line_with(&mut app, "print second").contains('▎'));
}

/// The coder's session, with a call of Bash open since second 10 whose command is `title`.
fn app_with_open_call(title: &str) -> App {
    let mut app = transcript_app();
    let coder = app.snapshot.working[0].clone();
    app.transcribed(
        coder.id,
        &[
            update(
                10,
                json!({"sessionUpdate": "tool_call", "toolCallId": "3", "name": "Bash", "title": "Terminal", "kind": "execute", "status": "pending"}),
            ),
            update(
                13,
                json!({"sessionUpdate": "tool_call_update", "toolCallId": "3", "title": title}),
            ),
        ],
    );
    app
}

/// Agents edit first and run the tests on the last line of one long command.
#[test]
fn an_open_call_shows_the_end_of_its_command() {
    let script: String = (1..=30).map(|line| format!("edit {line}\n")).collect();
    let mut app = app_with_open_call(&format!("python3 - <<'E'\n{script}E\ncargo test -p wf-cli"));
    let shown = tall(&mut app);
    assert!(shown.contains("python3 - <<'E'"), "{shown}");
    assert!(shown.contains("cargo test -p wf-cli"), "{shown}");
    assert!(shown.contains("lines hidden"), "{shown}");
    assert!(!shown.contains("edit 15"), "abridged: {shown}");
}

#[test]
fn a_long_command_shown_wraps_instead_of_being_cut_while_it_runs() {
    let mut app = app_with_open_call(&format!("{} && cargo test", "x".repeat(100)));
    let shown = tall(&mut app);
    assert!(shown.contains("cargo test"), "{shown}");
}

#[test]
fn an_open_call_says_how_long_it_has_been_running() {
    let mut app = app_with_open_call("cargo test");
    app.tick(100_000);
    assert!(
        line_with(&mut app, "cargo test").contains("running 1m"),
        "90 seconds in"
    );
}

#[test]
fn the_scroll_position_leaves_the_state_of_an_open_call_alone() {
    let mut app = app_with_open_call("cargo test");
    app.tick(100_000);
    let shown = screen(&mut app, 80, 20);
    assert!(shown.contains("running 1m"), "{shown}");
    assert!(shown.contains(" 100% "), "{shown}");
}

#[test]
fn a_finished_call_folds_its_command_back_to_the_first_line() {
    let mut app = app_with_open_call("python3 - <<'E'\nedit\nE\ncargo test -p wf-cli");
    let coder = app.snapshot.working[0].clone();
    app.transcribed(
        coder.id,
        &[update(
            20,
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "3", "status": "completed"}),
        )],
    );
    let shown = tall(&mut app);
    assert!(shown.contains("python3 - <<'E' …"), "{shown}");
    assert!(!shown.contains("cargo test -p wf-cli"), "{shown}");
}

//! Dashboard tests: the files each block of calls changed, under it in the transcript, opened to
//! their diff.

use super::support::*;
use crate::dashboard::app::App;
use ratatui::crossterm::event::KeyCode;
use serde_json::json;
use wf_core::id::SessionId;
use wf_core::session::GitOid;
use wf_harness::{Direction, TranscriptEntry};

const CHANGED: &str = "\
diff --git a/src/lib.rs b/src/lib.rs
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,2 +1,2 @@
 fn main() {
-    old();
+    new();
diff --git a/notes.md b/notes.md
deleted file mode 100644
--- a/notes.md
+++ /dev/null
@@ -1 +0,0 @@
-draft
";

fn tree(byte: u8) -> GitOid {
    GitOid::from_bytes([byte; 20])
}

/// A call of Bash that changed the files through the shell, and the text the agent wrote after
/// it, whose first chunk ended the block: its snapshot comes in the middle of the text.
fn block() -> Vec<TranscriptEntry> {
    vec![
        entry(
            0,
            Direction::ToAgent,
            json!({"id": 1, "method": "session/prompt", "params": {"prompt": [{"type": "text", "text": "Tidy up."}]}}),
        ),
        update(
            1,
            json!({"sessionUpdate": "tool_call", "toolCallId": "1", "name": "Bash", "title": "sed -i s/old/new/ src/lib.rs && rm notes.md"}),
        ),
        update(
            2,
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "1", "status": "completed"}),
        ),
        update(
            3,
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Tidied"}}),
        ),
        entry(
            3,
            Direction::Wf,
            json!({
                "snapshot": tree(2).to_string(), "at": "block", "left_out": [],
                "changes": {"against": tree(1).to_string(), "files": [
                    {"path": "src/lib.rs", "change": "modified", "added": 1, "removed": 1},
                    {"path": "notes.md", "change": "deleted", "added": 0, "removed": 1},
                ]},
            }),
        ),
        update(
            4,
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": " up.\n\n"}}),
        ),
    ]
}

/// The coder's session open on [`block`].
fn changes_app() -> (App, SessionId) {
    let mut app = app();
    let coder = app.snapshot.working[0].clone();
    app.transcribed(coder.id, &block());
    open_through_item(&mut app, coder.id);
    let mut snapshot = snapshot();
    snapshot.in_view = Some(session_in_view(coder.clone()));
    app.refresh(snapshot);
    (app, coder.id)
}

/// The line of `shown` with `text`.
fn line_with<'a>(shown: &'a str, text: &str) -> &'a str {
    shown
        .lines()
        .find(|line| line.contains(text))
        .unwrap_or_else(|| panic!("{text} on screen: {shown}"))
}

#[test]
fn the_files_a_block_changed_are_folded_under_it_and_enter_opens_them_to_their_diff() {
    let (mut app, coder) = changes_app();
    insta::assert_snapshot!("changes_folded", screen(&mut app, 70, 16));

    // The cursor is on the text after the block; up is the files.
    press(&mut app, KeyCode::Char('k'));
    press(&mut app, KeyCode::Enter);
    let shown = screen(&mut app, 70, 16);
    assert!(
        line_with(&shown, "2 files changed").contains('▾'),
        "{shown}"
    );
    assert!(shown.contains("asking for the diff…"), "{shown}");
    let lines = app.transcript_lines.get_mut(&coder).unwrap();
    let transcript = &app.transcripts[&coder];
    let unread = lines.unread_changes(transcript);
    let [(block, snapshot, against)] = unread.as_slice() else {
        panic!("one entry to read: {unread:?}");
    };
    assert_eq!((*snapshot, *against), (tree(2), tree(1)));
    lines.read_changes(*block, Ok(wf_vcs::diff::parse(CHANGED)), transcript);
    assert!(lines.unread_changes(transcript).is_empty(), "read once");
    insta::assert_snapshot!("changes_opened", screen(&mut app, 70, 20));

    press(&mut app, KeyCode::Enter);
    let shown = screen(&mut app, 70, 16);
    assert!(
        line_with(&shown, "2 files changed").contains('▸'),
        "{shown}"
    );
    assert!(!shown.contains("new();"), "folded again: {shown}");
}

#[test]
fn changes_whose_diff_cannot_be_read_say_why_under_their_files() {
    let (mut app, coder) = changes_app();
    screen(&mut app, 70, 16);
    press(&mut app, KeyCode::Char('k'));
    press(&mut app, KeyCode::Enter);
    let lines = app.transcript_lines.get_mut(&coder).unwrap();
    let transcript = &app.transcripts[&coder];
    let (block, ..) = lines.unread_changes(transcript)[0];
    lines.read_changes(
        block,
        Err("the session ran on desktop, which cannot be reached: timed out".to_owned()),
        transcript,
    );
    let shown = screen(&mut app, 70, 16);
    assert!(shown.contains("notes.md (deleted)"), "{shown}");
    assert!(
        shown.contains("no diff: the session ran on desktop"),
        "{shown}"
    );
}

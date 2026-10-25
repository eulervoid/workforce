//! Dashboard tests: diff.

use super::support::*;
use crate::dashboard::app::{App, Effect, Prompt, Status, Wheel};
use crate::dashboard::snapshot::{Detail, InView};
use ratatui::crossterm::event::KeyCode;
use wf_core::id::{ItemId, SessionId};

#[test]
fn the_diff_says_when_the_agent_has_not_made_any_commits_or_they_change_no_files() {
    let mut app = diff_app();
    let pane = app.diff_on_top_mut().unwrap();
    pane.read(Ok(crate::dashboard::diff::ItemWork {
        commit: work_commit(),
        changes: crate::dashboard::diff::Changes::NoCommits,
        uncommitted: None,
    }));
    let screen_text = screen(&mut app, 60, 24);
    assert!(
        screen_text.contains("the agent has not made any commits"),
        "{screen_text}"
    );
    let pane = app.diff_on_top_mut().unwrap();
    pane.read(Ok(crate::dashboard::diff::ItemWork {
        commit: work_commit(),
        changes: crate::dashboard::diff::Changes::Files(Vec::new()),
        uncommitted: None,
    }));
    let screen_text = screen(&mut app, 60, 24);
    assert!(
        screen_text.contains("the work changes no files"),
        "{screen_text}"
    );
}

#[test]
fn d_opens_the_diff_of_a_review_with_files_hunks_and_numbered_lines() {
    let mut app = diff_app();
    insta::assert_snapshot!(screen(&mut app, 60, 24));
}

#[test]
fn the_cursor_moves_over_files_and_lines_and_enter_folds_a_file() {
    let mut app = diff_app();
    let cursor_line = |app: &mut App| {
        screen(app, 60, 24)
            .lines()
            .find(|line| line.contains('▎'))
            .unwrap_or_default()
            .to_owned()
    };
    assert!(cursor_line(&mut app).contains("src/lib.rs"));
    // The heading of a hunk is passed over.
    press(&mut app, KeyCode::Char('j'));
    assert!(cursor_line(&mut app).contains("use std::io;"));
    press(&mut app, KeyCode::Char(']'));
    assert!(cursor_line(&mut app).contains("README.md"));
    press(&mut app, KeyCode::Char('['));
    assert!(cursor_line(&mut app).contains("src/lib.rs"));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Enter);
    let folded = screen(&mut app, 60, 24);
    assert!(cursor_line(&mut app).contains("src/lib.rs"), "{folded}");
    assert!(!folded.contains("use std::io;"), "{folded}");
    press(&mut app, KeyCode::Char('G'));
    assert!(cursor_line(&mut app).contains("Indented."));
    // `i` goes to the item, `D` back to the diff.
    press(&mut app, KeyCode::Char('i'));
    assert!(matches!(app.path().last(), Some(Detail::Item(_))));
    press(&mut app, KeyCode::Char('D'));
    assert!(matches!(app.path().last(), Some(Detail::Diff(_))));
    assert_eq!(
        press(&mut app, KeyCode::Char('p')),
        Some(Effect::PageDiff {
            item: ItemId::from_bytes([3; 32])
        })
    );
}

#[test]
fn i_in_a_diff_opened_from_the_list_replaces_it_with_its_item() {
    let mut app = diff_app();
    press(&mut app, KeyCode::Char('i'));
    assert_eq!(app.path(), [Detail::Item(ItemId::from_bytes([3; 32]))]);
}

#[test]
fn i_in_a_diff_or_session_opened_from_its_item_goes_back_to_the_item() {
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
    let item_scroll = app.stack().last().unwrap().scroll;
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.path(), [Detail::Item(parse), Detail::Session(coder)]);
    press(&mut app, KeyCode::Char('i'));
    assert_eq!(app.path(), [Detail::Item(parse)]);
    assert_eq!(app.stack().last().unwrap().scroll, item_scroll);
    // The diff, opened from the item.
    press(&mut app, KeyCode::Char('D'));
    assert_eq!(app.path(), [Detail::Item(parse), Detail::Diff(parse)]);
    press(&mut app, KeyCode::Char('i'));
    assert_eq!(app.path(), [Detail::Item(parse)]);
}

#[test]
fn i_in_a_diff_below_an_item_and_its_session_goes_back_to_the_item() {
    let mut app = app();
    press(&mut app, KeyCode::Char('2'));
    let parse = ItemId::from_bytes([1; 32]);
    let mut snapshot = with_items();
    snapshot.in_view = Some(item_in_view(
        parse,
        String::new(),
        Some(SessionId::from_bytes([11; 32])),
    ));
    app.refresh(snapshot);
    press(&mut app, KeyCode::Char('G'));
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Enter);
    let mut snapshot = with_items();
    snapshot.in_view = Some(session_in_view(snapshot.working[0].clone()));
    app.refresh(snapshot);
    press(&mut app, KeyCode::Enter);
    assert!(matches!(
        app.path()[..],
        [Detail::Item(_), Detail::Session(_), Detail::Diff(_)]
    ));
    press(&mut app, KeyCode::Char('i'));
    assert_eq!(app.path(), [Detail::Item(parse)]);
}

#[test]
fn i_in_a_diff_opened_from_a_session_replaces_it_with_the_item() {
    let mut app = app();
    app.refresh(snapshot());
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Right);
    assert!(matches!(
        app.path()[..],
        [Detail::Session(_), Detail::Diff(_)]
    ));
    press(&mut app, KeyCode::Char('i'));
    assert!(matches!(
        app.path()[..],
        [Detail::Session(_), Detail::Item(_)]
    ));
}

#[test]
fn the_wheel_scrolls_the_diff_and_takes_the_cursor_along() {
    let mut app = diff_app();
    screen(&mut app, 60, 12);
    app.wheel(30, Wheel::Down);
    app.wheel(30, Wheel::Down);
    let shown = screen(&mut app, 60, 12);
    assert!(!shown.contains("src/lib.rs"), "{shown}");
    let cursor = shown.lines().find(|line| line.contains('▎'));
    assert!(cursor.is_some(), "the cursor is on screen: {shown}");
}

#[test]
fn v_selects_lines_within_their_hunk_and_esc_ends_the_selection() {
    let mut app = diff_app();
    // On `use std::io;`, the first line of the first hunk.
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('v'));
    assert!(
        app.prompt.is_none(),
        "v selects in a diff rather than review"
    );
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    insta::assert_snapshot!(screen(&mut app, 60, 16));
    // The selection stays in its hunk.
    for _ in 0..5 {
        press(&mut app, KeyCode::Char('j'));
    }
    let pane = app.diff_on_top().unwrap();
    let rows = pane.rows();
    let selected = pane.selected().unwrap();
    assert_eq!(selected.clone().count(), 4);
    assert!(
        selected
            .map(|row| rows[row])
            .all(|row| matches!(row, crate::dashboard::diff::DiffRow::Line { hunk: 0, .. }))
    );
    press(&mut app, KeyCode::Esc);
    assert!(app.diff_on_top().unwrap().selection.is_none());
    assert!(
        matches!(app.path().last(), Some(Detail::Diff(_))),
        "esc ended only the selection"
    );
    // `R` asks for the verdict on the review, as `v` does elsewhere.
    press(&mut app, KeyCode::Char('R'));
    assert!(matches!(app.prompt, Some(Prompt::Verdict { .. })));
}

#[test]
fn a_drag_selects_lines_and_a_click_moves_the_cursor() {
    let mut app = diff_app();
    let (column, row) = find(&mut app, 60, 24, "fn parse() {}");
    assert_eq!(app.click(column, row), None);
    let pane = app.diff_on_top().unwrap();
    assert_eq!(pane.cursor, 3, "the cursor is on the line clicked");
    assert!(pane.selection.is_none());
    let (_, below) = find(&mut app, 60, 24, "fn main() {}");
    app.drag(column, below);
    let pane = app.diff_on_top().unwrap();
    assert_eq!(pane.selected(), Some(3..=5));
    // A drag past the hunk keeps to it.
    app.drag(column, 22);
    assert_eq!(app.diff_on_top().unwrap().selected(), Some(3..=5));
}

#[test]
fn a_change_request_on_selected_lines_is_a_draft_under_them_until_the_verdict_sends_it() {
    let mut app = diff_app();
    let archive = ItemId::from_bytes([3; 32]);
    // From `use std::io;` to `+fn parse_lazily() {}`.
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('v'));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('r'));
    type_text(&mut app, "Keep the old name, callers use it.");
    insta::assert_snapshot!("line_comment_prompt", screen(&mut app, 80, 16));
    press(&mut app, KeyCode::Enter);
    let drafts = app.drafts(archive).to_vec();
    assert_eq!(drafts.len(), 1);
    let lines = &drafts[0].lines;
    assert_eq!(drafts[0].kind, wf_core::event::CommentKind::ChangeRequest);
    assert_eq!(lines.location(), "src/lib.rs:1-2");
    assert_eq!(lines.side, wf_core::event::DiffSide::New);
    assert_eq!(lines.commit, work_commit());
    assert_eq!(
        lines.quote,
        " use std::io;\n-fn parse() {}\n+fn parse_lazily() {}"
    );
    assert!(app.diff_on_top().unwrap().selection.is_none());
    insta::assert_snapshot!("line_comment_draft", screen(&mut app, 80, 16));
    // Approving with a change request drafted is refused; requesting changes sends it.
    press(&mut app, KeyCode::Char('R'));
    assert!(screen(&mut app, 80, 16).contains("with 1 comment on lines"));
    assert_eq!(press(&mut app, KeyCode::Char('a')), None);
    assert!(matches!(app.status, Some(Status::ActionFailed { .. })));
    press(&mut app, KeyCode::Char('R'));
    assert_eq!(
        press(&mut app, KeyCode::Char('r')),
        Some(Effect::RequestChanges {
            item: archive,
            reviewed: Some(work_commit()),
            comments: drafts
        })
    );
    app.reviewed(archive);
    assert!(app.drafts(archive).is_empty());
}

#[test]
fn a_draft_under_the_cursor_is_edited_with_e_and_dropped_with_d() {
    let mut app = diff_app();
    let archive = ItemId::from_bytes([3; 32]);
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('c'));
    type_text(&mut app, "Why io?");
    press(&mut app, KeyCode::Enter);
    // The cursor stops on the draft, under its line.
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.diff_on_top().unwrap().draft_at_cursor(), Some(0));
    assert!(screen(&mut app, 80, 16).contains("e edit · d drop"));
    press(&mut app, KeyCode::Char('e'));
    type_text(&mut app, " It is unused.");
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.drafts(archive)[0].text, "Why io? It is unused.");
    assert_eq!(app.drafts(archive).len(), 1);
    press(&mut app, KeyCode::Char('d'));
    assert!(app.drafts(archive).is_empty());
    // The drafts go with the verdict, whichever it is.
    press(&mut app, KeyCode::Char('c'));
    type_text(&mut app, "Fine.");
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Char('R'));
    assert!(matches!(
        press(&mut app, KeyCode::Char('a')),
        Some(Effect::Approve { comments, .. }) if comments.len() == 1
    ));
}

#[test]
fn a_terminal_too_short_for_the_body_draws_every_tab_without_panicking() {
    for height in 1..=9 {
        for width in [1, 20, 60, 130] {
            let mut app = app();
            app.refresh(with_items());
            for tab in ['1', '2', '3', '4'] {
                press(&mut app, KeyCode::Char(tab));
                screen(&mut app, width, height);
                // The list scrolled away from its first line has indicators above it.
                app.wheel(0, Wheel::Down);
                screen(&mut app, width, height);
                press(&mut app, KeyCode::Enter);
                screen(&mut app, width, height);
                press(&mut app, KeyCode::Esc);
            }
        }
    }
}

#[test]
fn a_diff_is_let_go_of_once_closed_unless_a_review_is_being_written() {
    let mut app = diff_app();
    let archive = ItemId::from_bytes([3; 32]);
    app.refresh(snapshot());
    assert!(app.diffs.contains_key(&archive), "open");
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('c'));
    type_text(&mut app, "Why io?");
    press(&mut app, KeyCode::Enter);
    // Moving the selection closes what was opened from the list.
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('k'));
    assert!(app.stack().is_empty());
    app.refresh(snapshot());
    assert!(app.diffs.contains_key(&archive), "its draft is kept");
    app.reviewed(archive);
    app.refresh(snapshot());
    assert!(app.diffs.is_empty());
}

#[test]
fn a_click_on_drop_in_a_diff_drops_the_draft_instead_of_denying() {
    let mut app = diff_app();
    let archive = ItemId::from_bytes([3; 32]);
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('c'));
    type_text(&mut app, "Why io?");
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.drafts(archive).len(), 1);
    let (column, row) = find(&mut app, 80, 16, "d drop");
    assert_eq!(app.click(column, row), None);
    assert!(app.drafts(archive).is_empty());
}

#[test]
fn without_a_review_a_comment_on_lines_is_posted_at_once() {
    let mut app = app();
    let parse = ItemId::from_bytes([1; 32]);
    app.open_item(parse);
    press(&mut app, KeyCode::Char('l'));
    assert_eq!(app.wanted().detail, Some(Detail::Diff(parse)));
    app.diff_on_top_mut()
        .unwrap()
        .read(Ok(crate::dashboard::diff::ItemWork {
            commit: work_commit(),
            changes: crate::dashboard::diff::Changes::Files(wf_vcs::diff::parse(DIFF)),
            uncommitted: None,
        }));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('c'));
    type_text(&mut app, "Removed on purpose?");
    let Some(Effect::PostLineComment { item, comment }) = press(&mut app, KeyCode::Enter) else {
        panic!("a comment posted");
    };
    assert_eq!(item, parse);
    // A removed line alone counts on the old side.
    assert_eq!(comment.lines.side, wf_core::event::DiffSide::Old);
    assert_eq!(comment.lines.location(), "src/lib.rs:2");
    assert!(app.drafts(parse).is_empty());
}

#[test]
fn comments_in_the_log_show_under_their_lines_or_as_outdated_under_their_file() {
    let mut app = diff_app();
    let archive = ItemId::from_bytes([3; 32]);
    let posted =
        |commit: wf_core::session::CommitId, text: &str| crate::dashboard::diff::PostedComment {
            by: "@ada".to_owned(),
            comment: crate::dashboard::diff::LineComment {
                kind: wf_core::event::CommentKind::ChangeRequest,
                lines: wf_core::event::LineAnchor {
                    commit,
                    path: "src/lib.rs".to_owned(),
                    side: wf_core::event::DiffSide::New,
                    first: 21,
                    last: 21,
                    quote: "+// really".to_owned(),
                },
                text: text.to_owned(),
            },
            resolved: false,
        };
    let older = wf_core::session::CommitId::Git(
        "0000000000000000000000000000000000000001".parse().unwrap(),
    );
    app.refresh_detail(Some(InView::Diff {
        id: archive,
        comments: vec![
            posted(work_commit(), "Say why."),
            posted(older, "Was this here before?"),
        ],
    }));
    insta::assert_snapshot!(screen(&mut app, 80, 24));
}

#[test]
fn a_refresh_clears_only_the_failure_of_reading_the_workforce() {
    let mut app = app();
    app.refresh_failed(&anyhow::anyhow!("daemon gone"));
    app.refresh(snapshot());
    assert!(matches!(app.status, Some(Status::RefreshFailed(_))));
    app.refreshed();
    assert_eq!(app.status, None);
    // A transcript that could not be read stays for the next refresh to find, until a key.
    app.transcript_failed(&anyhow::anyhow!("malformed line"));
    app.refreshed();
    app.refresh(snapshot());
    assert!(matches!(app.status, Some(Status::TranscriptFailed { .. })));
    assert!(screen(&mut app, 60, 16).contains("error: malformed line"));
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.status, None);
}

#[test]
fn a_diff_read_from_another_device_says_where_and_keeps_the_last_one_while_it_is_unreachable() {
    let device = wf_core::identity::DeviceSigner::generate([9; 32]).key();
    let mut app = diff_app();
    let now_ms = app.snapshot.now_ms;
    let pane = app.diff_on_top_mut().unwrap();
    let work = crate::dashboard::diff::ItemWork {
        commit: work_commit(),
        changes: crate::dashboard::diff::Changes::Files(wf_vcs::diff::parse(DIFF)),
        uncommitted: None,
    };
    pane.read_from_device(device, work, now_ms.saturating_sub(120_000));
    let shown = screen(&mut app, 100, 24);
    assert!(
        shown.contains(&format!("committed on {}, 2 min ago", device.fmt_short())),
        "{shown}"
    );
    assert!(!shown.contains("unreachable"), "{shown}");

    let pane = app.diff_on_top_mut().unwrap();
    pane.device_unreachable(device, "timed out".to_owned());
    let shown = screen(&mut app, 120, 24);
    assert!(
        shown.contains("unreachable, as of 2 min ago: timed out"),
        "{shown}"
    );
    // The last diff stays on screen.
    assert!(!app.diff_on_top_mut().unwrap().files().is_empty());

    let pane = app.diff_on_top_mut().unwrap();
    pane.device_unchanged(device, now_ms);
    let shown = screen(&mut app, 120, 24);
    assert!(shown.contains("just now"), "{shown}");
    assert!(!shown.contains("unreachable"), "{shown}");
}

#[test]
fn a_device_that_cannot_be_reached_before_any_diff_was_read_is_said_so() {
    let device = wf_core::identity::DeviceSigner::generate([9; 32]).key();
    let mut app = diff_app();
    let pane = app.diff_on_top_mut().unwrap();
    pane.device_unreachable(device, "timed out".to_owned());
    let shown = screen(&mut app, 100, 24);
    assert!(shown.contains("cannot be reached: timed out"), "{shown}");
}

#[test]
fn a_device_asked_is_not_asked_again_within_the_interval_even_when_unreachable() {
    use crate::dashboard::Asks;

    let item = ItemId::from_bytes([3; 32]);
    let mut asked = Asks::new(std::time::Duration::from_secs(3));
    assert!(asked.due(item));
    asked.asked(item);
    assert!(!asked.due(item));
    assert!(asked.due(ItemId::from_bytes([4; 32])));
    asked.forget(item);
    assert!(asked.due(item));
}

const UNCOMMITTED: &str = "\
diff --git a/src/scratch.rs b/src/scratch.rs
new file mode 100644
index 0000000..3333333
--- /dev/null
+++ b/src/scratch.rs
@@ -0,0 +1,2 @@
+// not committed
+fn scratch() {}
";

#[test]
fn the_uncommitted_part_is_a_section_after_the_committed_files_where_no_comment_is_written() {
    let mut app = diff_app();
    let pane = app.diff_on_top_mut().unwrap();
    pane.read(Ok(crate::dashboard::diff::ItemWork {
        commit: work_commit(),
        changes: crate::dashboard::diff::Changes::Files(wf_vcs::diff::parse(DIFF)),
        uncommitted: Some(wf_vcs::diff::parse(UNCOMMITTED)),
    }));
    let committed_files = pane.files().len();
    let shown = screen(&mut app, 100, 40);
    assert!(
        shown.contains("comments on lines only on committed"),
        "{shown}"
    );
    assert!(shown.contains("━━ uncommitted, against"), "{shown}");
    assert!(
        shown.contains("src/scratch.rs (new) (uncommitted)"),
        "{shown}"
    );
    assert!(shown.find("src/lib.rs").unwrap() < shown.find("src/scratch.rs").unwrap());
    assert!(!shown[..shown.find("━━ uncommitted").unwrap()].contains("(uncommitted)"));

    // A line of the committed part takes a comment; one of the uncommitted part does not.
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('c'));
    assert!(matches!(app.prompt, Some(Prompt::LineComment { .. })));
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('G'));
    let pane = app.diff_on_top_mut().unwrap();
    let rows = pane.rows();
    assert!(matches!(
        rows[pane.cursor],
        crate::dashboard::diff::DiffRow::Line { file, .. } if file >= committed_files
    ));
    press(&mut app, KeyCode::Char('c'));
    assert!(app.prompt.is_none());

    // Once the agent repository is gone only the commits are left.
    let pane = app.diff_on_top_mut().unwrap();
    pane.read(Ok(crate::dashboard::diff::ItemWork {
        commit: work_commit(),
        changes: crate::dashboard::diff::Changes::Files(wf_vcs::diff::parse(DIFF)),
        uncommitted: None,
    }));
    let shown = screen(&mut app, 100, 40);
    assert!(!shown.contains("uncommitted"), "{shown}");
}

#[test]
fn the_pane_says_the_agent_made_no_commits_while_it_shows_the_uncommitted_part() {
    let mut app = diff_app();
    let pane = app.diff_on_top_mut().unwrap();
    pane.read(Ok(crate::dashboard::diff::ItemWork {
        commit: work_commit(),
        changes: crate::dashboard::diff::Changes::NoCommits,
        uncommitted: Some(wf_vcs::diff::parse(UNCOMMITTED)),
    }));
    let shown = screen(&mut app, 120, 40);
    assert!(shown.contains("has not made any commits"), "{shown}");
    assert!(
        shown.contains("src/scratch.rs (new) (uncommitted)"),
        "{shown}"
    );
}

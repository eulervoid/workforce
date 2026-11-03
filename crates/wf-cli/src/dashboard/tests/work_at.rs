//! Dashboard tests: the work at a comment, picked with `w` or opened from the comment's id.

use super::support::*;
use crate::dashboard::act;
use crate::dashboard::app::{App, Choice, Effect, Prompt};
use crate::dashboard::diff::{Changes, ItemWork, UncommittedFiles};
use crate::dashboard::snapshot::{Detail, InView};
use ratatui::crossterm::event::KeyCode;
use wf_core::event::Event;
use wf_core::id::{CommentId, HashId, ItemId, SessionId};
use wf_core::session::GitOid;
use wf_core::state::{State, Trust};
use wf_core::testing::{Fixture, blocker, comment, create, define_agent, resolving, start_session};

fn spans() -> ItemId {
    ItemId::from_bytes([2; 32])
}

/// The comment of `#spans` in the snapshot of [`with_links`], which shows work.
fn report() -> CommentId {
    CommentId::from_bytes([21; 32])
}

/// The app with `#spans` open, whose comment [`report`] shows work.
fn app_with_work() -> App {
    let mut app = app();
    app.open_item(spans());
    let mut snapshot = with_links();
    snapshot
        .links
        .with_work
        .insert((spans(), report().short()), report());
    let Some(InView::Item { id, text, session }) = snapshot.in_view.take() else {
        unreachable!("`with_links` has `#spans` in view")
    };
    let text = format!(
        "{text}\n  07:02  @ada/coder · {} drafted the notes\n",
        report().short()
    );
    snapshot.in_view = Some(InView::Item { id, text, session });
    app.refresh(snapshot);
    app
}

const UNCOMMITTED: &str = "\
diff --git a/notes.md b/notes.md
new file mode 100644
--- /dev/null
+++ b/notes.md
@@ -0,0 +1 @@
+draft
";

fn work() -> ItemWork {
    ItemWork {
        commit: work_commit(),
        changes: Changes::Files(wf_vcs::diff::parse(DIFF)),
        uncommitted: Some(UncommittedFiles {
            snapshot: GitOid::from_bytes([3; 20]),
            files: wf_vcs::diff::parse(UNCOMMITTED),
        }),
    }
}

/// Has the snapshot catch up with the work at `comment` of `#spans` on top.
fn in_view(app: &mut App, comment: CommentId) {
    app.refresh_detail(Some(InView::WorkAt {
        item: spans(),
        comment,
        title: format!(
            "work at comment {} by coder, as it was then",
            comment.short()
        ),
    }));
}

#[test]
fn w_picks_a_comment_that_shows_work_and_opens_the_work_at_it_read_only() {
    let mut app = app_with_work();
    assert_eq!(
        press(&mut app, KeyCode::Char('w')),
        Some(Effect::ChooseWorkAt { item: spans() })
    );
    let older = CommentId::from_bytes([22; 32]);
    app.ask(Prompt::Choose(Choice::WorkAt {
        item: spans(),
        comments: vec![
            (
                report(),
                format!("{}  @ada/coder  drafted the notes", report().short()),
            ),
            (older, format!("{}  @ada  Use humantime", older.short())),
        ],
        index: 0,
    }));
    insta::assert_snapshot!("choose_work_at", screen(&mut app, 60, 12));
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(press(&mut app, KeyCode::Enter), None);
    let opened = Detail::WorkAt {
        item: spans(),
        comment: older,
    };
    assert_eq!(app.wanted().detail, Some(opened.clone()));

    app.diff_on_top_mut().unwrap().read(Ok(work()));
    in_view(&mut app, older);
    let shown = screen(&mut app, 80, 30);
    assert!(
        shown.contains(&format!("work at comment {} by coder", older.short())),
        "{shown}"
    );
    assert!(shown.contains("━━ uncommitted, against"), "{shown}");
    assert!(shown.contains("notes.md (new) (uncommitted)"), "{shown}");
    // No comment on lines, review or pager here: the keys only move.
    for key in ['c', 'r', 'v', 'p', 'R'] {
        assert_eq!(press(&mut app, KeyCode::Char(key)), None, "{key}");
        assert!(app.prompt.is_none(), "{key}");
    }
    assert_eq!(app.wanted().detail, Some(opened));
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.wanted().detail, Some(Detail::Item(spans())));
}

#[test]
fn the_work_at_a_comment_that_cannot_be_read_says_why_in_place_of_a_diff() {
    let mut app = app_with_work();
    app.ask(Prompt::Choose(Choice::WorkAt {
        item: spans(),
        comments: vec![(report(), report().short())],
        index: 0,
    }));
    press(&mut app, KeyCode::Enter);
    app.diff_on_top_mut().unwrap().read(Err(
        "the session ran on desktop, which cannot be reached: timed out".to_owned(),
    ));
    in_view(&mut app, report());
    let shown = screen(&mut app, 80, 20);
    assert!(shown.contains("which cannot be reached"), "{shown}");
    assert!(!shown.contains("uncommitted"), "{shown}");
}

#[test]
fn the_id_of_a_comment_that_shows_work_links_to_the_work_at_it() {
    let mut app = app_with_work();
    let (column, row) = find(&mut app, 60, 24, &report().short());
    assert_eq!(app.click(column, row), None);
    assert_eq!(
        app.wanted().detail,
        Some(Detail::WorkAt {
            item: spans(),
            comment: report(),
        })
    );
}

/// A workforce with a work session on an item that reported, asked a blocker the principal
/// answered, and a remark of the principal; the events in the order written.
fn workforce() -> (State, Vec<Event>, ItemId, [CommentId; 4]) {
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let mut events = Vec::new();
    let mut apply = |state: &mut State, event: Event| {
        state.receive(event.clone(), Trust::Untrusted).unwrap();
        events.push(event.clone());
        CommentId::from(event.id())
    };
    let item: ItemId = fixture
        .apply(&mut state, 0, 1_000, create("task", "Draft the notes"))
        .id()
        .into();
    fixture.apply(&mut state, 0, 1_001, define_agent("coder"));
    let session: SessionId = fixture
        .apply(&mut state, 0, 1_002, start_session("coder", item))
        .id()
        .into();
    let agent = fixture.agent_actor("coder", session);
    let event = fixture.author_as(&state, 0, agent.clone(), 1_003, comment(item, "drafted"));
    let report = apply(&mut state, event);
    let event = fixture.author_as(
        &state,
        0,
        agent,
        1_004,
        blocker(item, "which crate?\nor none"),
    );
    let asked = apply(&mut state, event);
    let event = fixture.author(&state, 0, 1_005, resolving(item, "humantime", &[asked]));
    let answer = apply(&mut state, event);
    let event = fixture.author(&state, 0, 1_006, comment(item, "a remark"));
    let remark = apply(&mut state, event);
    (state, events, item, [report, asked, answer, remark])
}

#[test]
fn the_chooser_has_the_comments_that_show_work_the_newest_first_and_none_is_said() {
    let (state, events, item, [report, asked, answer, remark]) = workforce();
    let Choice::WorkAt {
        comments, index, ..
    } = act::comments_with_work(&state, &events, item).unwrap()
    else {
        panic!("the work at a comment");
    };
    let picked: Vec<CommentId> = comments.iter().map(|(comment, _)| *comment).collect();
    assert_eq!(picked, [answer, asked, report]);
    assert!(!picked.contains(&remark));
    assert_eq!(index, 0);
    // Who wrote it, its session included, and the first line of its text.
    let label = &comments[1].1;
    assert!(
        label.starts_with(&format!("{}  @ada/coder/", asked.short()))
            && label.ends_with("  which crate?"),
        "{label}"
    );

    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let item: ItemId = fixture
        .apply(&mut state, 0, 1_000, create("task", "Nothing yet"))
        .id()
        .into();
    fixture.apply(&mut state, 0, 1_001, comment(item, "a remark"));
    let error = act::comments_with_work(&state, &[], item).unwrap_err();
    assert!(error.to_string().contains("no comment of #"), "{error}");
}

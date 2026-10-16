//! What a session is told: its brief, what happened while it was parked, and what happened while
//! the daemon was down.

use std::sync::Arc;

use wf_core::brief::{Brief, Sources, last_comment, speaker};
use wf_core::event::EventBody;
use wf_core::id::{CommentId, EventId, HashId, ItemId, SessionId};
use wf_core::item::ItemView;
use wf_core::session::{CommitId, SessionRole};
use wf_core::state::State;
use wf_store::RedbStore;
use wf_vcs::{GitCli, VcsAdapter, Work, item_branch};

use super::{Run, blocking};
use crate::Loaded;

/// What the runner reads a brief from: the log of the workforce, and the diff of the commit under
/// review, which is read from git before the store is so that git does not hold the store up.
struct Logged<'a> {
    store: &'a RedbStore,
    diff: String,
}

impl Sources for Logged<'_> {
    fn text(&self, comment: CommentId) -> Option<String> {
        let EventBody::Commented(written) =
            wf_store::EventStore::event(self.store, &EventId::from(comment))
                .ok()??
                .content
                .body
        else {
            return None;
        };
        Some(written.text_with_lines())
    }

    fn branch(&self, item: ItemId) -> String {
        item_branch(item)
    }

    fn diff(&self, _target: &str, _commit: CommitId) -> String {
        self.diff.clone()
    }
}

/// The first prompt of a session: its brief. `Err` says why there is none; a review that cannot
/// show the change it reviews has none, so that nothing is approved unseen.
pub(super) async fn brief(loaded: &Arc<Loaded>, run: &Run) -> Result<String, String> {
    let diff = match run.role {
        SessionRole::Review {
            commit: Some(commit),
        } => {
            let repository = run.repository.clone();
            let work = Work::Unmerged {
                target: run.target.clone(),
                commit,
            };
            blocking(move || GitCli::open(&repository)?.diff(&work, &[]))
                .await
                .map_err(|error| wf_core::error_chain(&error))?
                .map_err(|error| {
                    format!(
                        "the change to review could not be read: {}",
                        wf_core::error_chain(&error)
                    )
                })?
        }
        SessionRole::Review { commit: None }
        | SessionRole::Work
        | SessionRole::Supervise
        | SessionRole::Diagnose => String::new(),
    };
    let session = run.session;
    loaded
        .storage
        .with(move |replica| {
            let sources = Logged {
                store: replica.store(),
                diff,
            };
            Brief::of(replica.state(), session, &sources)
                .map(|brief| brief.render())
                .unwrap_or_default()
        })
        .await
        .map_err(|_| "the workforce could not be read".to_string())
}

/// What to say when a parked session can go on: what others wrote on the item since the session's
/// last blocker, and what finished dependencies left.
pub(super) async fn resumed(loaded: &Arc<Loaded>, run: &Run) -> String {
    let (session, item) = (run.session, run.item);
    loaded
        .storage
        .with(move |replica| {
            let sources = Logged {
                store: replica.store(),
                diff: String::new(),
            };
            resumed_from(replica.state(), session, item, &sources)
        })
        .await
        .unwrap_or_default()
}

/// What to say to a session the daemon resumed after a restart: that it was, and what others
/// wrote on the item since the session was last prompted.
pub(super) async fn restarted(loaded: &Arc<Loaded>, run: &Run, prompted_ms: u64) -> String {
    let (session, item) = (run.session, run.item);
    loaded
        .storage
        .with(move |replica| {
            let sources = Logged {
                store: replica.store(),
                diff: String::new(),
            };
            restarted_from(replica.state(), session, item, prompted_ms, &sources)
        })
        .await
        .unwrap_or_default()
}

fn restarted_from(
    state: &State,
    session: SessionId,
    item: ItemId,
    prompted_ms: u64,
    sources: &Logged,
) -> String {
    let mut text = String::from(
        "wf restarted while you were working, and resumed this conversation. A command you ran \
         may not have finished; check before you go on.",
    );
    if let Some(ItemView::Live(item)) = state.item(item) {
        let mut since: Vec<_> = item
            .comments()
            .iter()
            .filter(|(_, comment)| {
                comment.at.wall_ms() > prompted_ms && comment.session() != Some(session)
            })
            .collect();
        since.sort_by_key(|(id, comment)| (comment.at, **id));
        if !since.is_empty() {
            text.push_str("\nWritten on the item since:");
        }
        for (id, comment) in since {
            if let Some(written) = sources.text(*id) {
                text.push_str(&format!(
                    "\n- {}: {}",
                    speaker(state, &comment.by),
                    written.trim()
                ));
            }
        }
    }
    text.push_str("\n`wf work brief` shows the item as it is now. Continue.");
    text
}

fn resumed_from(state: &State, session: SessionId, item: ItemId, sources: &Logged) -> String {
    let mut text = String::from("What you were waiting for is resolved.");
    if let Some(ItemView::Live(item)) = state.item(item) {
        let asked = item
            .comments()
            .values()
            .filter(|comment| comment.session() == Some(session))
            .filter(|comment| comment.kind == wf_core::event::CommentKind::Blocker)
            .map(|comment| comment.at)
            .max();
        if let Some(asked) = asked {
            let mut since: Vec<_> = item
                .comments()
                .iter()
                .filter(|(_, comment)| comment.at > asked && comment.session() != Some(session))
                .collect();
            since.sort_by_key(|(id, comment)| (comment.at, **id));
            for (id, comment) in since {
                if let Some(written) = sources.text(*id) {
                    text.push_str(&format!(
                        "\n- {}: {}",
                        speaker(state, &comment.by),
                        written.trim()
                    ));
                }
            }
        }
    }
    let graph = state.dependency_graph();
    for dependency in graph.depends_on(item) {
        let Some(ItemView::Live(dependency)) = state.item(dependency) else {
            continue;
        };
        if let Some(note) = last_comment(dependency, sources) {
            text.push_str(&format!(
                "\n- #{} {}: {note}",
                HashId::short(&dependency.id()),
                dependency.title()
            ));
        }
    }
    text.push_str("\nContinue.");
    text
}

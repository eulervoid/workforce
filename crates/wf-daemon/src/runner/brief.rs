//! What a session is told: its brief, and what happened while it was parked.

use std::sync::Arc;

use wf_core::brief::{Brief, Sources, last_comment, speaker};
use wf_core::event::EventBody;
use wf_core::id::{CommentId, EventId, HashId, ItemId, SessionId};
use wf_core::item::ItemView;
use wf_core::session::{CommitId, SessionRole};
use wf_core::state::State;
use wf_store::RedbStore;
use wf_vcs::{GitCli, VcsAdapter, item_branch};

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
        let EventBody::Comment(written) =
            wf_store::EventStore::event(self.store, &EventId::from(comment))
                .ok()??
                .content
                .body
        else {
            return None;
        };
        Some(written.text)
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
            let (repository, target) = (run.repository.clone(), run.target.clone());
            blocking(move || GitCli::open(&repository)?.diff(&target, commit, &[]))
                .await
                .map_err(|error| wf_core::report::report(&error))?
                .map_err(|error| {
                    format!(
                        "the change to review could not be read: {}",
                        wf_core::report::report(&error)
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
                    text.push_str(&format!("\n- {}: {}", speaker(&comment.by), written.trim()));
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

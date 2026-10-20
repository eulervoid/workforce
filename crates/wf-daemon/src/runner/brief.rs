//! What a session is told: its brief, what happened while it was parked, and what happened while
//! the daemon was down.

use std::sync::Arc;

use wf_core::brief::{Brief, Sources, restarted_text, resumed_text};
use wf_core::event::EventBody;
use wf_core::id::{CommentId, EventId, ItemId, SessionId};
use wf_core::session::{CommitId, SessionRole};
use wf_store::RedbStore;
use wf_vcs::{Work, diverged_ref, item_branch};

use super::{Run, blocking};
use crate::Loaded;

/// What the runner reads a brief from: the log of the workforce, and the diff of the commit under
/// review, which is read from git before the store is so that git does not hold the store up.
struct Logged<'a> {
    store: &'a RedbStore,
    diff: String,
    /// The refs of work set apart in the repository.
    apart: Vec<String>,
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

    fn diverged(&self, session: SessionId) -> Option<String> {
        let reference = diverged_ref(session);
        self.apart.contains(&reference).then_some(reference)
    }
}

/// The first prompt of a session: its brief. `Err` says why there is none; a review that cannot
/// show the change it reviews has none, so that nothing is approved unseen.
pub(super) async fn brief(loaded: &Arc<Loaded>, run: &Run) -> Result<String, String> {
    let diff = match &run.role {
        SessionRole::Review {
            commit: Some(commit),
            ..
        } => {
            let git = run.git.clone();
            let work = Work::Unmerged {
                target: run.target.clone(),
                commit: *commit,
            };
            blocking(move || git.diff(&work, &[]))
                .await
                .map_err(|error| wf_core::error_chain(&error))?
                .map_err(|error| {
                    format!(
                        "the change to review could not be read: {}",
                        wf_core::error_chain(&error)
                    )
                })?
        }
        SessionRole::Review { commit: None, .. }
        | SessionRole::Work
        | SessionRole::Supervise
        | SessionRole::Diagnose => String::new(),
    };
    let session = run.session;
    let git = run.git.clone();
    // A repository that cannot be listed has no work set apart to name.
    let apart = blocking(move || git.refs_apart())
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    loaded
        .storage
        .with(move |replica| {
            let sources = Logged {
                store: replica.store(),
                diff,
                apart,
            };
            Brief::of(replica.state(), session, &sources).map(|brief| brief.render())
        })
        .await
        .map_err(|_| "the workforce could not be read".to_string())?
        .ok_or_else(|| "the item is archived or the session is not known".to_string())
}

/// What to say when a parked session can go on: what others wrote on the item since the session's
/// last blocker, and what finished dependencies left.
pub(super) async fn resumed(loaded: &Arc<Loaded>, run: &Run) -> String {
    let session = run.session;
    loaded
        .storage
        .with(move |replica| {
            let sources = Logged {
                store: replica.store(),
                diff: String::new(),
                apart: Vec::new(),
            };
            resumed_text(replica.state(), session, &sources)
        })
        .await
        .unwrap_or_default()
}

/// What to say to a session the daemon resumed after a restart: that it was, and what others
/// wrote on the item since the session was last prompted.
pub(super) async fn restarted(loaded: &Arc<Loaded>, run: &Run, prompted_ms: u64) -> String {
    let session = run.session;
    loaded
        .storage
        .with(move |replica| {
            let sources = Logged {
                store: replica.store(),
                diff: String::new(),
                apart: Vec::new(),
            };
            restarted_text(replica.state(), session, prompted_ms, &sources)
        })
        .await
        .unwrap_or_default()
}

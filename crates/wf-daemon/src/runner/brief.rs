//! What a session is told: its brief, what happened while it was parked, and what happened while
//! the daemon was down.

use std::collections::BTreeSet;
use std::sync::Arc;

use wf_core::brief::{Brief, Sources, restarted_text, resumed_text, with_untold};
use wf_core::event::EventBody;
use wf_core::id::{CommentId, EventId, ItemId, SessionId};
use wf_core::session::{CommitId, SessionRole};
use wf_core::state::State;
use wf_core::told::ItemComment;
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

/// A prompt for a session, and the comments it tells.
pub(super) struct Prompt {
    pub(super) text: String,
    pub(super) tells: Vec<CommentId>,
}

impl Prompt {
    /// A prompt that tells no comment.
    pub(super) fn plain(text: String) -> Self {
        Prompt {
            text,
            tells: Vec::new(),
        }
    }
}

/// The first prompt of a session: its brief, which tells every comment written for the session so
/// far. `Err` says why there is none; a review that cannot show the change it reviews has none, so
/// that nothing is approved unseen.
pub(super) async fn brief(loaded: &Arc<Loaded>, run: &Run) -> Result<Prompt, String> {
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
            let state = replica.state();
            Brief::of(state, session, &sources).map(|brief| Prompt {
                text: brief.render(),
                tells: state
                    .written_for(session)
                    .into_iter()
                    .map(|written| written.comment)
                    .collect(),
            })
        })
        .await
        .map_err(|_| "the workforce could not be read".to_string())?
        .ok_or_else(|| "the item is archived or the session is not known".to_string())
}

/// What to say when a parked session can go on: what was written for it and not `told` yet, and
/// what finished dependencies left.
pub(super) async fn resumed(loaded: &Arc<Loaded>, run: &Run, told: &BTreeSet<CommentId>) -> Prompt {
    telling(loaded, run, told, |state, session, untold, sources| {
        resumed_text(state, session, untold, sources)
    })
    .await
    .unwrap_or_else(|| Prompt::plain(String::new()))
}

/// What to say to a session the daemon resumed after a restart: that it was, and what was written
/// for it and not `told` yet.
pub(super) async fn restarted(
    loaded: &Arc<Loaded>,
    run: &Run,
    told: &BTreeSet<CommentId>,
) -> Prompt {
    telling(loaded, run, told, |state, session, untold, sources| {
        restarted_text(state, session, untold, sources)
    })
    .await
    .unwrap_or_else(|| Prompt::plain(String::new()))
}

/// The prompt at the end of a turn, `prompt`, after what was written for the session and not
/// `told` yet.
pub(super) async fn after_turn(
    loaded: &Arc<Loaded>,
    run: &Run,
    told: &BTreeSet<CommentId>,
    prompt: String,
) -> Prompt {
    let fallback = prompt.clone();
    telling(loaded, run, told, move |state, session, untold, sources| {
        with_untold(state, session, untold, sources, prompt)
    })
    .await
    .unwrap_or_else(|| Prompt::plain(fallback))
}

/// The prompt `text` writes from the comments written for the session and not `told` yet; `None`
/// when the workforce cannot be read.
async fn telling(
    loaded: &Arc<Loaded>,
    run: &Run,
    told: &BTreeSet<CommentId>,
    text: impl FnOnce(&State, SessionId, &[ItemComment], &Logged<'_>) -> String + Send + 'static,
) -> Option<Prompt> {
    let session = run.session;
    let told = told.clone();
    loaded
        .storage
        .with(move |replica| {
            let sources = Logged {
                store: replica.store(),
                diff: String::new(),
                apart: Vec::new(),
            };
            let state = replica.state();
            let untold = state.untold(session, &told);
            Prompt {
                text: text(state, session, &untold, &sources),
                tells: untold.into_iter().map(|written| written.comment).collect(),
            }
        })
        .await
        .ok()
}

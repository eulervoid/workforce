//! `wf work`: what an agent in a work session runs, to read its brief and end the session.

use anyhow::anyhow;
use serde::Serialize;
use wf_core::brief::{Brief, Sources};
use wf_core::event::EventBody;
use wf_core::id::{CommentId, HashId, ItemId, SessionId};
use wf_core::lifecycle::Finish;
use wf_core::session::CommitId;

use super::{Done, emit};
use crate::cli::*;
use crate::workspace::Workspace;

pub(super) fn work(
    json: bool,
    command: WorkCommand,
    workspace: &mut Workspace,
) -> anyhow::Result<()> {
    let session = workspace.session().ok_or_else(|| {
        anyhow!(
            "`wf work` is for agents inside a session; {} is not set",
            wf_daemon::caller::SESSION_VARIABLE
        )
    })?;
    match command {
        WorkCommand::Brief => brief(json, workspace, session),
        WorkCommand::Submit { note } => end(
            json,
            workspace,
            session,
            Finish::Submit { note },
            "submitted",
        ),
        WorkCommand::Release { note } => end(
            json,
            workspace,
            session,
            Finish::Release { note },
            "released",
        ),
    }
}

#[derive(Serialize)]
struct BriefView {
    session: String,
    brief: String,
}

fn brief(json: bool, workspace: &Workspace, session: SessionId) -> anyhow::Result<()> {
    // The checkout of a session is a worktree of the repository, so git works from here.
    let git = match workspace.repository() {
        Ok(git) => Some(git),
        Err(_) => wf_vcs::GitCli::open(&std::env::current_dir()?).ok(),
    };
    let sources = Here { workspace, git };
    let brief = Brief::of(workspace.state(), session, &sources)
        .ok_or_else(|| anyhow!("session {} has no item to brief on", session.short()))?;
    let view = BriefView {
        session: session.to_string(),
        brief: brief.render(),
    };
    emit(json, &view, |view| view.brief.clone())
}

/// What `wf work brief` reads a brief from: the log, through the workspace, and git here.
struct Here<'a> {
    workspace: &'a Workspace,
    git: Option<wf_vcs::GitCli>,
}

impl Sources for Here<'_> {
    fn text(&self, comment: CommentId) -> Option<String> {
        let EventBody::Commented(written) =
            self.workspace.event(&comment.into()).ok()??.content.body
        else {
            return None;
        };
        Some(written.text_with_lines())
    }

    fn branch(&self, item: ItemId) -> String {
        wf_vcs::item_branch(item)
    }

    fn diff(&self, target: &str, commit: CommitId) -> String {
        self.git
            .as_ref()
            .and_then(|git| {
                let work = wf_vcs::Work::Unmerged {
                    target: target.to_owned(),
                    commit,
                };
                git.diff(&work, &[]).ok()
            })
            .unwrap_or_default()
    }
}

fn end(
    json: bool,
    workspace: &mut Workspace,
    session: SessionId,
    finish: Finish,
    verb: &str,
) -> anyhow::Result<()> {
    let item = workspace
        .state()
        .session(session)
        .expect("the session was resolved when the workspace was opened")
        .item();
    workspace.end_session(finish)?;
    emit(
        json,
        &Done {
            message: format!("{verb} #{}", item.short()),
            event: None,
            session: None,
            item: Some(item.to_base32()),
        },
        |done| format!("{}\n", done.message),
    )
}

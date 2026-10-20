//! `wf work`: what an agent in a work session runs, to read its brief and end the session.

use anyhow::{anyhow, bail};
use serde::Serialize;
use wf_core::brief::{Brief, Sources};
use wf_core::event::EventBody;
use wf_core::id::{CommentId, HashId, ItemId, SessionId};
use wf_core::lifecycle::Finish;
use wf_core::session::CommitId;

use super::{Done, emit};
use crate::cli::*;
use crate::workspace::Workspace;
use wf_daemon::proto::EndAnswer;

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
        WorkCommand::Submit { note } => {
            rebase_onto_target(workspace, session)?;
            end(
                json,
                workspace,
                session,
                Finish::Submit { note },
                "submitted",
            )
        }
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
    let git = workspace.repository().ok();
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

/// Rebases the branch in the agent's repository onto the tip of the target, for work that is
/// still being worked on: nobody has approved its commits yet, so rewriting them costs nothing,
/// and the reviewer sees them as they will land. A conflict, or uncommitted changes the rebase
/// would have to carry, stop the submission: the rebase stopped at the conflict waits in the
/// repository, and the next submission stages the files that are resolved and goes on with it.
fn rebase_onto_target(workspace: &mut Workspace, session: SessionId) -> anyhow::Result<()> {
    if !workspace.state().rebases_at_submit(session) {
        return Ok(());
    }
    let item = workspace
        .state()
        .session(session)
        .expect("the session was resolved when the workspace was opened")
        .item();
    let target = workspace
        .state()
        .policy()
        .map(|config| config.policy.merge.target.clone())
        .unwrap_or_default();
    let onto = workspace.target_commit()?;
    let directory =
        wf_vcs::item_repository(&workspace.data.workforce(&workspace.workforce()), item);
    match wf_vcs::GitCli::open(&directory)?.rebase(onto)? {
        wf_vcs::Rebase::UpToDate => Ok(()),
        wf_vcs::Rebase::Rebased => Ok(()),
        wf_vcs::Rebase::Conflict { paths } => bail!(
            "the submission rebases the branch onto `{target}`, and the rebase stopped at a commit \
             that conflicts in {}. Resolve the conflicts in these files, then submit again: \
             `wf work submit` stages them and goes on with the rebase, which may stop at a later \
             commit",
            paths.join(", ")
        ),
        wf_vcs::Rebase::DirtyCheckout(dirty) => bail!(
            "the submission rebases the branch onto `{target}`, but the repository has \
             uncommitted changes to {}; commit or discard them, then submit again",
            dirty.files()
        ),
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
    let message = match workspace.end_session(finish)? {
        EndAnswer::Ended => format!("{verb} #{}", item.short()),
        EndAnswer::Checking { checks } => format!(
            "Checking #{}: the checks ({}) run on the merge of the work in your sandbox. They \
             can take a while: end your turn now. You are prompted again with what failed, and \
             when they pass the work is submitted for review.",
            item.short(),
            checks
                .iter()
                .map(|check| check.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    emit(
        json,
        &Done {
            message,
            event: None,
            session: None,
            item: Some(item.to_base32()),
        },
        |done| format!("{}\n", done.message),
    )
}

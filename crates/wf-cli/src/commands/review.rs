//! `wf review`: verdicts on submitted work.

use anyhow::anyhow;
use wf_core::event::EventBody;
use wf_core::id::HashId;
use wf_core::lifecycle::Finish;
use wf_core::session::{SessionRole, Verdict};

use super::comment::comment_why;
use super::done;
use super::item::{Scope, choose_item};
use crate::cli::*;
use crate::interactive::Mode;
use crate::workspace::Workspace;

pub(crate) fn review(
    json: bool,
    mode: Mode,
    workspace: &mut Workspace,
    args: ReviewArgs,
) -> anyhow::Result<()> {
    let own_session = workspace.session().and_then(|id| {
        workspace
            .state()
            .session(id)
            .map(|session| (id, session.item(), session.role()))
    });
    let item = match (&args.item, own_session) {
        (Some(text), _) => workspace.resolve_item(text)?,
        (None, Some((_, item, _))) => item,
        (None, None) => choose_item(workspace, mode, None, Scope::Live)?,
    };
    let commit = match own_session {
        Some((_, own, SessionRole::Review { commit })) if own == item => commit,
        Some(_) | None => {
            workspace
                .state()
                .submission(item)
                .ok_or_else(|| anyhow!("#{} has no submitted work to review", item.short()))?
                .0
        }
    };
    let verdict = if args.approve {
        Verdict::Approve
    } else if args.request_changes {
        Verdict::RequestChanges
    } else {
        Verdict::Escalate
    };
    let because = comment_why(workspace, item, args.note)?;
    let event = workspace.author(EventBody::Reviewed(wf_core::event::Review {
        item,
        commit,
        verdict,
        because,
    }))?;
    if let Some((session, _, SessionRole::Review { .. })) = own_session {
        workspace.end_session(session, Finish::Reviewed)?;
    }
    let message = match verdict {
        Verdict::Approve => format!("approved #{}", item.short()),
        Verdict::RequestChanges => format!("requested changes on #{}", item.short()),
        Verdict::Escalate => format!("escalated #{} to the principal", item.short()),
    };
    done(json, message, &event, Some(item))
}

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
    let item = match (&args.item, &own_session) {
        (Some(text), _) => workspace.resolve_item(text)?,
        (None, Some((_, item, _))) => *item,
        (None, None) => choose_item(workspace, mode, None, Scope::Live)?,
    };
    let commit = match &own_session {
        Some((_, own, SessionRole::Review { commit, .. })) if *own == item => *commit,
        Some(_) | None => {
            workspace
                .state()
                .submission(item)
                .ok_or_else(|| anyhow!("#{} has no submitted work to review", item.short()))?
                .0
        }
    };
    let verdict = args.verdict();
    let because = comment_why(workspace, item, args.note)?;
    let round_limit_reached =
        workspace
            .state()
            .round_limit_reached(item, verdict, workspace.actor());
    let event = workspace.author(EventBody::Reviewed(wf_core::event::Reviewed {
        item,
        commit,
        verdict,
        because,
        round_limit_reached,
    }))?;
    if let Some((_, _, SessionRole::Review { .. })) = own_session {
        workspace.end_session(Finish::Reviewed)?;
    }
    let message = match verdict {
        Verdict::Approve => format!("approved #{}", item.short()),
        Verdict::RequestChanges => match round_limit_reached {
            None => format!("requested changes on #{}", item.short()),
            Some(rounds) => format!(
                "requested changes on #{} after {}, the round limit of its review level: it waits for the principal",
                item.short(),
                wf_core::inbox::round_count(rounds)
            ),
        },
        Verdict::Escalate => format!("escalated #{} to the principal", item.short()),
    };
    done(json, message, &event, Some(item))
}

impl ReviewArgs {
    /// The verdict flag that was given; clap requires exactly one.
    fn verdict(&self) -> Verdict {
        match (self.approve, self.request_changes) {
            (true, _) => Verdict::Approve,
            (false, true) => Verdict::RequestChanges,
            (false, false) => Verdict::Escalate,
        }
    }
}

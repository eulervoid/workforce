//! `wf review`: verdicts on submitted work.

use anyhow::anyhow;
use wf_core::event::{Commented, Event, EventBody, Reviewed};
use wf_core::id::{CommentId, HashId, ItemId};
use wf_core::lifecycle::Finish;
use wf_core::session::{CommitId, SessionRole, Verdict};

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
    let VerdictWritten {
        event,
        round_limit_reached,
        overruled,
    } = write_verdict(workspace, item, commit, verdict, because)?;
    if let Some((_, _, SessionRole::Review { .. })) = own_session {
        workspace.end_session(Finish::Reviewed)?;
    }
    let message = match verdict {
        Verdict::Approve => format!("approved #{}{}", item.short(), resolving(overruled)),
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

/// What [`write_verdict`] wrote.
pub(crate) struct VerdictWritten {
    pub event: Event,
    /// What the verdict records as its round limit.
    pub round_limit_reached: Option<u32>,
    /// How many change requests the verdict overruled and resolved.
    pub overruled: usize,
}

/// Writes a verdict on `commit` of `item`, recording the round limit as wf-core decides it, and
/// then resolves the change requests it overrules, so that they do not hold up a later
/// submission if the merge sends the work back.
pub(crate) fn write_verdict(
    workspace: &mut Workspace,
    item: ItemId,
    commit: Option<CommitId>,
    verdict: Verdict,
    because: Option<CommentId>,
) -> anyhow::Result<VerdictWritten> {
    let state = workspace.state();
    let round_limit_reached = state.round_limit_reached(item, verdict, workspace.actor());
    let overruled = state.overruled_change_requests(item, verdict, workspace.actor());
    let event = workspace.author(EventBody::Reviewed(Reviewed {
        item,
        commit,
        verdict,
        because,
        round_limit_reached,
    }))?;
    if !overruled.is_empty() {
        workspace.author(EventBody::Commented(Commented {
            resolves: overruled.clone(),
            ..Commented::plain(item, "Overruled: approved at the round limit.")
        }))?;
    }
    Ok(VerdictWritten {
        event,
        round_limit_reached,
        overruled: overruled.len(),
    })
}

/// `, resolving the 2 change requests it overruled`, or nothing for none.
pub(crate) fn resolving(overruled: usize) -> String {
    match overruled {
        0 => String::new(),
        1 => ", resolving the change request it overruled".to_owned(),
        count => format!(", resolving the {count} change requests it overruled"),
    }
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

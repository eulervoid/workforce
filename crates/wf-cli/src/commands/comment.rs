//! `wf comment` and `wf mentions`.

use anyhow::{anyhow, bail};
use wf_core::event::{Change, CommentKind, Commented, EventBody};
use wf_core::id::{CommentId, HashId, ItemId};
use wf_core::item::{Item, ItemEntry};
use wf_core::lifecycle::Finish;
use wf_core::refs::{self, Mention, Token};
use wf_core::session::SessionRole;

use super::item::{Scope, choose_item};
use super::{done, edit_in_editor, emit};
use crate::cli::*;
use crate::interactive::Mode;
use crate::render;
use crate::view::{self, MentionsView};
use crate::workspace::Workspace;

pub(super) fn comment(
    json: bool,
    mode: Mode,
    workspace: &mut Workspace,
    args: CommentArgs,
) -> anyhow::Result<()> {
    let id = choose_item(workspace, mode, args.item.clone(), Scope::Any)?;
    let resolves = comments_to_resolve(workspace, id, &args)?;
    let mut text = args.text.join(" ");
    if args.edit || (text.is_empty() && resolves.is_empty() && mode == Mode::Interactive) {
        text = edit_in_editor(&text, ".md")?;
    }
    let text = text.trim();
    if text.is_empty() && resolves.is_empty() {
        bail!("the comment is empty; nothing was sent");
    }
    let kind = args.kind();
    let step = args
        .step
        .map(|number| step_numbered(workspace, id, number))
        .transpose()?;
    let event = workspace.author(EventBody::Commented(Commented {
        item: id,
        text: text.to_string(),
        kind,
        resolves: resolves.clone(),
        step,
        lines: None,
    }))?;
    if let (Some(step), true) = (step, args.done) {
        workspace.author(EventBody::ItemEdited(wf_core::event::ItemEdited {
            item: id,
            changes: vec![Change::StepDone { step, done: true }],
        }))?;
    }
    let comment = CommentId::from(event.id());
    let mut message = format!(
        "wrote {} {} on #{}",
        kind.label(),
        comment.short(),
        id.short()
    );
    if !resolves.is_empty() {
        let ids: Vec<String> = resolves.iter().map(HashId::short).collect();
        message.push_str(&format!(", resolving {}", ids.join(", ")));
    }
    match (args.step, args.done) {
        (Some(number), true) => message.push_str(&format!(", and checked step {number}")),
        (Some(number), false) => message.push_str(&format!(", on step {number}")),
        (None, _) => {}
    }
    // A supervise session is over once it has answered or escalated.
    let supervising = workspace.session().filter(|session| {
        workspace
            .state()
            .session(*session)
            .is_some_and(|session| session.item() == id && session.role() == SessionRole::Supervise)
    });
    if let (Some(_), false) = (supervising, resolves.is_empty()) {
        workspace.end_session(Finish::Supervised)?;
    }
    // A diagnose session is over once it has said what went wrong.
    let diagnosing = workspace.session().filter(|session| {
        workspace
            .state()
            .session(*session)
            .is_some_and(|session| session.item() == id && session.role() == SessionRole::Diagnose)
    });
    if diagnosing.is_some() {
        workspace.end_session(Finish::Diagnosed)?;
    }
    done(json, message, &event, Some(id))
}

/// The id of a step of the item's body, numbered as `wf item show` prints them.
fn step_numbered(
    workspace: &Workspace,
    item: ItemId,
    number: std::num::NonZeroUsize,
) -> anyhow::Result<wf_core::body::StepId> {
    let state = workspace.state();
    let Some(ItemEntry::Live(live)) = state.item(item) else {
        bail!("#{} is archived", item.short());
    };
    let steps: Vec<_> = live.steps().collect();
    match steps.get(number.get() - 1) {
        Some(step) => Ok(step.id),
        None if steps.is_empty() => bail!("#{} has no steps", item.short()),
        None => bail!(
            "#{} has {} steps, there is no step {number}",
            item.short(),
            steps.len()
        ),
    }
}

/// Writes why a move or verdict is made as a plain comment on the item, for the move or verdict
/// to name.
pub(super) fn comment_why(
    workspace: &mut Workspace,
    item: ItemId,
    why: Option<String>,
) -> anyhow::Result<Option<CommentId>> {
    let Some(why) = why.filter(|why| !why.trim().is_empty()) else {
        return Ok(None);
    };
    let written = workspace.author(EventBody::Commented(Commented::plain(item, why.trim())))?;
    Ok(Some(CommentId::from(written.id())))
}

/// The blockers and change requests a comment resolves: those named, and every open one of the
/// kinds asked for, as they are open right now. The event names them, so one written meanwhile
/// on another device stays open.
fn comments_to_resolve(
    workspace: &Workspace,
    item: ItemId,
    args: &CommentArgs,
) -> anyhow::Result<Vec<CommentId>> {
    let state = workspace.state();
    let Some(ItemEntry::Live(live)) = state.item(item) else {
        if args.resolve.is_empty() && !args.resolve_blockers && !args.resolve_change_requests {
            return Ok(Vec::new());
        }
        bail!(
            "#{} is archived; its comments cannot be resolved",
            item.short()
        );
    };
    let mut resolves = Vec::new();
    for prefix in &args.resolve {
        let comments = live.comments().keys().copied();
        resolves.push(wf_core::id::resolve_prefix(prefix, comments)?);
    }
    for (wanted, flag) in [
        (CommentKind::Blocker, args.resolve_blockers),
        (CommentKind::ChangeRequest, args.resolve_change_requests),
    ] {
        if !flag {
            continue;
        }
        let open = open_of_kind(live, wanted);
        if open.is_empty() {
            bail!("#{} has no open {}s", item.short(), wanted.label());
        }
        resolves.extend(open);
    }
    resolves.sort();
    resolves.dedup();
    Ok(resolves)
}

/// The open comments of the kind asked for. A check failure is a change request.
fn open_of_kind(item: &Item, wanted: CommentKind) -> Vec<CommentId> {
    item.open_comments()
        .into_iter()
        .filter(|(_, comment)| {
            comment.kind == wanted
                || (wanted.is_change_request() && comment.kind.is_change_request())
        })
        .map(|(id, _)| id)
        .collect()
}

pub(super) fn mentions(json: bool, workspace: &Workspace, target: &str) -> anyhow::Result<()> {
    let state = workspace.state();
    let mention = match refs::parse(target).into_iter().next() {
        Some(Token::Mention(mention)) => mention,
        Some(Token::ItemRef(_) | Token::CommentRef { .. }) | None => {
            bail!("`{target}` is not a mention; write it like @ada or @ada/coder")
        }
    };
    let resolved: Option<_> = state.resolver().mention(&mention);
    let resolved = resolved.ok_or_else(|| {
        anyhow!(
            "{} does not name anyone in this workforce",
            Mention::to_string(&mention)
        )
    })?;
    let sources = state.mentions_of(&resolved);
    let mentions = MentionsView {
        target: view::mention_label(state, &resolved),
        sources: sources
            .iter()
            .map(|source| view::source_view(state, source))
            .collect(),
    };
    emit(json, &mentions, render::mentions)
}

impl CommentArgs {
    /// `--block` and `--request-change` exclude each other.
    fn kind(&self) -> CommentKind {
        match (self.block, self.request_change) {
            (true, _) => CommentKind::Blocker,
            (false, true) => CommentKind::ChangeRequest,
            (false, false) => CommentKind::Plain,
        }
    }
}

#[cfg(test)]
mod tests {
    use wf_core::testing::*;

    use super::*;

    #[test]
    fn resolving_change_requests_takes_check_failures_too_and_no_blockers() {
        let fixture = Fixture::new(1);
        let mut state = fixture.state();
        let task: ItemId = fixture
            .apply(&mut state, 0, 2_000, create("task", "work"))
            .id()
            .into();
        let change = fixture.apply(&mut state, 0, 2_001, change_request(task, "rename"));
        let failure = fixture.apply(&mut state, 0, 2_002, check_failure(task, "cargo test"));
        fixture.apply(&mut state, 0, 2_003, blocker(task, "which one?"));
        let Some(ItemEntry::Live(item)) = state.item(task) else {
            unreachable!("the item is live")
        };
        let mut expected = vec![CommentId::from(change.id()), CommentId::from(failure.id())];
        expected.sort();
        let mut open = open_of_kind(item, CommentKind::ChangeRequest);
        open.sort();
        assert_eq!(open, expected);
        assert_eq!(open_of_kind(item, CommentKind::Blocker).len(), 1);
    }
}

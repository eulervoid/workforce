//! Carrying out what the keys of the dashboard ask of the workforce: the same events the commands
//! write, without their output, which would draw over the dashboard.

use anyhow::{anyhow, bail};
use ratatui::DefaultTerminal;
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{EnterAlternateScreen, enable_raw_mode};
use std::collections::BTreeSet;

use tui_input::Input;
use wf_core::event::{Change, CommentKind, Commented, EventBody, ItemEdited, ItemMoved};
use wf_core::id::{CommentId, EventId, HashId, ItemId, SessionId};
use wf_core::item::{Item, ItemEntry, ItemState, Priority};
use wf_core::name::{AgentName, Label, LaneName, RecoveryLevelName, Requirement, ReviewLevelName};
use wf_core::policy::{Field as PolicyField, Policy, TypeConfig};
use wf_core::session::{CommitId, SessionRole, Verdict};
use wf_core::state::State;
use wf_harness::Permission;

use super::app::{
    Choice, Effect, Field, Inherited, Member, Prompt, Request, Set, SetInput, Starter, Written,
};
use super::diff::LineComment;
use crate::commands::{edit_in_editor, edit_until_valid, item_work, resolving, write_verdict};
use crate::draft::Draft;
use crate::view::TargetFollowed;
use crate::workspace::{Movable, Workspace};

/// What is written above this line in `$EDITOR` counts; what is below is there to read.
const SCISSORS: &str = "--- wf: everything below this line is left out ---";

/// What carrying out an effect came to.
pub enum Outcome {
    /// The loop ends.
    Quit,
    /// What it did, to say so.
    Did(String),
    /// Something to pick or answer before it can be done.
    Ask(Prompt),
    /// A comment on lines written in `$EDITOR`, a draft of the review of `item` in place of the
    /// draft `replacing`.
    Drafted {
        item: ItemId,
        comment: LineComment,
        replacing: Option<usize>,
    },
    /// The first `posted` drafts of the review of `item` were posted, then it failed with `error`.
    ReviewFailed {
        item: ItemId,
        posted: usize,
        error: anyhow::Error,
    },
    /// The review of `item` was sent with the comments on lines drafted for it, as `what` says.
    Reviewed { item: ItemId, what: String },
}

/// Carries out `effect`.
pub fn carry_out(
    effect: Effect,
    workspace: &mut Workspace,
    terminal: &mut DefaultTerminal,
) -> anyhow::Result<Outcome> {
    match effect {
        Effect::Quit => Ok(Outcome::Quit),
        Effect::ChooseStart { item } => {
            Ok(Outcome::Ask(Prompt::Choose(starters(workspace, item)?)))
        }
        Effect::ChooseField { item } => Ok(Outcome::Ask(Prompt::Choose(fields(
            workspace.state(),
            item,
        )?))),
        Effect::ChooseWorkAt { item } => {
            let events = workspace.item_events(&[item])?;
            Ok(Outcome::Ask(Prompt::Choose(comments_with_work(
                workspace.state(),
                &events,
                item,
            )?)))
        }
        Effect::Edit { item, field } => edit(workspace, terminal, item, field),
        Effect::Change { item, changes } => change(workspace, item, changes).map(Outcome::Did),
        Effect::Approve {
            item,
            reviewed,
            comments,
        } => {
            let commit = submitted(workspace.state(), item, reviewed)?;
            let mut sink = WorkspaceSink {
                workspace,
                item,
                commit,
                overruled: 0,
            };
            let sent = send_review(&mut sink, &comments, &[], Verdict::Approve);
            reviewed_or_failed(sent, item, || {
                format!(
                    "approved #{}{}{}",
                    item.short(),
                    with(&comments),
                    resolving(sink.overruled)
                )
            })
        }
        Effect::RequestChanges {
            item,
            reviewed,
            comments,
        } => request_changes(workspace, terminal, item, reviewed, &comments),
        Effect::LineCommentInEditor {
            item,
            mut comment,
            then,
        } => {
            let quote: String = comment
                .lines
                .quote
                .lines()
                .map(|line| format!("    {line}\n"))
                .collect();
            let template = format!(
                "{}\n{SCISSORS}\nYour {} on {} of #{}:\n\n{quote}",
                comment.text,
                comment.kind.label(),
                comment.lines.location(),
                item.short()
            );
            let text = suspended(terminal, || edit_in_editor(&template, ".md"))??;
            comment.text = above_scissors(&text);
            if comment.text.is_empty() {
                return Ok(Outcome::Did("nothing written".to_owned()));
            }
            match then {
                Written::Post => post_line_comment(workspace, item, comment).map(Outcome::Did),
                Written::Draft { replacing } => Ok(Outcome::Drafted {
                    item,
                    comment,
                    replacing,
                }),
            }
        }
        Effect::PostLineComment { item, comment } => {
            post_line_comment(workspace, item, comment).map(Outcome::Did)
        }
        Effect::StartSession { item, agent } => {
            start_agent(workspace, item, agent).map(Outcome::Did)
        }
        Effect::StopSession { session } => stop_session(workspace, session).map(Outcome::Did),
        Effect::Move { item, state } => move_item(workspace, item, state).map(Outcome::Did),
        Effect::Comment { item, text } => comment(workspace, item, text).map(Outcome::Did),
        Effect::CommentInEditor { item, text } => {
            comment_in_editor(workspace, terminal, item, text).map(Outcome::Did)
        }
        Effect::Permit { request, answer } => permit(workspace, request, answer).map(Outcome::Did),
        Effect::Resolve {
            item,
            blocker,
            text,
        } => resolve(workspace, item, blocker, text).map(Outcome::Did),
        Effect::ResolveInEditor {
            item,
            blocker,
            text,
        } => resolve_in_editor(workspace, terminal, item, blocker, text).map(Outcome::Did),
        Effect::PageDiff { item } => page_diff(workspace, terminal, item).map(Outcome::Did),
    }
}

/// The agents of the principal of this machine, the policy's default agent first. A draft can be
/// released for the dispatcher too, which comes first. An item that a live session holds is
/// refused right away, before a starter is picked for nothing, unless the daemon could take it
/// over: whether its device is gone, the daemon answers.
fn starters(workspace: &Workspace, item: ItemId) -> anyhow::Result<Choice> {
    let state = workspace.state();
    state.ensure_takeable(item, &SessionRole::Work, workspace.device_key())?;
    let draft = matches!(
        state.item(item),
        Some(ItemEntry::Live(live)) if live.state() == ItemState::Draft
    );
    let principal = workspace.principal();
    let default = state
        .policy()
        .and_then(|stored| stored.policy.default_agent.clone());
    let mut agents: Vec<AgentName> = state
        .agents()
        .filter(|(id, _)| id.principal == principal)
        .map(|(id, _)| id.name.clone())
        .collect();
    agents.sort_by_key(|agent| Some(agent) != default.as_ref());
    if agents.is_empty() && !draft {
        bail!("you have no agents; add one with `wf agent add`");
    }
    let starters = draft
        .then_some(Starter::Dispatcher)
        .into_iter()
        .chain(agents.into_iter().map(Starter::Agent))
        .collect();
    Ok(Choice::Start {
        item,
        starters,
        index: 0,
    })
}

/// The states a principal moves an item to, that its type uses, less the one it is in. `active`
/// and `review` are the runner's, and `open` waits until no work session holds the item.
fn states(state: &State, item: ItemId) -> anyhow::Result<Choice> {
    let live = live(state, item)?;
    let config = type_config(state, live)?;
    let current = live.state();
    let held = state.holder(item, &SessionRole::Work).is_some();
    let states: Vec<ItemState> = [
        ItemState::Draft,
        ItemState::Open,
        ItemState::Done,
        ItemState::Cancelled,
    ]
    .into_iter()
    .filter(|state| config.uses(*state) && *state != current)
    .filter(|state| !(held && *state == ItemState::Open))
    .collect();
    if states.is_empty() {
        bail!("a {} that is {current} cannot be moved", live.item_type());
    }
    Ok(Choice::State {
        item,
        states,
        index: 0,
    })
}

/// The item `item`, while it is live.
/// The comments of `item` that show the work at a snapshot ([`State::comment_snapshot`]), the
/// newest first, each with who wrote it and the first line of its text, read from `events`.
pub(super) fn comments_with_work(
    state: &State,
    events: &[wf_core::event::Event],
    item: ItemId,
) -> anyhow::Result<Choice> {
    let live = live(state, item)?;
    let first_line = |comment: CommentId| {
        events
            .iter()
            .find(|event| CommentId::from(event.id()) == comment)
            .and_then(|event| {
                let EventBody::Commented(commented) = &event.content.body else {
                    return None;
                };
                commented.text.lines().next()
            })
            .unwrap_or_default()
            .to_owned()
    };
    let mut shown: Vec<(&CommentId, &wf_core::item::Comment)> = live
        .comments()
        .iter()
        .filter(|(comment, _)| state.comment_snapshot(item, **comment).is_some())
        .collect();
    shown.sort_by_key(|(comment, folded)| std::cmp::Reverse((folded.at, **comment)));
    if shown.is_empty() {
        bail!(
            "no comment of #{} shows work: only those by its work sessions, and answers to \
             their blockers, do",
            item.short()
        );
    }
    let comments = shown
        .into_iter()
        .map(|(comment, folded)| {
            let label = format!(
                "{}  {}  {}",
                comment.short(),
                state.actor_label(&folded.by),
                first_line(*comment)
            );
            (*comment, label)
        })
        .collect();
    Ok(Choice::WorkAt {
        item,
        comments,
        index: 0,
    })
}

fn live(state: &State, item: ItemId) -> anyhow::Result<&Item> {
    match state
        .item(item)
        .ok_or_else(|| anyhow!("#{} is gone", item.short()))?
    {
        ItemEntry::Live(live) => Ok(live),
        ItemEntry::Archived(_) => bail!("#{} is archived", item.short()),
    }
}

fn policy(state: &State) -> &Policy {
    &state.policy().expect("a workforce has a policy").policy
}

fn type_config<'a>(state: &'a State, live: &Item) -> anyhow::Result<&'a TypeConfig> {
    policy(state)
        .type_config(live.item_type())
        .ok_or_else(|| anyhow!("no item type `{}` in the policy", live.item_type()))
}

/// Refuses a field the type of `live` does not have.
fn ensure_allowed(state: &State, live: &Item, field: PolicyField) -> anyhow::Result<()> {
    if !type_config(state, live)?.allows_field(field) {
        bail!("a {} has no {}", live.item_type(), policy_field_name(field));
    }
    Ok(())
}

fn policy_field_name(field: PolicyField) -> &'static str {
    match field {
        PolicyField::Priority => "priority",
        PolicyField::Labels => "labels",
        PolicyField::Requires => "requirements",
    }
}

/// The edit menu of `item`: each field with what it is now.
pub(super) fn fields(state: &State, item: ItemId) -> anyhow::Result<Choice> {
    let live = live(state, item)?;
    let config = type_config(state, live)?;
    let of_type = |level: Option<String>| {
        Inherited {
            item_type: live.item_type().clone(),
            level,
        }
        .describe()
    };
    let listed = |values: Vec<String>| {
        if values.is_empty() {
            "—".to_owned()
        } else {
            values.join(", ")
        }
    };
    let unless_allowed = |field: PolicyField, value: String| {
        if config.allows_field(field) {
            value
        } else {
            format!("a {} has none", live.item_type())
        }
    };
    let value = |field: Field| match field {
        Field::Text => "in $EDITOR".to_owned(),
        Field::State => live.state().to_string(),
        Field::Priority => unless_allowed(
            PolicyField::Priority,
            live.priority()
                .map_or_else(|| "—".to_owned(), |priority| priority.to_string()),
        ),
        Field::Lane => live.lane().map_or_else(
            || of_type(Some(config.lane.to_string())),
            ToString::to_string,
        ),
        Field::Review => live.review_level().map_or_else(
            || of_type(config.review.as_ref().map(ToString::to_string)),
            ToString::to_string,
        ),
        Field::Recovery => live.recovery_level().map_or_else(
            || of_type(config.recovery.as_ref().map(ToString::to_string)),
            ToString::to_string,
        ),
        Field::Labels => unless_allowed(
            PolicyField::Labels,
            listed(live.labels().values().map(ToString::to_string).collect()),
        ),
        Field::Requires => unless_allowed(
            PolicyField::Requires,
            listed(live.requires().values().map(ToString::to_string).collect()),
        ),
    };
    Ok(Choice::Field {
        item,
        fields: Field::ALL
            .into_iter()
            .map(|field| (field, value(field)))
            .collect(),
        index: 0,
    })
}

/// Asks for the new value of `field` of `item`, or for the title and body, edits them in
/// `$EDITOR`.
fn edit(
    workspace: &mut Workspace,
    terminal: &mut DefaultTerminal,
    item: ItemId,
    field: Field,
) -> anyhow::Result<Outcome> {
    match asked(workspace.state(), item, field)? {
        Some(prompt) => Ok(Outcome::Ask(prompt)),
        None => edit_item(workspace, terminal, item).map(Outcome::Did),
    }
}

/// What asks for the new value of `field` of `item`: a choice, or the set input. `None` for the
/// title and body, which are written in `$EDITOR`.
pub(super) fn asked(state: &State, item: ItemId, field: Field) -> anyhow::Result<Option<Prompt>> {
    Ok(Some(match field {
        Field::Text => return Ok(None),
        Field::State => Prompt::Choose(states(state, item)?),
        Field::Priority => Prompt::Choose(priorities(state, item)?),
        Field::Lane => Prompt::Choose(lanes(state, item)?),
        Field::Review => Prompt::Choose(review_levels(state, item)?),
        Field::Recovery => Prompt::Choose(recovery_levels(state, item)?),
        Field::Labels => Prompt::Set(labels(state, item)?),
        Field::Requires => Prompt::Set(requirements(state, item)?),
    }))
}

/// The priorities, and none, starting on the item's.
fn priorities(state: &State, item: ItemId) -> anyhow::Result<Choice> {
    let live = live(state, item)?;
    ensure_allowed(state, live, PolicyField::Priority)?;
    let priorities = vec![
        Some(Priority::P0),
        Some(Priority::P1),
        Some(Priority::P2),
        Some(Priority::P3),
        None,
    ];
    let index = position(&priorities, &live.priority());
    Ok(Choice::Priority {
        item,
        priorities,
        index,
    })
}

/// The lanes of the policy, starting on the item's.
fn lanes(state: &State, item: ItemId) -> anyhow::Result<Choice> {
    let live = live(state, item)?;
    let policy = policy(state);
    let lanes: Vec<LaneName> = policy.lanes.keys().cloned().collect();
    let index = live
        .effective_lane(policy)
        .map_or(0, |lane| position(&lanes, lane));
    Ok(Choice::Lane { item, lanes, index })
}

/// The type's review level, then those of the policy, starting on the item's.
fn review_levels(state: &State, item: ItemId) -> anyhow::Result<Choice> {
    let live = live(state, item)?;
    let config = type_config(state, live)?;
    let levels: Vec<Option<ReviewLevelName>> = std::iter::once(None)
        .chain(
            policy(state)
                .review
                .iter()
                .map(|level| Some(level.name.clone())),
        )
        .collect();
    Ok(Choice::Review {
        item,
        index: position(&levels, &live.review_level().cloned()),
        levels,
        of_type: Inherited {
            item_type: live.item_type().clone(),
            level: config.review.clone(),
        },
    })
}

/// The type's recovery level, then those of the policy, starting on the item's.
fn recovery_levels(state: &State, item: ItemId) -> anyhow::Result<Choice> {
    let live = live(state, item)?;
    let config = type_config(state, live)?;
    let levels: Vec<Option<RecoveryLevelName>> = std::iter::once(None)
        .chain(
            policy(state)
                .recovery
                .iter()
                .map(|level| Some(level.name.clone())),
        )
        .collect();
    Ok(Choice::Recovery {
        item,
        index: position(&levels, &live.recovery_level().cloned()),
        levels,
        of_type: Inherited {
            item_type: live.item_type().clone(),
            level: config.recovery.clone(),
        },
    })
}

/// Where `value` is among `options`; the first when it is not.
fn position<T: PartialEq>(options: &[T], value: &T) -> usize {
    options
        .iter()
        .position(|option| option == value)
        .unwrap_or_default()
}

/// The labels of the live items, those of `item` marked.
fn labels(state: &State, item: ItemId) -> anyhow::Result<SetInput> {
    let live = live(state, item)?;
    ensure_allowed(state, live, PolicyField::Labels)?;
    let known: BTreeSet<&Label> = state
        .live_items()
        .flat_map(|item| item.labels().values())
        .collect();
    let members = known
        .into_iter()
        .map(|label| {
            member(
                label.to_string(),
                live.labels()
                    .contains(label)
                    .then(|| live.labels().observed(label)),
            )
        })
        .collect();
    Ok(set_input(item, Set::Labels, members))
}

/// The capabilities of the agents and the requirements of the live items, those of `item`
/// marked; with what the agents of its lane offer, on the devices of their principal.
fn requirements(state: &State, item: ItemId) -> anyhow::Result<SetInput> {
    let live = live(state, item)?;
    ensure_allowed(state, live, PolicyField::Requires)?;
    let lane = live.effective_lane(policy(state));
    let offered: BTreeSet<Requirement> = state
        .agents()
        .filter(|(_, agent)| lane.is_some_and(|lane| agent.lanes.contains(lane)))
        .flat_map(|(id, agent)| {
            let hardware = state
                .devices()
                .filter(|device| device.principal == id.principal)
                .filter_map(|device| device.profile)
                .flat_map(|profile| profile.hardware_requirements());
            agent.capabilities.iter().cloned().chain(hardware)
        })
        .collect();
    let known: BTreeSet<&Requirement> = state
        .agents()
        .flat_map(|(_, agent)| agent.capabilities.iter())
        .chain(state.live_items().flat_map(|item| item.requires().values()))
        .collect();
    let members = known
        .into_iter()
        .map(|requirement| {
            member(
                requirement.to_string(),
                live.requires()
                    .contains(requirement)
                    .then(|| live.requires().observed(requirement)),
            )
        })
        .collect();
    Ok(set_input(item, Set::Requires { offered }, members))
}

fn member(value: String, held: Option<Vec<EventId>>) -> Member {
    Member {
        value,
        marked: held.is_some(),
        held,
    }
}

fn set_input(item: ItemId, set: Set, members: Vec<Member>) -> SetInput {
    SetInput {
        item,
        set,
        members,
        search: Input::default(),
        index: 0,
    }
}

/// Writes `changes` to `item` as one edit.
fn change(workspace: &mut Workspace, item: ItemId, changes: Vec<Change>) -> anyhow::Result<String> {
    workspace.author(EventBody::ItemEdited(ItemEdited { item, changes }))?;
    Ok(format!("edited #{}", item.short()))
}

/// Starts a session of `agent` on `item`.
fn start_agent(
    workspace: &mut Workspace,
    item: ItemId,
    agent: AgentName,
) -> anyhow::Result<String> {
    let pushed = push_target(workspace);
    let session = workspace.start_session(agent.clone(), item, None)?;
    Ok(format!(
        "started {agent} on #{} ({}){pushed}",
        item.short(),
        session.short()
    ))
}

/// Pushes the principal's commits on the target to wf's repository before an agent may start
/// from it, and says so after what the action did: `; main: pushed 2 commits to wf`. The header
/// goes on warning of commits that could not be pushed.
fn push_target(workspace: &Workspace) -> String {
    match workspace.follow_target(Movable::Both) {
        Some(TargetFollowed::Moved(moved)) => format!("; {moved}"),
        Some(TargetFollowed::Apart(_)) | None => String::new(),
    }
}

fn stop_session(workspace: &mut Workspace, session: SessionId) -> anyhow::Result<String> {
    workspace.stop_session(session)?;
    Ok(format!("stopped {}", session.short()))
}

fn move_item(workspace: &mut Workspace, item: ItemId, state: ItemState) -> anyhow::Result<String> {
    let pushed = match state {
        ItemState::Open => push_target(workspace),
        ItemState::Draft
        | ItemState::Active
        | ItemState::Review
        | ItemState::Done
        | ItemState::Cancelled => String::new(),
    };
    workspace.author(EventBody::ItemMoved(ItemMoved {
        item,
        to: state,
        by_session: None,
        because: None,
    }))?;
    Ok(format!("#{} is {state}{pushed}", item.short()))
}

/// Edits `item` in `$EDITOR`, as `wf item edit` does.
fn edit_item(
    workspace: &mut Workspace,
    terminal: &mut DefaultTerminal,
    item: ItemId,
) -> anyhow::Result<String> {
    let draft = {
        let ItemEntry::Live(live) = workspace
            .state()
            .item(item)
            .ok_or_else(|| anyhow!("#{} is gone", item.short()))?
        else {
            bail!("#{} is archived; unarchive it to edit", item.short());
        };
        Draft::of(live)
    };
    let markdown = draft.to_markdown()?;
    let draft = suspended(terminal, || {
        edit_until_valid(&markdown, ".md", Draft::from_markdown)
    })??;
    let changes = match workspace.state().item(item) {
        Some(ItemEntry::Live(live)) => draft.changes(live),
        Some(ItemEntry::Archived(_)) | None => bail!("#{} is gone", item.short()),
    };
    if changes.is_empty() {
        return Ok("nothing changed".to_owned());
    }
    workspace.author(EventBody::ItemEdited(ItemEdited { item, changes }))?;
    Ok(format!("edited #{}", item.short()))
}

fn permit(
    workspace: &mut Workspace,
    request: Request,
    answer: Permission,
) -> anyhow::Result<String> {
    let held = workspace.answer_permission(request.session, request.since_ms, answer)?;
    let verb = match answer {
        Permission::Allow => "allowed",
        Permission::Deny => "denied",
    };
    Ok(format!("{verb}: {}", held.what))
}

/// Writes a comment on `item` in `$EDITOR`, starting from `text`, then posts it.
fn comment_in_editor(
    workspace: &mut Workspace,
    terminal: &mut DefaultTerminal,
    item: ItemId,
    text: String,
) -> anyhow::Result<String> {
    let title = workspace
        .state()
        .item(item)
        .map(|view| format!(" \"{}\"", view.title()))
        .unwrap_or_default();
    let template = format!(
        "{text}\n{SCISSORS}\nYour comment on #{}{title}\n",
        item.short()
    );
    let text = suspended(terminal, || edit_in_editor(&template, ".md"))??;
    let text = above_scissors(&text);
    if text.is_empty() {
        return Ok("nothing written, no comment posted".to_owned());
    }
    comment(workspace, item, text)
}

/// Writes the comment that resolves a blocker in `$EDITOR`, starting from `text`.
fn resolve_in_editor(
    workspace: &mut Workspace,
    terminal: &mut DefaultTerminal,
    item: ItemId,
    blocker: CommentId,
    text: String,
) -> anyhow::Result<String> {
    let question = blocker_text(workspace, blocker);
    let template = format!(
        "{text}\n{SCISSORS}\nYour answer to the blocker {} on #{}:\n\n{question}\n",
        blocker.short(),
        item.short()
    );
    let text = suspended(terminal, || edit_in_editor(&template, ".md"))??;
    let text = above_scissors(&text);
    if text.is_empty() {
        return Ok("nothing written, the blocker stays open".to_owned());
    }
    resolve(workspace, item, blocker, text)
}

fn page_diff(
    workspace: &Workspace,
    terminal: &mut DefaultTerminal,
    item: ItemId,
) -> anyhow::Result<String> {
    let git = workspace.repository()?;
    let work = item_work(workspace, &git, item)?;
    // The pager stays open on a short diff too, so quitting it goes back to the dashboard.
    suspended(terminal, || {
        git.show_diff(&work, &[], wf_vcs::Pager::StaysOpen)
    })??;
    Ok(String::new())
}

/// Requests changes on the work of `item`, with the comments on lines drafted for its review.
/// Without a change request among them, the change requests are written in `$EDITOR` first, one
/// per line.
fn request_changes(
    workspace: &mut Workspace,
    terminal: &mut DefaultTerminal,
    item: ItemId,
    reviewed: Option<CommitId>,
    comments: &[LineComment],
) -> anyhow::Result<Outcome> {
    let commit = submitted(workspace.state(), item, reviewed)?;
    let on_lines = comments
        .iter()
        .filter(|comment| comment.kind == CommentKind::ChangeRequest)
        .count();
    let mut changes = Vec::new();
    if on_lines == 0 {
        let template = format!(
            "\n{SCISSORS}\nThe changes the work of #{} needs, one per line.\n",
            item.short()
        );
        let text = suspended(terminal, || edit_in_editor(&template, ".md"))??;
        changes = above_scissors(&text)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect();
        if changes.is_empty() {
            return Ok(Outcome::Did(
                "no changes written, the review still waits".to_owned(),
            ));
        }
    }
    let mut sink = WorkspaceSink {
        workspace,
        item,
        commit,
        overruled: 0,
    };
    let sent = send_review(&mut sink, comments, &changes, Verdict::RequestChanges);
    let count = changes.len() + on_lines;
    let plain = comments.len() - on_lines;
    reviewed_or_failed(sent, item, || {
        let also = if plain > 0 {
            format!(" and {plain} comment{}", plural(plain))
        } else {
            String::new()
        };
        format!(
            "requested {count} change{} on #{}{also}",
            plural(count),
            item.short()
        )
    })
}

/// A review that failed after `drafts` of its comments on lines and `changes` of its written
/// change requests were posted.
struct Unsent {
    drafts: usize,
    changes: usize,
    error: anyhow::Error,
}

fn reviewed_or_failed(
    sent: Result<(), Unsent>,
    item: ItemId,
    what: impl FnOnce() -> String,
) -> anyhow::Result<Outcome> {
    match sent {
        Ok(()) => Ok(Outcome::Reviewed { item, what: what() }),
        Err(Unsent {
            drafts: 0,
            changes: 0,
            error,
        }) => Err(error),
        Err(Unsent {
            drafts,
            changes,
            error,
        }) => Ok(Outcome::ReviewFailed {
            item,
            posted: drafts,
            error: if changes == 0 {
                error
            } else {
                error.context(format!(
                    "{changes} written change request{} stay posted",
                    plural(changes)
                ))
            },
        }),
    }
}

/// Where a review is sent to.
trait ReviewSink {
    fn line_comment(&mut self, comment: &LineComment) -> anyhow::Result<()>;
    fn change_request(&mut self, text: &str) -> anyhow::Result<()>;
    fn verdict(&mut self, verdict: Verdict) -> anyhow::Result<()>;
}

struct WorkspaceSink<'a> {
    workspace: &'a mut Workspace,
    item: ItemId,
    commit: Option<CommitId>,
    /// How many change requests the verdict overruled and resolved.
    overruled: usize,
}

impl ReviewSink for WorkspaceSink<'_> {
    fn line_comment(&mut self, comment: &LineComment) -> anyhow::Result<()> {
        post_line_comment(self.workspace, self.item, comment.clone())?;
        Ok(())
    }

    fn change_request(&mut self, text: &str) -> anyhow::Result<()> {
        self.workspace.author(EventBody::Commented(Commented {
            kind: CommentKind::ChangeRequest,
            ..Commented::plain(self.item, text)
        }))?;
        Ok(())
    }

    fn verdict(&mut self, verdict: Verdict) -> anyhow::Result<()> {
        let written = write_verdict(self.workspace, self.item, self.commit, verdict, None)?;
        self.overruled = written.overruled;
        Ok(())
    }
}

/// Posts the drafted comments on lines and the `changes` as change requests, then gives the
/// verdict.
fn send_review(
    sink: &mut impl ReviewSink,
    comments: &[LineComment],
    changes: &[String],
    verdict: Verdict,
) -> Result<(), Unsent> {
    let mut sent = Unsent {
        drafts: 0,
        changes: 0,
        error: anyhow::anyhow!("sent"),
    };
    let mut send = || -> anyhow::Result<()> {
        for comment in comments {
            sink.line_comment(comment)?;
            sent.drafts += 1;
        }
        for change in changes {
            sink.change_request(change)?;
            sent.changes += 1;
        }
        sink.verdict(verdict)
    };
    send().map_err(|error| Unsent { error, ..sent })
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// ` with 2 comments on lines`, or nothing for none.
fn with(comments: &[LineComment]) -> String {
    match comments.len() {
        0 => String::new(),
        count => format!(" with {count} comment{} on lines", plural(count)),
    }
}

fn post_line_comment(
    workspace: &mut Workspace,
    item: ItemId,
    comment: LineComment,
) -> anyhow::Result<String> {
    let location = comment.lines.location();
    let kind = comment.kind;
    workspace.author(EventBody::Commented(Commented {
        kind,
        lines: Some(comment.lines),
        ..Commented::plain(item, comment.text)
    }))?;
    Ok(format!(
        "wrote a {} on {location} of #{}",
        kind.label(),
        item.short()
    ))
}

fn comment(workspace: &mut Workspace, item: ItemId, text: String) -> anyhow::Result<String> {
    workspace.author(EventBody::Commented(Commented::plain(item, text)))?;
    Ok(format!("commented on #{}", item.short()))
}

fn resolve(
    workspace: &mut Workspace,
    item: ItemId,
    blocker: CommentId,
    text: String,
) -> anyhow::Result<String> {
    workspace.author(EventBody::Commented(Commented {
        resolves: vec![blocker],
        ..Commented::plain(item, text)
    }))?;
    Ok(format!("resolved {}", blocker.short()))
}

/// The commit the work of `item` was submitted as, if it is what the principal `reviewed`.
fn submitted(
    state: &State,
    item: ItemId,
    reviewed: Option<CommitId>,
) -> anyhow::Result<Option<CommitId>> {
    let (commit, _) = state
        .submission(item)
        .ok_or_else(|| anyhow!("#{} has no submitted work to review", item.short()))?;
    if commit != reviewed {
        bail!(
            "#{} was submitted again or its branch moved since you looked; look at the diff again",
            item.short()
        );
    }
    Ok(commit)
}

/// The text of a blocker, quoted, for reading it while answering.
fn blocker_text(workspace: &Workspace, blocker: CommentId) -> String {
    let text = match workspace.event(&blocker.into()) {
        Ok(Some(event)) => {
            // A blocker is a comment; anything else has no text to quote.
            if let EventBody::Commented(comment) = event.content.body {
                comment.text
            } else {
                String::new()
            }
        }
        Ok(None) | Err(_) => String::new(),
    };
    text.lines()
        .map(|line| format!("> {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn above_scissors(text: &str) -> String {
    text.split(SCISSORS)
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned()
}

/// Runs `run` on the plain terminal, as the dashboard left it, and draws the dashboard again
/// afterwards.
fn suspended<T>(terminal: &mut DefaultTerminal, run: impl FnOnce() -> T) -> anyhow::Result<T> {
    super::stop_reports()?;
    ratatui::restore();
    let result = run();
    enable_raw_mode()?;
    execute!(std::io::stdout(), EnterAlternateScreen)?;
    super::start_reports()?;
    terminal.clear()?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use wf_core::session::EndReason;
    use wf_core::testing::*;

    use super::*;

    #[test]
    fn only_what_is_above_the_scissors_counts() {
        let text = format!("Use humantime.\n\n{SCISSORS}\n> Which crate?\n");
        assert_eq!(above_scissors(&text), "Use humantime.");
        assert_eq!(above_scissors(&format!("\n{SCISSORS}\n> Which?")), "");
    }

    #[test]
    fn a_verdict_is_only_for_the_submission_that_was_looked_at() {
        let fixture = Fixture::new(1);
        let mut state = fixture.state();
        let apply = |state: &mut State, body| fixture.apply(state, 0, 1_000, body);
        apply(&mut state, define_agent("coder"));
        let item = apply(&mut state, create("task", "Work")).id().into();
        assert!(submitted(&state, item, None).is_err(), "nothing submitted");

        let session = apply(&mut state, start_session("coder", item)).id().into();
        let submitted_commit =
            CommitId::Git("a8d4948400000000000000000000000000000000".parse().unwrap());
        apply(&mut state, submit_session(session, submitted_commit));
        assert_eq!(
            submitted(&state, item, Some(submitted_commit)).unwrap(),
            Some(submitted_commit)
        );
        let moved_on = CommitId::Git("b8d4948400000000000000000000000000000000".parse().unwrap());
        let refused = submitted(&state, item, Some(moved_on)).unwrap_err();
        assert!(
            refused.to_string().contains("look at the diff again"),
            "{refused}"
        );
        assert!(submitted(&state, item, None).is_err());
    }

    #[test]
    fn an_item_a_work_session_holds_is_not_offered_to_open() {
        let fixture = Fixture::new(1);
        let mut state = fixture.state();
        let apply = |state: &mut State, body| fixture.apply(state, 0, 1_000, body);
        apply(&mut state, define_agent("coder"));
        let item = apply(&mut state, create("task", "Work")).id().into();
        let session = apply(&mut state, start_session("coder", item)).id().into();
        apply(&mut state, moved(item, ItemState::Active));
        let offered = |state: &State| {
            let Choice::State { states, .. } = states(state, item).unwrap() else {
                panic!("not a choice of states");
            };
            states
        };
        assert_eq!(
            offered(&state),
            [ItemState::Draft, ItemState::Done, ItemState::Cancelled]
        );

        apply(&mut state, end_session(session, EndReason::Released));
        assert!(offered(&state).contains(&ItemState::Open));
    }

    /// Fails the `fail_at`th call to it, counting from 0.
    struct FakeSink {
        calls: usize,
        fail_at: usize,
    }

    impl FakeSink {
        fn call(&mut self) -> anyhow::Result<()> {
            self.calls += 1;
            if self.calls - 1 == self.fail_at {
                bail!("write failed");
            }
            Ok(())
        }
    }

    impl ReviewSink for FakeSink {
        fn line_comment(&mut self, _: &LineComment) -> anyhow::Result<()> {
            self.call()
        }
        fn change_request(&mut self, _: &str) -> anyhow::Result<()> {
            self.call()
        }
        fn verdict(&mut self, _: Verdict) -> anyhow::Result<()> {
            self.call()
        }
    }

    fn line_comment() -> LineComment {
        LineComment {
            kind: CommentKind::Plain,
            lines: wf_core::event::LineAnchor {
                commit: CommitId::Git("b8d4948400000000000000000000000000000000".parse().unwrap()),
                path: "src/lib.rs".to_owned(),
                side: wf_core::event::DiffSide::New,
                first: 1,
                last: 1,
                quote: String::new(),
            },
            text: "Say why.".to_owned(),
        }
    }

    fn an_item() -> ItemId {
        let fixture = Fixture::new(1);
        let mut state = fixture.state();
        fixture
            .apply(&mut state, 0, 1_000, create("task", "Work"))
            .id()
            .into()
    }

    fn failed_review(fail_at: usize) -> Result<(), Unsent> {
        let mut sink = FakeSink { calls: 0, fail_at };
        let comments = [line_comment(), line_comment()];
        let changes = ["Rename it.".to_owned()];
        send_review(&mut sink, &comments, &changes, Verdict::RequestChanges)
    }

    #[test]
    fn a_review_that_fails_before_anything_is_posted_is_an_error() {
        let item = an_item();
        let outcome = reviewed_or_failed(failed_review(0), item, String::new);
        assert!(outcome.is_err());
    }

    #[test]
    fn a_review_that_fails_after_drafts_were_posted_says_how_many() {
        let item = an_item();
        let Ok(Outcome::ReviewFailed { posted, .. }) =
            reviewed_or_failed(failed_review(1), item, String::new)
        else {
            panic!("expected ReviewFailed");
        };
        assert_eq!(posted, 1);
    }

    #[test]
    fn a_review_that_fails_at_the_verdict_leaves_the_posted_change_requests_named() {
        let item = an_item();
        let Ok(Outcome::ReviewFailed { posted, error, .. }) =
            reviewed_or_failed(failed_review(3), item, String::new)
        else {
            panic!("expected ReviewFailed");
        };
        assert_eq!(posted, 2);
        assert!(format!("{error:#}").contains("1 written change request stay posted"));
    }

    #[test]
    fn a_review_that_goes_through_is_reviewed() {
        let item = an_item();
        let sent = failed_review(usize::MAX);
        let outcome = reviewed_or_failed(sent, item, || "approved".to_owned()).unwrap();
        assert!(matches!(outcome, Outcome::Reviewed { .. }));
    }
}

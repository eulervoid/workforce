//! What a session is told about its task: the item, what was said on it, and how to report back.
//! The daemon sends it as the first prompt of a session, and `wf work brief` prints it again.

use std::fmt::Write;

use crate::event::CommentKind;
use crate::id::{CommentId, HashId, ItemId, SessionId};
use crate::identity::Actor;
use crate::item::{Item, ItemEntry};
use crate::name::ReviewStepName;
use crate::policy::ReviewStep;
use crate::review::ReviewStatus;
use crate::session::{CommitId, EndReason, SessionRole};
use crate::state::State;
use crate::told::ItemComment;

/// What a brief tells that the state does not hold: the texts of comments, which only the log
/// has, and the branch and diffs of the repository.
pub trait Sources {
    /// The text of a comment, with the lines of the diff it is about above it, as
    /// [`crate::event::Commented::text_with_lines`] writes them.
    fn text(&self, comment: CommentId) -> Option<String>;
    /// The branch the work on `item` is committed to.
    fn branch(&self, item: ItemId) -> String;
    /// What `commit` changes against `target`.
    fn diff(&self, target: &str, commit: CommitId) -> String;
    /// The ref this device keeps the head of `session` under, when its work was set apart from
    /// the branch of its item because that session lost the claim.
    fn diverged(&self, _session: SessionId) -> Option<String> {
        None
    }
}

/// The brief of a session, by its role.
pub enum Brief {
    Work(WorkBrief),
    Review(ReviewBrief),
    Supervise(SuperviseBrief),
    Diagnose(DiagnoseBrief),
}

impl Brief {
    /// The brief of `session`, or `None` when the session or its item is not known or archived.
    pub fn of(state: &State, session: SessionId, sources: &impl Sources) -> Option<Self> {
        let session = state.session(session)?;
        let Some(ItemEntry::Live(item)) = state.item(session.item()) else {
            return None;
        };
        let merge = &state.policy()?.policy.merge;
        let target = merge.target.clone();
        let checks: Vec<String> = merge
            .checks
            .iter()
            .map(|check| check.name.to_string())
            .collect();
        let agent = session.agent();
        let instructions = state
            .agent(&agent.name, agent.principal)
            .and_then(|definition| definition.instructions.clone());
        let mentioned = mentioned(state, session.id(), item.id(), sources);
        Some(match session.role() {
            SessionRole::Work => Brief::Work(WorkBrief {
                agent: agent.name.to_string(),
                instructions: instructions.clone(),
                item: HashId::short(&item.id()),
                title: item.title().to_string(),
                body: item.body().render_numbered(),
                branch: sources.branch(item.id()),
                target,
                checks,
                dependencies: dependencies(state, item, sources),
                change_requests: open_change_requests(state, item, sources),
                earlier: earlier(state, item, sources),
                mentioned,
                diverged: diverged(state, item, sources),
            }),
            SessionRole::Review { commit, step } => Brief::Review(ReviewBrief {
                agent: agent.name.to_string(),
                instructions: instructions.clone(),
                item: HashId::short(&item.id()),
                title: item.title().to_string(),
                body: item.body().render_numbered(),
                commit: commit.map(|commit| commit.to_string()),
                diff: commit.map_or_else(String::new, |commit| sources.diff(&target, commit)),
                target,
                checks,
                step: review_step(state, item.id(), &step),
                earlier: earlier(state, item, sources),
                mentioned,
            }),
            SessionRole::Supervise => {
                let blockers = state
                    .open_blockers_of(item)
                    .into_iter()
                    .filter(|blocker| state.answers(agent, blocker))
                    .map(|blocker| OpenComment {
                        comment: blocker.comment.short(),
                        by: speaker(state, &item.comments()[&blocker.comment].by),
                        text: sources.text(blocker.comment).unwrap_or_default(),
                    })
                    .collect();
                let principal = state.principal_label(&agent.principal);
                Brief::Supervise(SuperviseBrief {
                    agent: agent.name.to_string(),
                    instructions,
                    item: HashId::short(&item.id()),
                    title: item.title().to_string(),
                    body: item.body().render_numbered(),
                    earlier: earlier(state, item, sources),
                    mentioned,
                    blockers,
                    principal,
                })
            }
            SessionRole::Diagnose => {
                let attempts = state
                    .sessions_of(item.id())
                    .filter(|other| other.role() == SessionRole::Work)
                    .filter_map(|other| {
                        let ended = other.ended()?;
                        match ended.reason {
                            EndReason::Released | EndReason::TimedOut | EndReason::Failed => {
                                Some(format!(
                                    "{}/{} {}",
                                    state.principal_label(&other.agent().principal),
                                    other.agent().name,
                                    ended.reason.label()
                                ))
                            }
                            EndReason::Submitted
                            | EndReason::Reviewed
                            | EndReason::Supervised
                            | EndReason::Cancelled
                            | EndReason::Interrupted
                            | EndReason::Diagnosed
                            | EndReason::MachineFault(_)
                            | EndReason::LeaseExpired
                            | EndReason::Outclaimed => None,
                        }
                    })
                    .collect();
                Brief::Diagnose(DiagnoseBrief {
                    agent: agent.name.to_string(),
                    instructions,
                    item: HashId::short(&item.id()),
                    title: item.title().to_string(),
                    body: item.body().render_numbered(),
                    earlier: earlier(state, item, sources),
                    mentioned,
                    attempts,
                })
            }
        })
    }

    pub fn render(&self) -> String {
        match self {
            Brief::Work(brief) => brief.render(),
            Brief::Review(brief) => brief.render(),
            Brief::Supervise(brief) => brief.render(),
            Brief::Diagnose(brief) => brief.render(),
        }
    }
}

/// Who wrote something, as an agent is told: `@ada` for a principal, `@ada/coder` for an
/// agent, and `@daemon`. A principal who is not a member is named by its short key.
pub fn speaker(state: &State, actor: &Actor) -> String {
    match actor {
        Actor::Principal(principal) => state.principal_label(principal),
        Actor::Daemon(_) => "@daemon".to_string(),
        Actor::Agent { agent, .. } => {
            format!("{}/{}", state.principal_label(&agent.principal), agent.name)
        }
    }
}

/// What to say to a session whose wait is over: what was written for it and not told yet, the
/// `untold` comments of [`State::untold`], and what finished dependencies left.
pub fn resumed_text(
    state: &State,
    session: SessionId,
    untold: &[ItemComment],
    sources: &impl Sources,
) -> String {
    let mut text = String::from("What you were waiting for is resolved.");
    let Some(folded) = state.session(session) else {
        text.push_str("\nContinue.");
        return text;
    };
    let item = folded.item();
    let lines = told_lines(state, item, untold, sources);
    if !lines.is_empty() {
        text.push_str("\nWritten for you meanwhile:");
        lines.iter().for_each(|line| text.push_str(line));
    }
    let graph = state.dependency_graph();
    for dependency in graph.depends_on(item) {
        let Some(ItemEntry::Live(dependency)) = state.item(dependency) else {
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

/// What to say to a session the daemon resumed after a restart: that it was, and the `untold`
/// comments written for it.
pub fn restarted_text(
    state: &State,
    session: SessionId,
    untold: &[ItemComment],
    sources: &impl Sources,
) -> String {
    let mut text = String::from(
        "wf restarted while you were working, and resumed this conversation. A command you ran \
         may not have finished; check before you go on.",
    );
    if let Some(folded) = state.session(session) {
        let lines = told_lines(state, folded.item(), untold, sources);
        if !lines.is_empty() {
            text.push_str("\nWritten for you since:");
        }
        lines.iter().for_each(|line| text.push_str(line));
    }
    text.push_str("\n`wf work brief` shows the item as it is now. Continue.");
    text
}

/// The prompt at the end of a turn, `prompt`, after the `untold` comments written for `session`.
pub fn with_untold(
    state: &State,
    session: SessionId,
    untold: &[ItemComment],
    sources: &impl Sources,
    prompt: String,
) -> String {
    let Some(folded) = state.session(session) else {
        return prompt;
    };
    let lines = told_lines(state, folded.item(), untold, sources);
    if lines.is_empty() {
        return prompt;
    }
    let mut text = String::from("Written for you during your turn:");
    lines.iter().for_each(|line| text.push_str(line));
    text.push_str("\n\n");
    text.push_str(&prompt);
    text
}

/// The comments in `told`, each as a line of a list with who wrote it, its kind and its id; one
/// on another item than `own`, the item of the session, names the item.
fn told_lines(
    state: &State,
    own: ItemId,
    told: &[ItemComment],
    sources: &impl Sources,
) -> Vec<String> {
    told.iter()
        .filter_map(|written| {
            let Some(ItemEntry::Live(item)) = state.item(written.item) else {
                return None;
            };
            let comment = item.comments().get(&written.comment)?;
            let text = sources.text(written.comment)?;
            let on = if written.item == own {
                String::new()
            } else {
                format!(" on #{} {}", HashId::short(&written.item), item.title())
            };
            Some(format!(
                "\n- {}{on} ({} {}): {}",
                speaker(state, &comment.by),
                comment.kind.label(),
                written.comment.short(),
                text.trim()
            ))
        })
        .collect()
}

/// Everything said on the item, oldest first.
fn earlier(state: &State, item: &Item, sources: &impl Sources) -> Vec<Earlier> {
    let mut comments: Vec<_> = item.comments().iter().collect();
    comments.sort_by_key(|(id, comment)| (comment.at, **id));
    comments
        .into_iter()
        .filter_map(|(id, comment)| {
            Some(Earlier {
                by: speaker(state, &comment.by),
                kind: comment.kind.label(),
                text: sources.text(*id)?,
            })
        })
        .collect()
}

/// The comments on other items written for `session`, which holds `own`: the agent's inbox, and
/// so far nothing that mentions the session.
fn mentioned(
    state: &State,
    session: SessionId,
    own: ItemId,
    sources: &impl Sources,
) -> Vec<Mentioned> {
    state
        .written_for(session)
        .into_iter()
        .filter(|written| written.item != own)
        .filter_map(|written| {
            let Some(ItemEntry::Live(item)) = state.item(written.item) else {
                return None;
            };
            let comment = item.comments().get(&written.comment)?;
            Some(Mentioned {
                by: speaker(state, &comment.by),
                item: HashId::short(&item.id()),
                title: item.title().to_string(),
                text: sources.text(written.comment)?,
            })
        })
        .collect()
}

/// The change requests on the item nothing has resolved yet, oldest first.
fn open_change_requests(state: &State, item: &Item, sources: &impl Sources) -> Vec<OpenComment> {
    let mut open: Vec<_> = item
        .comments()
        .iter()
        .filter(|(_, comment)| comment.kind.is_change_request() && comment.is_open())
        .collect();
    open.sort_by_key(|(id, comment)| (comment.at, **id));
    open.into_iter()
        .map(|(id, comment)| OpenComment {
            comment: id.short(),
            by: speaker(state, &comment.by),
            text: sources.text(*id).unwrap_or_default(),
        })
        .collect()
}

/// The work of the other sessions on the item that this device keeps apart from its branch.
fn diverged(state: &State, item: &Item, sources: &impl Sources) -> Vec<Diverged> {
    // What the branch holds is not set apart from it, whatever a ref left over says.
    let branch = state
        .item_heads()
        .into_iter()
        .find(|heads| heads.item == item.id())
        .and_then(|heads| heads.holder);
    state
        .sessions_of(item.id())
        .filter(|session| session.role() == SessionRole::Work)
        .filter(|session| {
            branch.is_none_or(|holder| {
                session.id() != holder.session && session.head() != Some(holder.commit)
            })
        })
        .filter_map(|session| {
            Some(Diverged {
                reference: sources.diverged(session.id())?,
                commit: session.head()?.to_string(),
                by: speaker(
                    state,
                    &Actor::Agent {
                        agent: session.agent().clone(),
                        session: session.id(),
                    },
                ),
            })
        })
        .collect()
}

/// The latest plain comment on an item: the answer, for a question.
pub fn last_comment(item: &Item, sources: &impl Sources) -> Option<String> {
    item.comments()
        .iter()
        .filter(|(_, comment)| comment.kind == CommentKind::Plain)
        .max_by_key(|(id, comment)| (comment.at, **id))
        .and_then(|(id, _)| sources.text(*id))
}

fn dependencies(state: &State, item: &Item, sources: &impl Sources) -> Vec<Dependency> {
    let graph = state.dependency_graph();
    graph
        .depends_on(item.id())
        .filter_map(|dependency| match state.item(dependency) {
            Some(ItemEntry::Live(dependency)) => Some(dependency),
            Some(ItemEntry::Archived(_)) | None => None,
        })
        .map(|dependency| Dependency {
            item: HashId::short(&dependency.id()),
            title: dependency.title().to_string(),
            state: dependency.state().to_string(),
            note: last_comment(dependency, sources),
        })
        .collect()
}

/// Which step of the review level a review session is: the open step of its name, and how many
/// run with it.
fn review_step(state: &State, item: ItemId, name: &Option<ReviewStepName>) -> StepBrief {
    let (requires, instructions, at_once) = match state.review_status(item) {
        Some(ReviewStatus::Awaiting { steps, .. }) => {
            let own = steps.iter().find_map(|step| match step {
                ReviewStep::Agent {
                    name: open,
                    requires,
                    instructions,
                } if open == name => Some((requires.clone(), instructions.clone())),
                ReviewStep::Agent { .. } | ReviewStep::Principal => None,
            });
            let (requires, instructions) = own.unwrap_or_default();
            (requires, instructions, steps.len())
        }
        Some(
            ReviewStatus::ChangesRequested
            | ReviewStatus::RoundLimitReached { .. }
            | ReviewStatus::Approved { .. }
            | ReviewStatus::Merged { .. },
        )
        | None => Default::default(),
    };
    StepBrief {
        name: name.as_ref().map(ToString::to_string),
        requires: requires.iter().map(ToString::to_string).collect(),
        instructions,
        at_once,
    }
}

/// What was said on the item before: a comment, a blocker or a change request.
pub struct Earlier {
    pub by: String,
    pub kind: String,
    pub text: String,
}

/// A comment on another item that mentions the agent.
pub struct Mentioned {
    pub by: String,
    /// The short id of the item it is on.
    pub item: String,
    pub title: String,
    pub text: String,
}

/// A blocker or change request nothing has resolved yet.
pub struct OpenComment {
    /// The short id `--resolve` takes.
    pub comment: String,
    pub by: String,
    pub text: String,
}

/// A dependency of the item, with what was said when it was finished.
pub struct Dependency {
    pub item: String,
    pub title: String,
    pub state: String,
    pub note: Option<String>,
}

/// The brief of a work session.
pub struct WorkBrief {
    /// The name of the agent of the session.
    pub agent: String,
    /// What the owner of the agent tells it in every brief.
    pub instructions: Option<String>,
    /// The short id of the item, as `wf` commands take it.
    pub item: String,
    pub title: String,
    /// The body as markdown, with the number of each step after its checkbox.
    pub body: String,
    pub branch: String,
    pub target: String,
    /// The names of the checks that run on the merge before the target moves.
    pub checks: Vec<String>,
    pub dependencies: Vec<Dependency>,
    /// The change requests the work has to resolve before it is submitted.
    pub change_requests: Vec<OpenComment>,
    pub earlier: Vec<Earlier>,
    /// Comments on other items that mention the agent, since its previous session started.
    pub mentioned: Vec<Mentioned>,
    /// Work of sessions that lost the claim, which is not in the branch.
    pub diverged: Vec<Diverged>,
}

/// The head of a work session that lost the claim on its item, kept apart from the branch.
pub struct Diverged {
    /// The ref that keeps it.
    pub reference: String,
    pub commit: String,
    /// Who did the work, as [`speaker`] names them.
    pub by: String,
}

impl WorkBrief {
    pub fn render(&self) -> String {
        let item = &self.item;
        let mut text = format!(
            "You are the agent `{agent}`, working on item #{item}: {title}\n\n",
            agent = self.agent,
            title = self.title
        );
        instructions(&mut text, self.instructions.as_deref());
        task(&mut text, &self.body);
        if !self.dependencies.is_empty() {
            text.push_str("## Dependencies\n\n");
            for dependency in &self.dependencies {
                let _ = write!(
                    text,
                    "- #{} {} ({})",
                    dependency.item, dependency.title, dependency.state
                );
                match &dependency.note {
                    Some(note) => {
                        let _ = writeln!(text, ": {}", note.trim());
                    }
                    None => text.push('\n'),
                }
            }
            text.push('\n');
        }
        if !self.change_requests.is_empty() {
            text.push_str("## Open change requests\n\nResolve every one before you submit.\n\n");
            open_comments(&mut text, &self.change_requests);
        }
        if !self.diverged.is_empty() {
            text.push_str(
                "## Work set apart\n\n\
                 Sessions that lost the claim on this item left commits that are not in your \
                 branch. Look at them (`git log`, `git diff`) and take what is useful, for example \
                 with `git cherry-pick`.\n\n",
            );
            for work in &self.diverged {
                let _ = writeln!(
                    text,
                    "- {} by {}: `{}`",
                    work.commit, work.by, work.reference
                );
            }
            text.push('\n');
        }
        history(&mut text, &self.earlier);
        mentions(&mut text, &self.mentioned);
        let _ = writeln!(
            text,
            "## How to work\n\n\
             - You are in your own clone of the repository, on the branch `{branch}`, started from `{target}`. Work only here.\n\
             - Commit small and often: only commits carry over to a later session.\n\
             - Tick each step as soon as it is done, not all at the end: `wf item check {item} <number>`\n\
             - Report while you work: `wf comment {item} \"<what you chose, what you rejected, and why>\" --step <number>`\n  \
             - Add `--done` to tick the step with the same command.\n  \
             - Leave out `--step` for a report on the whole item.\n  \
             - Write each report so that it makes sense on its own.",
            branch = self.branch,
            target = self.target,
        );
        if !self.change_requests.is_empty() {
            let _ = writeln!(
                text,
                "- Resolve a change request once it is done: \
                 `wf comment {item} \"<what you did>\" --resolve <id>`"
            );
        }
        if let Some(checks) = checks(&self.checks) {
            let _ = writeln!(
                text,
                "- While you work, build and run the tests near your change; do not run the whole \
                 suite. Submitting runs the checks of the merge ({checks}) in your sandbox and \
                 tells you what failed, so you fix it and submit again. `wf check` runs them \
                 yourself, for when that is worth it."
            );
        }
        let _ = writeln!(
            text,
            "- Need an answer from someone: `wf comment {item} \"<question>\" --block`, then end \
             your turn. You are prompted again once it is answered.\n\
             - Finished: `wf work submit --note \"<what the reviewer should know>\"`. Your commits \
             are reviewed and merged from there.\n\
             - Stuck: `wf work release --note \"<why>\"`\n\
             {AGAIN}"
        );
        text
    }
}

/// The diff is part of the brief up to this many bytes; beyond that the reviewer reads it itself.
pub const MAX_DIFF_LEN: usize = 60_000;

/// The step of a review level a review session takes.
#[derive(Default)]
pub struct StepBrief {
    /// The name of the step in the policy, if it has one.
    pub name: Option<String>,
    /// The capabilities the step requires of the agent.
    pub requires: Vec<String>,
    /// What the policy tells this reviewer to look at, beside the instructions of the agent.
    pub instructions: Option<String>,
    /// How many steps run at once, this one included.
    pub at_once: usize,
}

impl StepBrief {
    /// The step in words.
    fn words(&self) -> String {
        let mut words = match (&self.name, self.requires.is_empty()) {
            (Some(name), _) => format!("the `{name}` review"),
            (None, false) => format!("the review by an agent with {}", self.requires.join(", ")),
            (None, true) => "a review".to_string(),
        };
        if self.at_once > 1 {
            let _ = write!(words, ", one of {} at once", self.at_once);
        }
        words
    }
}

/// The brief of a review session.
pub struct ReviewBrief {
    /// The name of the agent of the session.
    pub agent: String,
    /// What the owner of the agent tells it in every brief.
    pub instructions: Option<String>,
    /// The short id of the item.
    pub item: String,
    pub title: String,
    pub body: String,
    /// The submitted commit, `None` when the work was submitted without commits.
    pub commit: Option<String>,
    pub target: String,
    /// The names of the checks that run on the merge before the target moves.
    pub checks: Vec<String>,
    /// Which step of the review level this is.
    pub step: StepBrief,
    pub earlier: Vec<Earlier>,
    /// Comments on other items that mention the agent, since its previous session started.
    pub mentioned: Vec<Mentioned>,
    pub diff: String,
}

impl ReviewBrief {
    pub fn render(&self) -> String {
        let item = &self.item;
        let mut text = format!(
            "You are the agent `{agent}`, reviewing the work on item #{item}: {title}\n\n",
            agent = self.agent,
            title = self.title
        );
        instructions(&mut text, self.instructions.as_deref());
        task(&mut text, &self.body);
        if let Some(step_instructions) = &self.step.instructions {
            let _ = writeln!(
                text,
                "## What to look at in this review\n\n{}\n",
                step_instructions.trim()
            );
        }
        text.push_str("## The work\n\n");
        match &self.commit {
            Some(commit) => {
                let target = &self.target;
                let _ = writeln!(
                    text,
                    "- Commit `{commit}` is checked out here. It is meant to go into `{target}`.\n\
                     - This is {step}.\n",
                    step = self.step.words(),
                );
                if self.diff.len() <= MAX_DIFF_LEN {
                    let _ = writeln!(text, "```diff\n{}\n```\n", self.diff.trim_end());
                } else {
                    let _ = writeln!(
                        text,
                        "The diff is too large to show here; run `git diff {target}...HEAD`.\n"
                    );
                }
            }
            None => {
                let _ = writeln!(
                    text,
                    "- It was submitted without commits: it is an answer, or the finding that \
                     nothing has to change. What the author said is in the history below.\n\
                     - Nothing is checked out here.\n\
                     - This is {step}.\n",
                    step = self.step.words(),
                );
            }
        }
        history(&mut text, &self.earlier);
        mentions(&mut text, &self.mentioned);
        let _ = writeln!(
            text,
            "## How to review\n\n\
             - Read the change or the answer, and run the tests if there are any.\n\
             {checks}\
             - Do not change anything.\n\
             - Request changes for anything that is wrong, also when it is small. Approve only \
             what you would merge as it is.\n\
             - Give exactly one verdict:\n  \
             - Good as it is: `wf review --approve --note \"<what you checked>\"`\n  \
             - Needs changes: write each change on its own with \
             `wf comment {item} \"<what to change>\" --request-change`, then run \
             `wf review --request-changes`. The next session has to resolve every change request \
             before it submits again.\n  \
             - Your principal has to decide: `wf review --escalate --note \"<what they have to decide>\"`\n\
             {AGAIN}",
            checks = checks(&self.checks).map_or_else(String::new, |checks| format!(
                "- The checks of the merge ({checks}) passed in the sandbox before this review, \
                 on the merge of the work into {target}: do not run them again, and do not \
                 escalate for tests that cannot run in your sandbox. Say in your note what you \
                 could not run here.\n",
                target = self.target
            )),
        );
        text
    }
}

/// The names of `checks`, as code, or `None` when there are none.
fn checks(checks: &[String]) -> Option<String> {
    (!checks.is_empty()).then(|| {
        checks
            .iter()
            .map(|check| format!("`{check}`"))
            .collect::<Vec<_>>()
            .join(", ")
    })
}

/// The brief of a supervise session.
pub struct SuperviseBrief {
    /// The name of the agent of the session.
    pub agent: String,
    /// What the owner of the agent tells it in every brief.
    pub instructions: Option<String>,
    /// The short id of the item, as `wf` commands take it.
    pub item: String,
    pub title: String,
    pub body: String,
    pub earlier: Vec<Earlier>,
    /// Comments on other items that mention the agent, since its previous session started.
    pub mentioned: Vec<Mentioned>,
    /// The blockers the supervisor is asked to answer or escalate.
    pub blockers: Vec<OpenComment>,
    /// The handle of the principal to escalate to, as in `@ada`.
    pub principal: String,
}

impl SuperviseBrief {
    pub fn render(&self) -> String {
        let item = &self.item;
        let mut text = format!(
            "You are the agent `{agent}`, supervising item #{item}: {title}\n\n\
             Work on it waits for an answer.\n\n",
            agent = self.agent,
            title = self.title
        );
        instructions(&mut text, self.instructions.as_deref());
        task(&mut text, &self.body);
        history(&mut text, &self.earlier);
        mentions(&mut text, &self.mentioned);
        text.push_str("## Waiting on you\n\n");
        open_comments(&mut text, &self.blockers);
        let resolve: Vec<String> = self
            .blockers
            .iter()
            .map(|blocker| format!("--resolve {}", blocker.comment))
            .collect();
        let _ = writeln!(
            text,
            "## How to answer\n\n\
             - Do not change any files.\n\
             - Answer everything above in one comment; your session ends with it.\n\
             - You can answer: `wf comment {item} \"<answer>\" {resolve}`\n\
             - Your principal has to decide: `wf comment {item} \"{principal} <what they have to decide>\" \
             --block {resolve}`. This hands the question to them.\n\
             {AGAIN}",
            resolve = resolve.join(" "),
            principal = self.principal,
        );
        text
    }
}

/// The brief of a diagnose session.
pub struct DiagnoseBrief {
    /// The name of the agent of the session.
    pub agent: String,
    /// What the owner of the agent tells it in every brief.
    pub instructions: Option<String>,
    /// The short id of the item, as `wf` commands take it.
    pub item: String,
    pub title: String,
    pub body: String,
    pub earlier: Vec<Earlier>,
    /// Comments on other items that mention the agent, since its previous session started.
    pub mentioned: Vec<Mentioned>,
    /// The work sessions that did not finish, oldest first, as `@ada/coder released`.
    pub attempts: Vec<String>,
}

impl DiagnoseBrief {
    pub fn render(&self) -> String {
        let item = &self.item;
        let mut text = format!(
            "You are the agent `{agent}`, looking into item #{item}: {title}\n\n\
             Work on it did not finish. Find out why, so that the next attempt goes better.\n\n",
            agent = self.agent,
            title = self.title
        );
        instructions(&mut text, self.instructions.as_deref());
        task(&mut text, &self.body);
        history(&mut text, &self.earlier);
        mentions(&mut text, &self.mentioned);
        if !self.attempts.is_empty() {
            text.push_str("## Attempts that did not finish\n\n");
            for attempt in &self.attempts {
                let _ = writeln!(text, "- {attempt}");
            }
            text.push('\n');
        }
        let _ = writeln!(
            text,
            "## How to report\n\n\
             - Do not change any files. Nothing is checked out here.\n\
             - Say in one comment what went wrong and what the next attempt should do \
             differently; your session ends with it: \
             `wf comment {item} \"<what went wrong, and what to do differently>\"`\n\
             - Nothing can go on without your principal's decision: \
             `wf comment {item} \"<what they have to decide>\" --block`\n\
             {AGAIN}"
        );
        text
    }
}

/// The prompt that goes out when a turn ended and the session on `item` is still going: how a
/// session of `role` ends.
pub fn nudge(item: ItemId, role: SessionRole) -> String {
    let item = HashId::short(&item);
    match role {
        SessionRole::Work => format!(
            "Continue.\n\
             - Finished: `wf work submit --note \"<what the reviewer should know>\"`\n\
             - Need an answer from someone: `wf comment {item} \"<question>\" --block`, then end your turn.\n\
             - Stuck: `wf work release --note \"<why>\"`\n\
             {AGAIN}"
        ),
        SessionRole::Review { .. } => format!(
            "Continue, and give exactly one verdict.\n\
             - Good as it is: `wf review --approve --note \"<what you checked>\"`\n\
             - Needs changes: `wf comment {item} \"<what to change>\" --request-change` for each, \
             then `wf review --request-changes`\n\
             - Your principal has to decide: `wf review --escalate --note \"<what they have to decide>\"`\n\
             {AGAIN}"
        ),
        SessionRole::Diagnose => format!(
            "Continue, and report in one comment.\n\
             - `wf comment {item} \"<what went wrong, and what to do differently>\"`\n\
             {AGAIN}"
        ),
        SessionRole::Supervise => format!(
            "Continue, and answer in one comment.\n\
             - You can answer: `wf comment {item} \"<answer>\" --resolve-blockers`\n\
             - Your principal has to decide: `wf comment {item} \"<what they have to decide>\" --block --resolve-blockers`\n\
             {AGAIN}"
        ),
    }
}

/// The prompt that goes out when a turn of a work session on `item` ended with the rebase of its
/// branch onto `target` stopped at a commit that conflicts in `paths`, as a refused submission
/// left it.
pub fn rebase_nudge(item: ItemId, target: &str, paths: &[String]) -> String {
    format!(
        "Your submission rebases the branch onto {target}, and the rebase is stopped at a commit \
         that conflicts in {}. Resolve the conflicts in these files, then submit again: `wf work submit` \
         stages the text files whose conflict markers you removed and goes on with the rebase, \
         which may stop at a later commit. Stage a binary file, or a file one side deleted, \
         yourself with `git add` or `git rm` once it is as it should be.\n\n{}",
        paths.join(", "),
        nudge(item, SessionRole::Work)
    )
}

/// The prompt that goes out when the checks of the policy failed on the merge of the work of
/// `item` as it was submitted: `failures` says which and how, `comment` is the short id of the
/// change request that tells the same, and the agent fixes them and submits again.
pub fn checks_failed_nudge(item: ItemId, comment: &str, failures: &str) -> String {
    format!(
        "Your submission was not sent to review: its checks failed.\n\n{failures}\n\n\
         Fix them, resolve the change request with `wf comment {item} \"<what you did>\" --resolve {comment}`, \
         then submit again.\n\n{}",
        nudge(item, SessionRole::Work),
        item = HashId::short(&item),
    )
}

/// The last rule of every brief.
const AGAIN: &str = "- Lost track: `wf work brief` prints this brief again.";

fn instructions(text: &mut String, instructions: Option<&str>) {
    if let Some(instructions) = instructions.filter(|instructions| !instructions.trim().is_empty())
    {
        let _ = writeln!(text, "## Instructions\n\n{}\n", instructions.trim());
    }
}

fn task(text: &mut String, body: &str) {
    if !body.trim().is_empty() {
        let _ = writeln!(text, "## Task\n\n{}\n", body.trim());
    }
}

fn history(text: &mut String, earlier: &[Earlier]) {
    if earlier.is_empty() {
        return;
    }
    text.push_str("## What was said before\n\n");
    for earlier in earlier {
        let _ = writeln!(
            text,
            "- {} ({}): {}",
            earlier.by,
            earlier.kind,
            earlier.text.trim()
        );
    }
    text.push('\n');
}

fn mentions(text: &mut String, mentioned: &[Mentioned]) {
    if mentioned.is_empty() {
        return;
    }
    text.push_str("## Mentions of you elsewhere\n\n");
    for mention in mentioned {
        let _ = writeln!(
            text,
            "- {} on #{} {}: {}",
            mention.by,
            mention.item,
            mention.title,
            mention.text.trim()
        );
    }
    text.push('\n');
}

fn open_comments(text: &mut String, open: &[OpenComment]) {
    for comment in open {
        let _ = writeln!(
            text,
            "- {} by {}: {}",
            comment.comment,
            comment.by,
            comment.text.trim()
        );
    }
    text.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn earlier() -> Vec<Earlier> {
        vec![Earlier {
            by: "@ada/coder".into(),
            kind: "comment".into(),
            text: "tried serde first".into(),
        }]
    }

    #[test]
    fn principals_and_agents_are_named_by_handle() {
        use crate::testing::*;

        let fixture = Fixture::new(1);
        let mut state = fixture.state();
        let task: ItemId = fixture
            .apply(&mut state, 0, 2_000, create("task", "Parse durations"))
            .id()
            .into();
        fixture.apply(&mut state, 0, 2_001, define_agent("coder"));
        let session: SessionId = fixture
            .apply(&mut state, 0, 3_000, start_session("coder", task))
            .id()
            .into();
        assert_eq!(speaker(&state, &fixture.founder_actor()), "@ada");
        assert_eq!(
            speaker(&state, &fixture.agent_actor("coder", session)),
            "@ada/coder"
        );
        assert_eq!(
            speaker(&state, &Actor::Daemon(fixture.founder.key())),
            "@daemon"
        );
    }

    #[test]
    fn a_check_failure_is_listed_among_the_open_change_requests() {
        use crate::testing::*;

        struct Texts;
        impl Sources for Texts {
            fn text(&self, _: CommentId) -> Option<String> {
                Some("cargo test failed".into())
            }
            fn branch(&self, _: ItemId) -> String {
                String::new()
            }
            fn diff(&self, _: &str, _: CommitId) -> String {
                String::new()
            }
        }

        let fixture = Fixture::new(1);
        let mut state = fixture.state();
        let task: ItemId = fixture
            .apply(&mut state, 0, 2_000, create("task", "work"))
            .id()
            .into();
        fixture.apply(&mut state, 0, 2_001, check_failure(task, "cargo test"));
        let Some(ItemEntry::Live(item)) = state.item(task) else {
            unreachable!("the item is live")
        };
        let open = open_change_requests(&state, item, &Texts);
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].text, "cargo test failed");
    }

    #[test]
    fn the_work_brief_names_the_agent_the_item_the_branch_and_the_commands() {
        let brief = WorkBrief {
            agent: "coder".into(),
            instructions: None,
            item: "k3f9ab".into(),
            title: "Parse durations".into(),
            body: "- [ ] 1. write parser".into(),
            branch: "wf/k3f9ab".into(),
            target: "main".into(),
            checks: vec![],
            dependencies: vec![Dependency {
                item: "q7x2cd".into(),
                title: "Which crate?".into(),
                state: "answered".into(),
                note: Some("Use humantime".into()),
            }],
            change_requests: vec![],
            earlier: earlier(),
            mentioned: vec![],
            diverged: vec![],
        };
        let text = brief.render();
        assert!(
            text.starts_with(
                "You are the agent `coder`, working on item #k3f9ab: Parse durations\n"
            )
        );
        assert!(text.contains("## Task\n\n- [ ] 1. write parser\n"));
        assert!(text.contains("- #q7x2cd Which crate? (answered): Use humantime"));
        assert!(text.contains("- @ada/coder (comment): tried serde first"));
        assert!(text.contains("`wf/k3f9ab`") && text.contains("`main`"));
        assert!(!text.contains("change request"), "{text}");
        assert!(!text.contains("wf check"), "{text}");
        for command in [
            "wf item check k3f9ab",
            "wf comment k3f9ab",
            "wf work submit",
            "--block",
            "wf work release",
            "wf work brief",
        ] {
            assert!(text.contains(command), "{command}");
        }
    }

    #[test]
    fn open_change_requests_are_listed_with_how_to_resolve_them() {
        let brief = WorkBrief {
            agent: "coder".into(),
            instructions: None,
            item: "k3f9ab".into(),
            title: "Parse durations".into(),
            body: String::new(),
            branch: "wf/k3f9ab".into(),
            target: "main".into(),
            checks: vec!["fmt".into(), "test".into()],
            dependencies: vec![],
            change_requests: vec![OpenComment {
                comment: "c4d2e1".into(),
                by: "@ada/reviewer".into(),
                text: "Exit with 2 on a missing file".into(),
            }],
            earlier: vec![],
            mentioned: vec![],
            diverged: vec![],
        };
        let text = brief.render();
        assert!(text.contains("## Open change requests\n"), "{text}");
        assert!(!text.contains("## Instructions"), "{text}");
        assert!(text.contains("- c4d2e1 by @ada/reviewer: Exit with 2 on a missing file"));
        assert!(text.contains("--resolve <id>"));
        assert!(
            text.contains(
                "Submitting runs the checks of the merge (`fmt`, `test`) in your sandbox"
            ),
            "{text}"
        );
    }

    #[test]
    fn the_instructions_of_the_agent_come_before_the_task() {
        let brief = ReviewBrief {
            agent: "reviewer".into(),
            instructions: Some("Be strict about tests.\n".into()),
            item: "k3f9ab".into(),
            title: "Parse durations".into(),
            body: "- [ ] 1. write parser".into(),
            commit: None,
            target: "main".into(),
            checks: vec![],
            step: StepBrief::default(),
            earlier: vec![],
            mentioned: vec![],
            diff: String::new(),
        };
        let text = brief.render();
        let instructions = text
            .find("## Instructions\n\nBe strict about tests.\n\n")
            .unwrap();
        assert!(instructions < text.find("## Task").unwrap(), "{text}");
    }

    #[test]
    fn the_review_brief_has_the_diff_and_the_verdicts() {
        let brief = ReviewBrief {
            agent: "reviewer".into(),
            instructions: None,
            item: "k3f9ab".into(),
            title: "Parse durations".into(),
            body: String::new(),
            commit: Some("abc123".into()),
            target: "main".into(),
            checks: vec![],
            step: StepBrief {
                name: Some("security".into()),
                requires: vec![],
                instructions: Some("Look for injection.".into()),
                at_once: 3,
            },
            earlier: vec![],
            mentioned: vec![],
            diff: "+added".into(),
        };
        let text = brief.render();
        assert!(text.contains("```diff\n+added\n```"));
        assert!(
            text.contains("Commit `abc123`")
                && text.contains("the `security` review, one of 3 at once")
                && text.contains("Look for injection.")
        );
        for command in ["--approve", "--request-changes", "--escalate"] {
            assert!(text.contains(command), "{command}");
        }
        let large = ReviewBrief {
            diff: "x".repeat(MAX_DIFF_LEN + 1),
            ..brief
        };
        let text = large.render();
        assert!(!text.contains("```diff") && text.contains("git diff main...HEAD"));
    }

    #[test]
    fn the_checks_run_at_submission_and_before_the_review() {
        let work = WorkBrief {
            agent: "coder".into(),
            instructions: None,
            item: "k3f9ab".into(),
            title: "Small".into(),
            body: String::new(),
            branch: "wf/k3f9ab".into(),
            target: "main".into(),
            checks: vec!["fmt".into(), "test".into()],
            dependencies: vec![],
            change_requests: vec![],
            earlier: vec![],
            mentioned: vec![],
            diverged: vec![],
        };
        let review = ReviewBrief {
            agent: "reviewer".into(),
            instructions: None,
            item: "k3f9ab".into(),
            title: "Small".into(),
            body: String::new(),
            commit: Some("abc123".into()),
            target: "main".into(),
            checks: vec!["fmt".into(), "test".into()],
            step: StepBrief::default(),
            earlier: vec![],
            mentioned: vec![],
            diff: "+added".into(),
        };
        let text = work.render();
        assert!(text.contains("do not run the whole suite"), "{text}");
        assert!(!text.contains("Before you submit, run"), "{text}");
        let text = review.render();
        assert!(
            text.contains("passed in the sandbox before this review"),
            "{text}"
        );

        // Without checks, nobody runs them, and the brief does not say otherwise.
        let unchecked = [
            WorkBrief {
                checks: vec![],
                ..work
            }
            .render(),
            ReviewBrief {
                checks: vec![],
                ..review
            }
            .render(),
        ];
        for text in unchecked {
            assert!(!text.contains("checks of the merge"), "{text}");
        }
    }

    #[test]
    fn empty_sections_are_left_out() {
        let brief = WorkBrief {
            agent: "coder".into(),
            instructions: None,
            item: "k3f9ab".into(),
            title: "Small".into(),
            body: String::new(),
            branch: "wf/k3f9ab".into(),
            target: "main".into(),
            checks: vec![],
            dependencies: vec![],
            change_requests: vec![],
            earlier: vec![],
            mentioned: vec![],
            diverged: vec![],
        };
        let text = brief.render();
        for heading in [
            "## Instructions",
            "## Task",
            "## Dependencies",
            "## Open change requests",
            "## What was said before",
        ] {
            assert!(!text.contains(heading), "{heading}");
        }
    }
}

//! What a session is told about its task: the item, what was said on it, and how to report back.
//! The daemon sends it as the first prompt of a session, and `wf work brief` prints it again.

use std::fmt::Write;

use crate::event::CommentKind;
use crate::id::{CommentId, HashId, ItemId, SessionId};
use crate::identity::Actor;
use crate::item::{Item, ItemView, Standing};
use crate::policy::ReviewStep;
use crate::review::ReviewStatus;
use crate::session::{CommitId, SessionRole};
use crate::state::State;

/// What a brief tells that the state does not hold: the texts of comments, which only the log
/// has, and the branch and diffs of the repository.
pub trait Sources {
    fn text(&self, comment: CommentId) -> Option<String>;
    /// The branch the work on `item` is committed to.
    fn branch(&self, item: ItemId) -> String;
    /// What `commit` changes against `target`.
    fn diff(&self, target: &str, commit: CommitId) -> String;
}

/// The brief of a session, by its role.
pub enum Brief {
    Work(WorkBrief),
    Review(ReviewBrief),
    Supervise(SuperviseBrief),
}

impl Brief {
    /// The brief of `session`, or `None` when the session or its item is not known or archived.
    pub fn of(state: &State, session: SessionId, sources: &impl Sources) -> Option<Self> {
        let session = state.session(session)?;
        let Some(ItemView::Live(item)) = state.item(session.item()) else {
            return None;
        };
        let target = state.policy()?.policy.merge.target.clone();
        let agent = session.agent();
        let instructions = state
            .agent(&agent.name, agent.principal)
            .and_then(|definition| definition.instructions.clone());
        Some(match session.role() {
            SessionRole::Work => Brief::Work(WorkBrief {
                agent: agent.name.to_string(),
                instructions: instructions.clone(),
                item: HashId::short(&item.id()),
                title: item.title().to_string(),
                body: item.body().render_numbered(),
                branch: sources.branch(item.id()),
                target,
                dependencies: dependencies(state, item, sources),
                change_requests: open_change_requests(item, sources),
                earlier: earlier(item, sources),
            }),
            SessionRole::Review { commit } => Brief::Review(ReviewBrief {
                agent: agent.name.to_string(),
                instructions: instructions.clone(),
                item: HashId::short(&item.id()),
                title: item.title().to_string(),
                body: item.body().render_numbered(),
                commit: commit.map(|commit| commit.to_string()),
                diff: commit.map_or_else(String::new, |commit| sources.diff(&target, commit)),
                target,
                step: review_step(state, item.id()),
                earlier: earlier(item, sources),
            }),
            SessionRole::Supervise => {
                let blockers = state
                    .open_blockers_of(item)
                    .into_iter()
                    .filter(|blocker| state.answers(agent, blocker))
                    .map(|blocker| OpenComment {
                        comment: blocker.comment.short(),
                        by: speaker(&item.comments()[&blocker.comment].by),
                        text: sources.text(blocker.comment).unwrap_or_default(),
                    })
                    .collect();
                let person = state
                    .members()
                    .find(|(principal, _)| **principal == agent.principal)
                    .map(|(_, member)| format!("@{}", member.handle))
                    .unwrap_or_default();
                Brief::Supervise(SuperviseBrief {
                    agent: agent.name.to_string(),
                    instructions,
                    item: HashId::short(&item.id()),
                    title: item.title().to_string(),
                    body: item.body().render_numbered(),
                    earlier: earlier(item, sources),
                    blockers,
                    person,
                })
            }
        })
    }

    pub fn render(&self) -> String {
        match self {
            Brief::Work(brief) => brief.render(),
            Brief::Review(brief) => brief.render(),
            Brief::Supervise(brief) => brief.render(),
        }
    }
}

/// Who wrote something, as an agent is told: the agent's name, or `someone` for a person.
pub fn speaker(actor: &Actor) -> String {
    match actor {
        Actor::Principal(_) => "someone".to_string(),
        Actor::Daemon(_) => "the daemon".to_string(),
        Actor::Agent { agent, .. } => agent.name.to_string(),
    }
}

/// Everything said on the item, oldest first.
fn earlier(item: &Item, sources: &impl Sources) -> Vec<Earlier> {
    let mut comments: Vec<_> = item.comments().iter().collect();
    comments.sort_by_key(|(id, comment)| (comment.at, **id));
    comments
        .into_iter()
        .filter_map(|(id, comment)| {
            Some(Earlier {
                by: speaker(&comment.by),
                kind: comment.kind.name().replace('_', " "),
                text: sources.text(*id)?,
            })
        })
        .collect()
}

/// The change requests on the item nothing has resolved yet, oldest first.
fn open_change_requests(item: &Item, sources: &impl Sources) -> Vec<OpenComment> {
    let mut open: Vec<_> = item
        .comments()
        .iter()
        .filter(|(_, comment)| comment.kind == CommentKind::ChangeRequest && comment.is_open())
        .collect();
    open.sort_by_key(|(id, comment)| (comment.at, **id));
    open.into_iter()
        .map(|(id, comment)| OpenComment {
            comment: id.short(),
            by: speaker(&comment.by),
            text: sources.text(*id).unwrap_or_default(),
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
    let Some(policy) = state.policy().map(|doc| &doc.policy) else {
        return Vec::new();
    };
    let graph = state.dependency_graph();
    graph
        .depends_on(item.id())
        .filter_map(|dependency| match state.item(dependency) {
            Some(ItemView::Live(dependency)) => Some(dependency),
            Some(ItemView::Archived(_)) | None => None,
        })
        .map(|dependency| Dependency {
            item: HashId::short(&dependency.id()),
            title: dependency.title().to_string(),
            state: match dependency.standing(policy) {
                Standing::Known { state, .. } => state.to_string(),
                Standing::Unknown { state } => {
                    state.map_or_else(|| "unknown".to_string(), ToString::to_string)
                }
            },
            note: last_comment(dependency, sources),
        })
        .collect()
}

/// Which step of the review level a review session is, in words.
fn review_step(state: &State, item: ItemId) -> String {
    match state.review_status(item) {
        Some(ReviewStatus::Awaiting {
            step: ReviewStep::Agent { requires },
        }) => format!(
            "the review by an agent with {}",
            requires
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Some(
            ReviewStatus::Awaiting {
                step: ReviewStep::Principal,
            }
            | ReviewStatus::ChangesRequested
            | ReviewStatus::Approved { .. }
            | ReviewStatus::Merged { .. },
        )
        | None => "a review".to_string(),
    }
}

/// What was said on the item before: a comment, a blocker or a change request.
pub struct Earlier {
    pub by: String,
    pub kind: String,
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
    pub dependencies: Vec<Dependency>,
    /// The change requests the work has to resolve before it is submitted.
    pub change_requests: Vec<OpenComment>,
    pub earlier: Vec<Earlier>,
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
        history(&mut text, &self.earlier);
        let _ = writeln!(
            text,
            "## How to work\n\n\
             - You are in a git worktree on the branch `{branch}`, started from `{target}`. Work only here.\n\
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
    /// Which step of the review level this is, in words.
    pub step: String,
    pub earlier: Vec<Earlier>,
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
        text.push_str("## The work\n\n");
        match &self.commit {
            Some(commit) => {
                let target = &self.target;
                let _ = writeln!(
                    text,
                    "- Commit `{commit}` is checked out here. It is meant to go into `{target}`.\n\
                     - This is {step}.\n",
                    step = self.step,
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
                    step = self.step,
                );
            }
        }
        history(&mut text, &self.earlier);
        let _ = writeln!(
            text,
            "## How to review\n\n\
             - Read the change or the answer, and run the tests if there are any.\n\
             - Do not change anything.\n\
             - Request changes for anything that is wrong, also when it is small. Approve only \
             what you would merge as it is.\n\
             - Give exactly one verdict:\n  \
             - Good as it is: `wf review --approve --note \"<what you checked>\"`\n  \
             - Needs changes: write each change on its own with \
             `wf comment {item} \"<what to change>\" --request-change`, then run \
             `wf review --request-changes`. The next session has to resolve every change request \
             before it submits again.\n  \
             - A person has to decide: `wf review --escalate --note \"<what they have to decide>\"`\n\
             {AGAIN}"
        );
        text
    }
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
    /// The blockers the supervisor is asked to answer or escalate.
    pub blockers: Vec<OpenComment>,
    /// The handle of the person to escalate to, as in `@joshua`.
    pub person: String,
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
             - A person has to decide: `wf comment {item} \"{person} <what they have to decide>\" \
             --block {resolve}`. This hands the question to them.\n\
             {AGAIN}",
            resolve = resolve.join(" "),
            person = self.person,
        );
        text
    }
}

/// The prompt that goes out when a turn ended and the session on `item` is still going.
pub fn nudge(item: ItemId) -> String {
    let item = HashId::short(&item);
    format!(
        "Continue.\n\
         - Finished: `wf work submit --note \"<what the reviewer should know>\"`\n\
         - Need an answer from someone: `wf comment {item} \"<question>\" --block`, then end your turn.\n\
         - Stuck: `wf work release --note \"<why>\"`\n\
         {AGAIN}"
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
            by: "coder".into(),
            kind: "comment".into(),
            text: "tried serde first".into(),
        }]
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
            dependencies: vec![Dependency {
                item: "q7x2cd".into(),
                title: "Which crate?".into(),
                state: "answered".into(),
                note: Some("Use humantime".into()),
            }],
            change_requests: vec![],
            earlier: earlier(),
        };
        let text = brief.render();
        assert!(
            text.starts_with(
                "You are the agent `coder`, working on item #k3f9ab: Parse durations\n"
            )
        );
        assert!(text.contains("## Task\n\n- [ ] 1. write parser\n"));
        assert!(text.contains("- #q7x2cd Which crate? (answered): Use humantime"));
        assert!(text.contains("- coder (comment): tried serde first"));
        assert!(text.contains("`wf/k3f9ab`") && text.contains("`main`"));
        assert!(!text.contains("change request"), "{text}");
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
            dependencies: vec![],
            change_requests: vec![OpenComment {
                comment: "c4d2e1".into(),
                by: "reviewer".into(),
                text: "Exit with 2 on a missing file".into(),
            }],
            earlier: vec![],
        };
        let text = brief.render();
        assert!(text.contains("## Open change requests\n"), "{text}");
        assert!(!text.contains("## Instructions"), "{text}");
        assert!(text.contains("- c4d2e1 by reviewer: Exit with 2 on a missing file"));
        assert!(text.contains("--resolve <id>"));
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
            step: "a review".into(),
            earlier: vec![],
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
            step: "step 1 of 2".into(),
            earlier: vec![],
            diff: "+added".into(),
        };
        let text = brief.render();
        assert!(text.contains("```diff\n+added\n```"));
        assert!(text.contains("Commit `abc123`") && text.contains("step 1 of 2"));
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
    fn empty_sections_are_left_out() {
        let brief = WorkBrief {
            agent: "coder".into(),
            instructions: None,
            item: "k3f9ab".into(),
            title: "Small".into(),
            body: String::new(),
            branch: "wf/k3f9ab".into(),
            target: "main".into(),
            dependencies: vec![],
            change_requests: vec![],
            earlier: vec![],
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

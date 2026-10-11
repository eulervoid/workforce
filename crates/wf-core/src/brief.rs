//! What a session is told about its task: the item, what was said on it, and how to report back.
//! The daemon sends it as the first prompt of a session.

use std::fmt::Write;

/// What was said on the item before: a comment, a blocker or a change request.
pub struct Earlier {
    pub by: String,
    pub kind: String,
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
    /// The short id of the item, as `wf` commands take it.
    pub item: String,
    pub title: String,
    /// The body as markdown, with the number of each step after its checkbox.
    pub body: String,
    pub branch: String,
    pub target: String,
    pub dependencies: Vec<Dependency>,
    pub earlier: Vec<Earlier>,
}

impl WorkBrief {
    pub fn render(&self) -> String {
        let mut text = format!(
            "You are working on item #{item}: {title}\n\n",
            item = self.item,
            title = self.title
        );
        if !self.body.trim().is_empty() {
            let _ = writeln!(text, "{}\n", self.body.trim());
        }
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
                        let _ = writeln!(text, ": {note}");
                    }
                    None => text.push('\n'),
                }
            }
            text.push('\n');
        }
        if !self.earlier.is_empty() {
            text.push_str("## What was said before\n\n");
            for earlier in &self.earlier {
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
        let _ = write!(
            text,
            "## How to work\n\n\
             You are in a git worktree on the branch `{branch}`, which started from `{target}`. \
             Work only here, and commit small and often.\n\
             - Check off a step of the body when it is done: `wf item check {item} <number>`.\n\
             - Say what you chose, what you rejected and why: `wf comment {item} \"...\"`. \
             Add `--step <number>` when it is about a step, and `--done` to check the step off as well. \
             Write comments that make sense on their own. They are encouraged, not required.\n\
             - When the work is finished, run `wf work submit`. Your commits are reviewed and merged from there.\n\
             - If you need an answer from someone, run `wf comment {item} \"<question>\" --block` and \
             stop; you are prompted again when it is answered.\n\
             - If you cannot make progress, run `wf work release --note \"<why>\"`.\n",
            branch = self.branch,
            target = self.target,
            item = self.item,
        );
        text
    }
}

/// The diff is part of the brief up to this many bytes; beyond that the reviewer reads it itself.
pub const MAX_DIFF_LEN: usize = 60_000;

/// The brief of a review session.
pub struct ReviewBrief {
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
        let mut text = format!(
            "You are reviewing the work on item #{item}: {title}\n\n",
            item = self.item,
            title = self.title
        );
        if !self.body.trim().is_empty() {
            let _ = writeln!(text, "{}\n", self.body.trim());
        }
        match &self.commit {
            Some(commit) => {
                let _ = write!(
                    text,
                    "## The work\n\n\
                     Commit `{commit}` is checked out here. It is meant to go into `{target}`. \
                     This is {step}.\n\n",
                    target = self.target,
                    step = self.step,
                );
                if self.diff.len() <= MAX_DIFF_LEN {
                    let _ = writeln!(text, "```diff\n{}\n```\n", self.diff.trim_end());
                } else {
                    let _ = writeln!(
                        text,
                        "The diff is too large to show here; run `git diff {}...HEAD`.\n",
                        self.target
                    );
                }
            }
            None => {
                let _ = write!(
                    text,
                    "## The work\n\n\
                     This was submitted without commits: it is an answer, or the finding that \
                     nothing has to change. What the author said is below, with the rest of the \
                     history. Nothing is checked out here. This is {step}.\n\n",
                    step = self.step,
                );
            }
        }
        if !self.earlier.is_empty() {
            text.push_str("## What was said before\n\n");
            for earlier in &self.earlier {
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
        let _ = write!(
            text,
            "## How to review\n\n\
             Read the change or the answer, and run the tests if there are any. Do not change anything. \
             Then give exactly one verdict:\n\
             - `wf review --approve --note \"...\"` if the work is good as it is.\n\
             - If it needs changes, write each change on its own: \
             `wf comment {item} \"<what to change>\" --request-change`, then run \
             `wf review --request-changes`. The item goes back for another session, which has to \
             resolve every change request before it submits again.\n\
             - `wf review --escalate --note \"...\"` if a person has to decide.\n",
            item = self.item,
        );
        text
    }
}

/// A blocker the supervisor is asked to answer or escalate.
pub struct WaitingBlocker {
    /// The short id `--resolve` takes.
    pub comment: String,
    pub by: String,
    pub text: String,
}

/// The brief of a supervise session.
pub struct SuperviseBrief {
    /// The short id of the item, as `wf` commands take it.
    pub item: String,
    pub title: String,
    pub body: String,
    pub earlier: Vec<Earlier>,
    pub blockers: Vec<WaitingBlocker>,
    /// The handle of the person to escalate to, as in `@joshua`.
    pub person: String,
}

impl SuperviseBrief {
    pub fn render(&self) -> String {
        let mut text = format!(
            "You are supervising item #{item}: {title}\n\n\
             Work on it waits for an answer. Do not change any files.\n\n",
            item = self.item,
            title = self.title
        );
        if !self.body.trim().is_empty() {
            let _ = writeln!(text, "{}\n", self.body.trim());
        }
        if !self.earlier.is_empty() {
            text.push_str("## What was said before\n\n");
            for earlier in &self.earlier {
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
        text.push_str("## Waiting on you\n\n");
        for blocker in &self.blockers {
            let _ = writeln!(
                text,
                "- {} by {}: {}",
                blocker.comment,
                blocker.by,
                blocker.text.trim()
            );
        }
        let resolve: Vec<String> = self
            .blockers
            .iter()
            .map(|blocker| format!("--resolve {}", blocker.comment))
            .collect();
        let _ = write!(
            text,
            "\n## How to answer\n\n\
             Answer everything above in one comment; your session ends with it.\n\
             - If you can answer: `wf comment {item} \"<answer>\" {resolve}`.\n\
             - If a person has to decide: `wf comment {item} \"{person} <what they have to decide>\" \
             --block {resolve}`. This hands the question to them.\n",
            item = self.item,
            resolve = resolve.join(" "),
            person = self.person,
        );
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_work_brief_names_the_item_the_branch_and_the_commands() {
        let brief = WorkBrief {
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
            earlier: vec![Earlier {
                by: "@joshua/coder".into(),
                kind: "report".into(),
                text: "tried serde first".into(),
            }],
        };
        let text = brief.render();
        assert!(text.starts_with("You are working on item #k3f9ab: Parse durations\n"));
        assert!(text.contains("- [ ] 1. write parser"));
        assert!(text.contains("- #q7x2cd Which crate? (answered): Use humantime"));
        assert!(text.contains("- @joshua/coder (report): tried serde first"));
        assert!(text.contains("`wf/k3f9ab`") && text.contains("`main`"));
        for command in [
            "wf item check k3f9ab",
            "wf comment k3f9ab",
            "wf work submit",
            "--block",
            "wf work release",
        ] {
            assert!(text.contains(command), "{command}");
        }
    }

    #[test]
    fn the_review_brief_has_the_diff_and_the_verdicts() {
        let brief = ReviewBrief {
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
            item: "k3f9ab".into(),
            title: "Small".into(),
            body: String::new(),
            branch: "wf/k3f9ab".into(),
            target: "main".into(),
            dependencies: vec![],
            earlier: vec![],
        };
        let text = brief.render();
        assert!(!text.contains("## Dependencies") && !text.contains("## What was said before"));
    }
}

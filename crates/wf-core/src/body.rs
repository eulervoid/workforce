//! The body of an item: markdown, with the task-list items at the top level of the text lifted
//! out as steps.
//!
//! A step is named by the hash of its normalized text, so an event can tick it without saying
//! where it is, and a tick for a step that was reworded in the meantime finds nothing instead of
//! the wrong step. The price is that the steps of one body must differ in text.

use std::collections::BTreeSet;
use std::fmt;

use pulldown_cmark::{Event, Options, Parser, Tag};
use serde::{Deserialize, Serialize};

use crate::refs::{self, Token};

const STEP_ID_LEN: usize = 8;

/// The hash of a step's text with runs of whitespace collapsed.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct StepId([u8; STEP_ID_LEN]);

impl StepId {
    pub fn of(text: &str) -> Self {
        let hash = blake3::hash(normalize(text).as_bytes());
        let mut id = [0; STEP_ID_LEN];
        id.copy_from_slice(&hash.as_bytes()[..STEP_ID_LEN]);
        Self(id)
    }
}

impl fmt::Display for StepId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(
            &data_encoding::BASE32_NOPAD
                .encode(&self.0)
                .to_ascii_lowercase(),
        )
    }
}

impl fmt::Debug for StepId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "StepId({self})")
    }
}

fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BodyError {
    #[error("the step \"{0}\" appears twice; steps need different text")]
    DuplicateStep(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BodyPart {
    Markdown(String),
    /// A checklist item. Its text is markdown and may continue over several lines.
    Step {
        text: String,
        done: bool,
    },
}

/// A step and where it stands in its body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Step<'a> {
    /// Counted from 1 over the steps of the body, in order.
    pub number: usize,
    pub id: StepId,
    pub text: &'a str,
    pub done: bool,
}

/// Markdown parts and steps in the order written. No two markdown parts are adjacent and no two
/// steps have the same id, so a body has one form and `parse(render(body))` is the body again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Body {
    parts: Vec<BodyPart>,
    tokens: BTreeSet<Token>,
}

impl Body {
    /// Splits `text` at the task-list items that are not nested in anything else. Items in
    /// quotes, in lists inside items and in code blocks stay markdown.
    pub fn parse(text: &str) -> Result<Self, BodyError> {
        let mut parts = Vec::new();
        let mut seen = BTreeSet::new();
        let mut cursor = 0;
        // For each open container, whether it is a list.
        let mut open_lists: Vec<bool> = Vec::new();
        let mut top_level_item = 0..0;
        for (event, range) in Parser::new_ext(text, Options::ENABLE_TASKLISTS).into_offset_iter() {
            match event {
                Event::Start(Tag::Item) if open_lists == [true] => {
                    top_level_item = range;
                    open_lists.push(false);
                }
                Event::Start(tag) => open_lists.push(matches!(tag, Tag::List(_))),
                Event::End(_) => {
                    open_lists.pop();
                }
                // The marker comes first in the item, in a paragraph if the list is loose.
                Event::TaskListMarker(done)
                    if matches!(open_lists.as_slice(), [true, false] | [true, false, false]) =>
                {
                    let step = text[range.end..top_level_item.end].trim();
                    if step.is_empty() {
                        continue;
                    }
                    if !seen.insert(StepId::of(step)) {
                        return Err(BodyError::DuplicateStep(normalize(step)));
                    }
                    push_markdown(&mut parts, &text[cursor..top_level_item.start]);
                    parts.push(BodyPart::Step {
                        text: step.to_string(),
                        done,
                    });
                    cursor = top_level_item.end;
                }
                _ => {}
            }
        }
        push_markdown(&mut parts, &text[cursor..]);
        Ok(Self::from_parts(parts))
    }

    /// All of `text` as markdown, for a body that cannot be split into steps.
    pub fn plain(text: &str) -> Self {
        let mut parts = Vec::new();
        push_markdown(&mut parts, text);
        Self::from_parts(parts)
    }

    fn from_parts(parts: Vec<BodyPart>) -> Self {
        let mut body = Self {
            parts,
            tokens: BTreeSet::new(),
        };
        body.tokens = refs::parse(&body.render());
        body
    }

    pub fn parts(&self) -> &[BodyPart] {
        &self.parts
    }

    /// The tokens do not depend on which steps are done.
    pub fn tokens(&self) -> &BTreeSet<Token> {
        &self.tokens
    }

    pub fn steps(&self) -> impl Iterator<Item = Step<'_>> {
        self.parts
            .iter()
            .filter_map(|part| match part {
                BodyPart::Step { text, done } => Some((text.as_str(), *done)),
                BodyPart::Markdown(_) => None,
            })
            .enumerate()
            .map(|(index, (text, done))| Step {
                number: index + 1,
                id: StepId::of(text),
                text,
                done,
            })
    }

    pub fn step(&self, id: StepId) -> Option<Step<'_>> {
        self.steps().find(|step| step.id == id)
    }

    /// The body with the steps ticked as `done_by_id` says; `None` keeps what is written.
    pub fn with_done(mut self, done_by_id: impl Fn(StepId) -> Option<bool>) -> Self {
        for part in &mut self.parts {
            if let BodyPart::Step { text, done } = part
                && let Some(marked) = done_by_id(StepId::of(text))
            {
                *done = marked;
            }
        }
        self
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        let mut previous_was_step = false;
        for (index, part) in self.parts.iter().enumerate() {
            let is_step = matches!(part, BodyPart::Step { .. });
            if index > 0 {
                out.push_str(if previous_was_step && is_step {
                    "\n"
                } else {
                    "\n\n"
                });
            }
            match part {
                BodyPart::Markdown(text) => out.push_str(text),
                BodyPart::Step { text, done } => {
                    out.push_str(if *done { "- [x] " } else { "- [ ] " });
                    out.push_str(text);
                }
            }
            previous_was_step = is_step;
        }
        out
    }
}

fn push_markdown(parts: &mut Vec<BodyPart>, text: &str) {
    let text = text.trim_end().trim_start_matches('\n');
    if !text.is_empty() {
        parts.push(BodyPart::Markdown(text.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(text: &str, done: bool) -> BodyPart {
        BodyPart::Step {
            text: text.into(),
            done,
        }
    }

    fn markdown(text: &str) -> BodyPart {
        BodyPart::Markdown(text.into())
    }

    fn parts(text: &str) -> Vec<BodyPart> {
        Body::parse(text).unwrap().parts().to_vec()
    }

    #[test]
    fn text_without_steps_is_one_markdown_part() {
        assert_eq!(
            parts("just prose\n\n- a plain list\n"),
            [markdown("just prose\n\n- a plain list")]
        );
        assert_eq!(parts(""), []);
    }

    #[test]
    fn task_items_become_steps_between_the_prose() {
        let body = parts("## Plan\n\n- [ ] write parser\n- [x] add tests\n\nNotes #abcd\n");
        assert_eq!(
            body,
            [
                markdown("## Plan"),
                step("write parser", false),
                step("add tests", true),
                markdown("Notes #abcd"),
            ]
        );
    }

    #[test]
    fn plain_items_between_steps_stay_markdown() {
        assert_eq!(
            parts("- [ ] a\n- plain\n- [x] b\n"),
            [step("a", false), markdown("- plain"), step("b", true)]
        );
    }

    #[test]
    fn a_step_keeps_what_is_nested_in_it() {
        assert_eq!(
            parts("- [ ] a\n  - [ ] not a step\n  - detail\n- [ ] b\n"),
            [
                step("a\n  - [ ] not a step\n  - detail", false),
                step("b", false)
            ]
        );
    }

    #[test]
    fn loose_and_ordered_lists_work() {
        assert_eq!(
            parts("1. [ ] a\n\n2. [x] b\n"),
            [step("a", false), step("b", true)]
        );
    }

    #[test]
    fn task_items_in_quotes_and_code_are_not_steps() {
        let text = "> - [ ] quoted\n\n```\n- [ ] code\n```\n";
        let body = Body::parse(text).unwrap();
        assert_eq!(body.steps().count(), 0);
        assert_eq!(body.parts().len(), 1);
    }

    #[test]
    fn an_item_without_text_is_not_a_step() {
        assert_eq!(Body::parse("- [ ]\n").unwrap().steps().count(), 0);
    }

    #[test]
    fn steps_are_numbered_from_one() {
        let body = Body::parse("intro\n\n- [ ] a\n\nmiddle\n\n- [x] b\n").unwrap();
        let numbered: Vec<_> = body.steps().map(|step| (step.number, step.text)).collect();
        assert_eq!(numbered, [(1, "a"), (2, "b")]);
    }

    #[test]
    fn duplicate_steps_are_rejected_whatever_the_spacing() {
        assert_eq!(
            Body::parse("- [ ] run   tests\n- [x] run tests\n"),
            Err(BodyError::DuplicateStep("run tests".into()))
        );
    }

    #[test]
    fn a_step_is_named_by_its_normalized_text() {
        assert_eq!(StepId::of("run  tests\n"), StepId::of("run tests"));
        assert_ne!(StepId::of("run tests"), StepId::of("run test"));
    }

    #[test]
    fn rendering_normalizes_steps_and_parsing_it_back_is_stable() {
        for text in [
            "",
            "prose only",
            "## Plan\n\n* [ ] a\n* [x] b\n\nafter",
            "- [ ] a\n  - nested\n- plain\n- [x] b\n\n\n\ntail",
            "1. [ ] a\n\n   second paragraph\n2. [x] b",
            "intro\n- [ ] a\n",
            "- [ ] a\nlazy continuation\n\n> - [ ] quoted",
        ] {
            let body = Body::parse(text).unwrap();
            let rendered = body.render();
            assert_eq!(Body::parse(&rendered).unwrap(), body, "{text:?}");
            assert_eq!(Body::parse(&rendered).unwrap().render(), rendered);
        }
    }

    #[test]
    fn rendering_writes_the_checkboxes() {
        let body = Body::parse("## Plan\n* [ ] a\n* [X] b\n").unwrap();
        assert_eq!(body.render(), "## Plan\n\n- [ ] a\n- [x] b");
    }

    #[test]
    fn tokens_come_from_prose_and_steps() {
        let body = Body::parse("see #abcd\n\n- [ ] ask @coder\n").unwrap();
        assert_eq!(body.tokens().len(), 2);
    }

    #[test]
    fn with_done_overrides_only_the_steps_it_names() {
        let body = Body::parse("- [ ] a\n- [x] b\n- [ ] c\n").unwrap();
        let marked = body.with_done(|id| {
            if id == StepId::of("a") {
                Some(true)
            } else if id == StepId::of("b") {
                Some(false)
            } else {
                None
            }
        });
        let done: Vec<_> = marked.steps().map(|step| step.done).collect();
        assert_eq!(done, [true, false, false]);
    }
}

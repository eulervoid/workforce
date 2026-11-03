//! What the footer asks for, and the keys typed while it does.

use ratatui::crossterm::event::{Event as TerminalEvent, KeyCode, KeyEvent, KeyModifiers};
use std::collections::BTreeSet;
use tui_input::backend::crossterm::EventHandler;
use tui_input::{Input, InputRequest};

use wf_core::crdt::SetChange;
use wf_core::event::{Change, CommentKind, LineAnchor};
use wf_core::id::{CommentId, EventId, ItemId, SessionId};
use wf_core::item::{ItemState, Priority};
use wf_core::name::{
    AgentName, Label, LaneName, NameError, RecoveryLevelName, Requirement, ReviewLevelName,
    TypeName,
};

use super::effect::Effect;
use super::nav::Scroll;
use super::{App, Status};
use crate::dashboard::diff::LineComment;
use crate::dashboard::snapshot::Detail;

/// Who starts work on an item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Starter {
    /// Release a draft: open it, and the dispatcher starts whoever is free.
    Dispatcher,
    Agent(AgentName),
}

/// What of an item the principal edits, in the order of the edit menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    /// The title and the body, in `$EDITOR`.
    Text,
    State,
    Priority,
    Lane,
    Review,
    Recovery,
    Labels,
    Requires,
}

impl Field {
    pub const ALL: [Field; 8] = [
        Field::Text,
        Field::State,
        Field::Priority,
        Field::Lane,
        Field::Review,
        Field::Recovery,
        Field::Labels,
        Field::Requires,
    ];

    /// The field as the edit menu names it.
    pub fn name(self) -> &'static str {
        match self {
            Field::Text => "title, body",
            Field::State => "move",
            Field::Priority => "priority",
            Field::Lane => "lane",
            Field::Review => "review",
            Field::Recovery => "recovery",
            Field::Labels => "labels",
            Field::Requires => "requires",
        }
    }
}

/// One of a list of things to pick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Choice {
    /// Who starts work on an item.
    Start {
        item: ItemId,
        starters: Vec<Starter>,
        index: usize,
    },
    /// The state to move an item to.
    State {
        item: ItemId,
        states: Vec<ItemState>,
        index: usize,
    },
    /// The edit menu: which field of an item to edit, each with what it is now.
    Field {
        item: ItemId,
        fields: Vec<(Field, String)>,
        index: usize,
    },
    /// The priority of an item; `None` for none.
    Priority {
        item: ItemId,
        priorities: Vec<Option<Priority>>,
        index: usize,
    },
    /// The lane of an item, of the policy's.
    Lane {
        item: ItemId,
        lanes: Vec<LaneName>,
        index: usize,
    },
    /// The review level of an item; `None` for that of its type, `of_type`.
    Review {
        item: ItemId,
        levels: Vec<Option<ReviewLevelName>>,
        of_type: Inherited<ReviewLevelName>,
        index: usize,
    },
    /// The recovery level of an item; `None` for that of its type, `of_type`.
    Recovery {
        item: ItemId,
        levels: Vec<Option<RecoveryLevelName>>,
        of_type: Inherited<RecoveryLevelName>,
        index: usize,
    },
}

/// What an item without a value of its own has of its type: `level` of the type `item_type`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inherited<T> {
    pub item_type: TypeName,
    pub level: Option<T>,
}

impl<T: std::fmt::Display> Inherited<T> {
    /// `task's`, or `standard (task's)`, as the edit menu and the choice say it.
    pub fn describe(&self) -> String {
        match &self.level {
            Some(level) => format!("{level} ({}'s)", self.item_type),
            None => format!("{}'s", self.item_type),
        }
    }
}

impl Choice {
    pub fn len(&self) -> usize {
        match self {
            Choice::Start { starters, .. } => starters.len(),
            Choice::State { states, .. } => states.len(),
            Choice::Field { fields, .. } => fields.len(),
            Choice::Priority { priorities, .. } => priorities.len(),
            Choice::Lane { lanes, .. } => lanes.len(),
            Choice::Review { levels, .. } => levels.len(),
            Choice::Recovery { levels, .. } => levels.len(),
        }
    }

    pub fn index(&self) -> usize {
        match self {
            Choice::Start { index, .. }
            | Choice::State { index, .. }
            | Choice::Field { index, .. }
            | Choice::Priority { index, .. }
            | Choice::Lane { index, .. }
            | Choice::Review { index, .. }
            | Choice::Recovery { index, .. } => *index,
        }
    }

    pub(super) fn index_mut(&mut self) -> &mut usize {
        match self {
            Choice::Start { index, .. }
            | Choice::State { index, .. }
            | Choice::Field { index, .. }
            | Choice::Priority { index, .. }
            | Choice::Lane { index, .. }
            | Choice::Review { index, .. }
            | Choice::Recovery { index, .. } => index,
        }
    }

    /// Whether its options are numbered, to be picked with their number.
    pub fn is_numbered(&self) -> bool {
        match self {
            Choice::Field { .. } => true,
            Choice::Start { .. }
            | Choice::State { .. }
            | Choice::Priority { .. }
            | Choice::Lane { .. }
            | Choice::Review { .. }
            | Choice::Recovery { .. } => false,
        }
    }

    /// What picking the selected option asks of the workforce.
    pub(super) fn picked(self) -> Option<Effect> {
        let change = |item, change| Effect::Change {
            item,
            changes: vec![change],
        };
        match self {
            Choice::Start {
                item,
                starters,
                index,
            } => Some(match starters.into_iter().nth(index)? {
                Starter::Dispatcher => Effect::Move {
                    item,
                    state: ItemState::Open,
                },
                Starter::Agent(agent) => Effect::StartSession { item, agent },
            }),
            Choice::State {
                item,
                states,
                index,
            } => Some(Effect::Move {
                item,
                state: states.into_iter().nth(index)?,
            }),
            Choice::Field {
                item,
                fields,
                index,
            } => Some(Effect::Edit {
                item,
                field: fields.get(index)?.0,
            }),
            Choice::Priority {
                item,
                priorities,
                index,
            } => Some(change(item, Change::Priority(*priorities.get(index)?))),
            Choice::Lane { item, lanes, index } => Some(change(
                item,
                Change::Lane(Some(lanes.into_iter().nth(index)?)),
            )),
            Choice::Review {
                item,
                levels,
                index,
                ..
            } => Some(change(
                item,
                Change::ReviewLevel(levels.into_iter().nth(index)?),
            )),
            Choice::Recovery {
                item,
                levels,
                index,
                ..
            } => Some(change(
                item,
                Change::RecoveryLevel(levels.into_iter().nth(index)?),
            )),
        }
    }
}

/// Which set of an item the set input changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Set {
    Labels,
    /// The requirements, with what the agents of the item's lane have: an item that requires
    /// anything else is never started.
    Requires {
        offered: BTreeSet<Requirement>,
    },
}

impl Set {
    pub fn name(&self) -> &'static str {
        match self {
            Set::Labels => "labels",
            Set::Requires { .. } => "requires",
        }
    }

    /// Whether `value` is one the set can hold: why not, if it is not.
    fn check(&self, value: &str) -> Result<(), NameError> {
        match self {
            Set::Labels => Label::new(value).map(drop),
            Set::Requires { .. } => Requirement::new(value).map(drop),
        }
    }

    /// Whether no agent of the item's lane has the requirement `value`.
    pub fn is_unmet(&self, value: &str) -> bool {
        match self {
            Set::Labels => false,
            Set::Requires { offered } => !offered.iter().any(|offer| offer.as_str() == value),
        }
    }

    /// The change that adds `value`, or removes it as of the adds `observed`.
    fn change(&self, value: &str, observed: Option<Vec<EventId>>) -> Option<Change> {
        fn change<T>(value: T, observed: Option<Vec<EventId>>) -> SetChange<T> {
            match observed {
                None => SetChange::Add(value),
                Some(observed) => SetChange::Remove { value, observed },
            }
        }
        // Every value was checked when it was listed or typed.
        Some(match self {
            Set::Labels => Change::Labels(change(Label::new(value).ok()?, observed)),
            Set::Requires { .. } => {
                Change::Requires(change(Requirement::new(value).ok()?, observed))
            }
        })
    }
}

/// A value of the set input: one known, or typed in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub value: String,
    /// The adds of the value the item has, which removing it removes; `None` when it has not.
    pub held: Option<Vec<EventId>>,
    /// Whether the item is to have it.
    pub marked: bool,
}

/// A row of the set input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Found<'a> {
    /// A known value that contains the search, by its place among all.
    Member(usize),
    /// The searched text, which no value is: enter creates it.
    New(&'a str),
}

/// The input for a set of an item, as one search field: the values known, those it is to have
/// marked.
#[derive(Clone, Debug)]
pub struct SetInput {
    pub item: ItemId,
    pub set: Set,
    pub members: Vec<Member>,
    /// What the values found contain; with no text, enter applies the marks.
    pub search: Input,
    /// The selected one of those found.
    pub index: usize,
}

impl SetInput {
    /// The rows the search finds: the values that contain it, and the text itself when no value
    /// is it, so that a value can be created that is part of another.
    pub fn found(&self) -> Vec<Found<'_>> {
        let search = self.search.value().trim();
        let mut found: Vec<Found> = self
            .members
            .iter()
            .enumerate()
            .filter(|(_, member)| member.value.contains(search))
            .map(|(index, _)| Found::Member(index))
            .collect();
        if !search.is_empty() && !self.members.iter().any(|member| member.value == search) {
            found.push(Found::New(search));
        }
        found
    }

    /// Marks or unmarks the value of the row `index` found, or creates it marked, and clears the
    /// search for the next, with the selection on that value. Why the value cannot be created, if
    /// it cannot.
    fn pick(&mut self, index: usize) -> Result<(), NameError> {
        let picked = match self.found().get(index).copied() {
            None => return Ok(()),
            Some(Found::Member(member)) => member,
            Some(Found::New(value)) => {
                self.set.check(value)?;
                let value = value.to_owned();
                self.members.push(Member {
                    value,
                    held: None,
                    marked: false,
                });
                self.members.len() - 1
            }
        };
        let member = &mut self.members[picked];
        member.marked = !member.marked;
        self.search.reset();
        // With no search, every value is found in its place.
        self.index = picked;
        Ok(())
    }

    /// What the marks change, as one edit; `None` when they change nothing.
    fn applied(self) -> Option<Effect> {
        let changes: Vec<Change> = self
            .members
            .into_iter()
            .filter_map(|member| match (member.held, member.marked) {
                (None, true) => self.set.change(&member.value, None),
                (Some(observed), false) => self.set.change(&member.value, Some(observed)),
                (None, false) | (Some(_), true) => None,
            })
            .collect();
        (!changes.is_empty()).then_some(Effect::Change {
            item: self.item,
            changes,
        })
    }
}

/// What the footer asks for.
#[derive(Clone, Debug)]
pub enum Prompt {
    /// Text the lines of a list must have; it applies while it is typed.
    Filter {
        list: Filtered,
        input: Input,
    },
    /// The answer to a blocker.
    Answer {
        item: ItemId,
        blocker: CommentId,
        input: Input,
    },
    /// A verdict on the work of an item: approve, request changes or look at the diff first.
    Verdict {
        item: ItemId,
    },
    Comment {
        item: ItemId,
        input: Input,
    },
    /// A comment or change request about lines of the diff of an item.
    LineComment {
        item: ItemId,
        kind: CommentKind,
        lines: LineAnchor,
        then: Written,
        input: Input,
    },
    Choose(Choice),
    /// Which values a set of an item has.
    Set(SetInput),
    /// Whether to stop a session, which costs the agent what it has not written down.
    Stop {
        session: SessionId,
    },
    /// The keys, until any key is pressed.
    Keys,
}

/// What becomes of a comment on lines once it is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Written {
    /// It is posted, as no review waits to send it with.
    Post,
    /// It is a draft of the review, in place of the draft `replacing`.
    Draft { replacing: Option<usize> },
}

/// A list that can be filtered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filtered {
    Items,
    Log,
}

/// What a key did to a prompt for a text that is posted.
enum Typed {
    /// The text goes on being typed.
    Typing,
    /// Esc, or enter with nothing typed.
    Cancelled,
    /// Ctrl-G: the text so far is to be finished in `$EDITOR`.
    InEditor(String),
    /// Enter: the text, trimmed, is posted.
    Sent(String),
}

/// Takes a key of a prompt for a text that is posted.
#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "crossterm's `KeyCode` has keys no binding uses"
)]
fn type_into(input: &mut Input, key: KeyEvent) -> Typed {
    let text = || input.value().trim().to_owned();
    match key.code {
        KeyCode::Esc => Typed::Cancelled,
        KeyCode::Char('g') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            Typed::InEditor(text())
        }
        KeyCode::Enter if input.value().trim().is_empty() => Typed::Cancelled,
        KeyCode::Enter => Typed::Sent(text()),
        _ => {
            input.handle_event(&TerminalEvent::Key(key));
            Typed::Typing
        }
    }
}

impl App {
    /// Pasted text: it goes into the prompt that is open, where a line break is a space as the
    /// prompts are one line; with none open, it is ignored rather than taken for keys. Whether
    /// there was a prompt to take it.
    pub fn paste(&mut self, text: &str) -> bool {
        let Some(
            Prompt::Filter { input, .. }
            | Prompt::Answer { input, .. }
            | Prompt::Comment { input, .. }
            | Prompt::LineComment { input, .. }
            | Prompt::Set(SetInput { search: input, .. }),
        ) = &mut self.prompt
        else {
            return false;
        };
        for character in text.replace("\r\n", "\n").chars() {
            let character = if matches!(character, '\n' | '\r') {
                ' '
            } else {
                character
            };
            input.handle(InputRequest::InsertChar(character));
        }
        if let Some(Prompt::Set(input)) = &mut self.prompt {
            input.index = 0;
        }
        if let Some(Prompt::Filter { list, input }) = &self.prompt {
            let text = input.value().to_owned();
            match list {
                Filtered::Items => self.items.filter = text,
                Filtered::Log => {
                    self.log.scroll = Scroll::END;
                    self.log.filter = text;
                }
            }
        }
        true
    }

    /// A key while the footer asks for something: the prompt it leaves open, and what it asks of
    /// the workforce.
    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "crossterm's `KeyCode` has keys no binding uses"
    )]
    pub(super) fn prompt_key(
        &mut self,
        prompt: Prompt,
        key: KeyEvent,
    ) -> (Option<Prompt>, Option<Effect>) {
        match prompt {
            Prompt::Filter { list, mut input } => {
                let filter = match list {
                    Filtered::Items => &mut self.items.filter,
                    Filtered::Log => {
                        // A line of the lines that matched is no place in those that match now.
                        self.log.scroll = Scroll::END;
                        &mut self.log.filter
                    }
                };
                match key.code {
                    KeyCode::Enter => (None, None),
                    KeyCode::Esc => {
                        filter.clear();
                        (None, None)
                    }
                    _ => {
                        input.handle_event(&TerminalEvent::Key(key));
                        *filter = input.value().to_owned();
                        (Some(Prompt::Filter { list, input }), None)
                    }
                }
            }
            Prompt::Answer {
                item,
                blocker,
                mut input,
            } => match type_into(&mut input, key) {
                Typed::Typing => (
                    Some(Prompt::Answer {
                        item,
                        blocker,
                        input,
                    }),
                    None,
                ),
                Typed::Cancelled => (None, None),
                Typed::InEditor(text) => (
                    None,
                    Some(Effect::ResolveInEditor {
                        item,
                        blocker,
                        text,
                    }),
                ),
                Typed::Sent(text) => (
                    None,
                    Some(Effect::Resolve {
                        item,
                        blocker,
                        text,
                    }),
                ),
            },
            Prompt::Comment { item, mut input } => match type_into(&mut input, key) {
                Typed::Typing => (Some(Prompt::Comment { item, input }), None),
                Typed::Cancelled => (None, None),
                Typed::InEditor(text) => (None, Some(Effect::CommentInEditor { item, text })),
                Typed::Sent(text) => (None, Some(Effect::Comment { item, text })),
            },
            Prompt::Verdict { item } => match key.code {
                KeyCode::Char('a') => {
                    let comments = self.drafts(item).to_vec();
                    if comments
                        .iter()
                        .any(|comment| comment.kind.is_change_request())
                    {
                        self.status = Some(Status::ActionFailed {
                            error: "the review has change requests: request changes, or drop them \
                                    in the diff"
                                .to_owned(),
                            at_ms: self.snapshot.now_ms,
                        });
                        return (None, None);
                    }
                    let reviewed = self.reviewed_commit(item);
                    (
                        None,
                        Some(Effect::Approve {
                            item,
                            reviewed,
                            comments,
                        }),
                    )
                }
                KeyCode::Char('r') => {
                    let comments = self.drafts(item).to_vec();
                    let reviewed = self.reviewed_commit(item);
                    (
                        None,
                        Some(Effect::RequestChanges {
                            item,
                            reviewed,
                            comments,
                        }),
                    )
                }
                // The diff is looked at before deciding.
                KeyCode::Char('D') => {
                    self.open(Detail::Diff(item));
                    (None, None)
                }
                KeyCode::Esc => (None, None),
                _ => (Some(Prompt::Verdict { item }), None),
            },
            Prompt::LineComment {
                item,
                kind,
                lines,
                then,
                mut input,
            } => {
                let outcome = type_into(&mut input, key);
                let comment = |text: String| LineComment {
                    kind,
                    lines: lines.clone(),
                    text,
                };
                match outcome {
                    Typed::Typing => {
                        let prompt = Prompt::LineComment {
                            item,
                            kind,
                            lines,
                            then,
                            input,
                        };
                        (Some(prompt), None)
                    }
                    Typed::Cancelled => (None, None),
                    Typed::InEditor(text) => (
                        None,
                        Some(Effect::LineCommentInEditor {
                            item,
                            comment: comment(text),
                            then,
                        }),
                    ),
                    Typed::Sent(text) => match then {
                        Written::Post => (
                            None,
                            Some(Effect::PostLineComment {
                                item,
                                comment: comment(text),
                            }),
                        ),
                        Written::Draft { replacing } => {
                            self.drafted(item, comment(text), replacing);
                            (None, None)
                        }
                    },
                }
            }
            Prompt::Choose(mut choice) => {
                let last = choice.len().saturating_sub(1);
                let numbered = choice.is_numbered();
                let index = choice.index_mut();
                match key.code {
                    KeyCode::Down | KeyCode::Char('j') => *index = (*index + 1).min(last),
                    KeyCode::Up | KeyCode::Char('k') => *index = index.saturating_sub(1),
                    KeyCode::Enter => return (None, choice.picked()),
                    KeyCode::Esc => return (None, None),
                    KeyCode::Char(digit @ '1'..='9') if numbered => {
                        let picked = usize::from(digit as u8 - b'1');
                        if picked <= last {
                            *index = picked;
                            return (None, choice.picked());
                        }
                    }
                    _ => {}
                }
                (Some(Prompt::Choose(choice)), None)
            }
            Prompt::Set(input) => self.set_key(input, key),
            Prompt::Stop { session } => match key.code {
                KeyCode::Char('y') => (None, Some(Effect::StopSession { session })),
                KeyCode::Char('n') | KeyCode::Esc => (None, None),
                _ => (Some(Prompt::Stop { session }), None),
            },
            Prompt::Keys => (None, None),
        }
    }

    /// A key of the set input. Letters go into the search, as a value can have any of them, so
    /// the arrows move.
    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "crossterm's `KeyCode` has keys no binding uses"
    )]
    fn set_key(&mut self, mut input: SetInput, key: KeyEvent) -> (Option<Prompt>, Option<Effect>) {
        match key.code {
            KeyCode::Esc => return (None, None),
            KeyCode::Down => {
                input.index = (input.index + 1).min(input.found().len().saturating_sub(1));
            }
            KeyCode::Up => input.index = input.index.saturating_sub(1),
            KeyCode::Enter if input.search.value().trim().is_empty() => {
                return (None, input.applied());
            }
            KeyCode::Enter => {
                let index = input.index;
                self.pick_found(&mut input, index);
            }
            _ => {
                input.search.handle_event(&TerminalEvent::Key(key));
                input.index = 0;
            }
        }
        (Some(Prompt::Set(input)), None)
    }

    /// Picks the row `index` the set input finds; a value that cannot be created says why.
    pub(super) fn pick_found(&mut self, input: &mut SetInput, index: usize) {
        if let Err(error) = input.pick(index) {
            self.status = Some(Status::ActionFailed {
                error: error.to_string(),
                at_ms: self.snapshot.now_ms,
            });
        }
    }
}

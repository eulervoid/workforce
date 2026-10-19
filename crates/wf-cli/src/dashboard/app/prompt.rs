//! What the footer asks for, and the keys typed while it does.

use ratatui::crossterm::event::{Event as TerminalEvent, KeyCode, KeyEvent, KeyModifiers};
use tui_input::backend::crossterm::EventHandler;
use tui_input::{Input, InputRequest};
use wf_core::event::{CommentKind, LineAnchor};
use wf_core::id::{CommentId, ItemId, SessionId};
use wf_core::item::ItemState;
use wf_core::name::AgentName;

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
}

impl Choice {
    pub fn len(&self) -> usize {
        match self {
            Choice::Start { starters, .. } => starters.len(),
            Choice::State { states, .. } => states.len(),
        }
    }

    pub fn index(&self) -> usize {
        match self {
            Choice::Start { index, .. } | Choice::State { index, .. } => *index,
        }
    }

    pub(super) fn index_mut(&mut self) -> &mut usize {
        match self {
            Choice::Start { index, .. } | Choice::State { index, .. } => index,
        }
    }

    /// What picking the selected option asks of the workforce.
    pub(super) fn picked(self) -> Option<Effect> {
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
        }
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

impl App {
    /// Pasted text: it goes into the prompt that is open, where a line break is a space as the
    /// prompts are one line; with none open, it is ignored rather than taken for keys. Whether
    /// there was a prompt to take it.
    pub fn paste(&mut self, text: &str) -> bool {
        let Some(
            Prompt::Filter { input, .. }
            | Prompt::Answer { input, .. }
            | Prompt::Comment { input, .. }
            | Prompt::LineComment { input, .. },
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
            } => match key.code {
                KeyCode::Esc => (None, None),
                KeyCode::Char('g') if key.modifiers.contains(KeyModifiers::CONTROL) => (
                    None,
                    Some(Effect::ResolveInEditor {
                        item,
                        blocker,
                        text: input.value().trim().to_owned(),
                    }),
                ),
                KeyCode::Enter if input.value().trim().is_empty() => (None, None),
                KeyCode::Enter => {
                    let text = input.value().trim().to_owned();
                    (
                        None,
                        Some(Effect::Resolve {
                            item,
                            blocker,
                            text,
                        }),
                    )
                }
                _ => {
                    input.handle_event(&TerminalEvent::Key(key));
                    (
                        Some(Prompt::Answer {
                            item,
                            blocker,
                            input,
                        }),
                        None,
                    )
                }
            },
            Prompt::Comment { item, mut input } => match key.code {
                KeyCode::Esc => (None, None),
                KeyCode::Char('g') if key.modifiers.contains(KeyModifiers::CONTROL) => (
                    None,
                    Some(Effect::CommentInEditor {
                        item,
                        text: input.value().trim().to_owned(),
                    }),
                ),
                KeyCode::Enter if input.value().trim().is_empty() => (None, None),
                KeyCode::Enter => {
                    let text = input.value().trim().to_owned();
                    (None, Some(Effect::Comment { item, text }))
                }
                _ => {
                    input.handle_event(&TerminalEvent::Key(key));
                    (Some(Prompt::Comment { item, input }), None)
                }
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
                let comment = |input: &Input| LineComment {
                    kind,
                    lines: lines.clone(),
                    text: input.value().trim().to_owned(),
                };
                match key.code {
                    KeyCode::Esc => (None, None),
                    KeyCode::Char('g') if key.modifiers.contains(KeyModifiers::CONTROL) => (
                        None,
                        Some(Effect::LineCommentInEditor {
                            item,
                            comment: comment(&input),
                            then,
                        }),
                    ),
                    KeyCode::Enter if input.value().trim().is_empty() => (None, None),
                    KeyCode::Enter => match then {
                        Written::Post => (
                            None,
                            Some(Effect::PostLineComment {
                                item,
                                comment: comment(&input),
                            }),
                        ),
                        Written::Draft { replacing } => {
                            self.drafted(item, comment(&input), replacing);
                            (None, None)
                        }
                    },
                    _ => {
                        input.handle_event(&TerminalEvent::Key(key));
                        let prompt = Prompt::LineComment {
                            item,
                            kind,
                            lines,
                            then,
                            input,
                        };
                        (Some(prompt), None)
                    }
                }
            }
            Prompt::Choose(mut choice) => {
                let last = choice.len().saturating_sub(1);
                let index = choice.index_mut();
                match key.code {
                    KeyCode::Down | KeyCode::Char('j') => *index = (*index + 1).min(last),
                    KeyCode::Up | KeyCode::Char('k') => *index = index.saturating_sub(1),
                    KeyCode::Enter => return (None, choice.picked()),
                    KeyCode::Esc => return (None, None),
                    _ => {}
                }
                (Some(Prompt::Choose(choice)), None)
            }
            Prompt::Stop { session } => match key.code {
                KeyCode::Char('y') => (None, Some(Effect::StopSession { session })),
                KeyCode::Char('n') | KeyCode::Esc => (None, None),
                _ => (Some(Prompt::Stop { session }), None),
            },
            Prompt::Keys => (None, None),
        }
    }
}

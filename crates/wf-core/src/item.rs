//! The item model: the single universal document behind tasks, questions and decisions.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Priority {
    P0,
    P1,
    P2,
    P3,
}

impl Priority {
    /// What `wf item add` gives an item of a type that has priorities when none is asked for.
    pub const DEFAULT: Priority = Priority::P2;
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("priority must be one of P0, P1, P2, P3")]
pub struct PriorityError;

impl fmt::Display for Priority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Priority::P0 => "P0",
            Priority::P1 => "P1",
            Priority::P2 => "P2",
            Priority::P3 => "P3",
        })
    }
}

impl FromStr for Priority {
    type Err = PriorityError;

    fn from_str(text: &str) -> Result<Self, PriorityError> {
        match text.to_ascii_uppercase().as_str() {
            "P0" => Ok(Priority::P0),
            "P1" => Ok(Priority::P1),
            "P2" => Ok(Priority::P2),
            "P3" => Ok(Priority::P3),
            _ => Err(PriorityError),
        }
    }
}

use std::collections::{BTreeMap, BTreeSet};

use crate::body::{Body, StepId};
use crate::crdt::{AddWinsSet, Lww, Stamp};
use crate::hlc::Hlc;
use crate::id::{EventId, ItemId, SessionId};
use crate::identity::Actor;
use crate::name::{Label, LaneName, Requirement, ReviewName, StateName, TypeName};
use crate::policy::{Policy, StateCategory};
use crate::refs::Token;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NoteKind {
    Comment,
    /// The note attached to a state transition.
    Transition,
    /// A report by a session of the item. The step may no longer exist in the body.
    Report {
        session: SessionId,
        step: Option<StepId>,
    },
}

impl NoteKind {
    pub fn name(&self) -> &'static str {
        match self {
            NoteKind::Comment => "comment",
            NoteKind::Transition => "transition",
            NoteKind::Report { .. } => "report",
        }
    }
}

/// What the fold keeps of a comment or transition note. The text itself stays in the log and is
/// loaded on demand by event id; the fold keeps who wrote it and the tokens found in it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Note {
    pub kind: NoteKind,
    pub by: Actor,
    pub at: Hlc,
    pub tokens: BTreeSet<Token>,
}

/// Where a reference or mention was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Origin {
    Body,
    Note(EventId),
}

/// The effective state of an item under the current policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Standing<'a> {
    Known {
        state: &'a StateName,
        state_category: StateCategory,
    },
    /// The item type or state was removed from the policy. The item keeps its state until moved.
    Unknown { state: Option<&'a StateName> },
}

impl Standing<'_> {
    pub fn state_category(&self) -> Option<StateCategory> {
        match self {
            Standing::Known { state_category, .. } => Some(*state_category),
            Standing::Unknown { .. } => None,
        }
    }
}

/// A folded item. Registers merge by [`Stamp`], sets are add-wins; see `crdt`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub(crate) id: ItemId,
    pub(crate) item_type: TypeName,
    pub(crate) author: Actor,
    pub(crate) created: Hlc,
    pub(crate) updated: Hlc,
    pub(crate) title: Lww<String>,
    pub(crate) body: Lww<Body>,
    /// Ticks of steps. One counts only while it is newer than the body it was made against.
    pub(crate) step_marks: BTreeMap<StepId, Lww<bool>>,
    /// `None` until the first transition: the type's initial state under the current policy.
    pub(crate) state: Lww<Option<StateName>>,
    pub(crate) priority: Lww<Option<Priority>>,
    pub(crate) lane: Lww<Option<LaneName>>,
    pub(crate) review: Lww<Option<ReviewName>>,
    pub(crate) labels: AddWinsSet<Label>,
    pub(crate) requires: AddWinsSet<Requirement>,
    pub(crate) depends_on: AddWinsSet<ItemId>,
    pub(crate) supersedes: Lww<Option<ItemId>>,
    pub(crate) archived: Lww<Option<Hlc>>,
    pub(crate) notes: BTreeMap<EventId, Note>,
}

impl Item {
    pub fn id(&self) -> ItemId {
        self.id
    }

    pub fn item_type(&self) -> &TypeName {
        &self.item_type
    }

    pub fn author(&self) -> &Actor {
        &self.author
    }

    pub fn created(&self) -> Hlc {
        self.created
    }

    pub fn updated(&self) -> Hlc {
        self.updated
    }

    pub fn title(&self) -> &str {
        self.title.value()
    }

    /// The body with the steps ticked as of the latest ticks.
    pub fn body(&self) -> Body {
        let body_stamp = self.body.stamp();
        self.body.value().clone().with_done(|step| {
            self.step_marks
                .get(&step)
                .filter(|mark| mark.stamp() > body_stamp)
                .map(|mark| *mark.value())
        })
    }

    pub fn body_tokens(&self) -> &BTreeSet<Token> {
        self.body.value().tokens()
    }

    pub fn priority(&self) -> Option<Priority> {
        *self.priority.value()
    }

    pub fn lane(&self) -> Option<&LaneName> {
        self.lane.value().as_ref()
    }

    /// The review level the item needs, if any.
    pub fn review(&self) -> Option<&ReviewName> {
        self.review.value().as_ref()
    }

    pub fn labels(&self) -> &AddWinsSet<Label> {
        &self.labels
    }

    pub fn requires(&self) -> &AddWinsSet<Requirement> {
        &self.requires
    }

    pub fn depends_on(&self) -> &AddWinsSet<ItemId> {
        &self.depends_on
    }

    pub fn supersedes(&self) -> Option<ItemId> {
        *self.supersedes.value()
    }

    pub fn archived_at(&self) -> Option<Hlc> {
        *self.archived.value()
    }

    pub fn notes(&self) -> &BTreeMap<EventId, Note> {
        &self.notes
    }

    /// The lane the item is routed to: its own, else its type's.
    pub fn effective_lane<'a>(&'a self, policy: &'a Policy) -> Option<&'a LaneName> {
        self.lane().or_else(|| {
            policy
                .type_config(&self.item_type)
                .map(|config| &config.lane)
        })
    }

    pub fn standing<'a>(&'a self, policy: &'a Policy) -> Standing<'a> {
        let explicit = self.state.value().as_ref();
        let Some(config) = policy.type_config(&self.item_type) else {
            return Standing::Unknown { state: explicit };
        };
        let state = explicit.unwrap_or(&config.initial);
        match config.state_category_of(state) {
            Some(state_category) => Standing::Known {
                state,
                state_category,
            },
            None => Standing::Unknown { state: Some(state) },
        }
    }

    pub(crate) fn touch(&mut self, hlc: Hlc) {
        self.updated = self.updated.max(hlc);
    }

    pub(crate) fn new(
        id: ItemId,
        author: Actor,
        stamp: Stamp,
        created: &crate::event::ItemCreated,
    ) -> Self {
        let mut labels = AddWinsSet::default();
        for label in &created.labels {
            labels.add(label.clone(), stamp);
        }
        let mut requires = AddWinsSet::default();
        for requirement in &created.requires {
            requires.add(requirement.clone(), stamp);
        }
        Self {
            id,
            item_type: created.item_type.clone(),
            author,
            created: stamp.hlc,
            updated: stamp.hlc,
            title: Lww::new(created.title.clone(), stamp),
            body: Lww::new(body_of(&created.body), stamp),
            step_marks: BTreeMap::new(),
            state: Lww::new(None, stamp),
            priority: Lww::new(created.priority, stamp),
            lane: Lww::new(created.lane.clone(), stamp),
            review: Lww::new(created.review.clone(), stamp),
            labels,
            requires,
            depends_on: AddWinsSet::default(),
            supersedes: Lww::new(None, stamp),
            archived: Lww::new(None, stamp),
            notes: BTreeMap::new(),
        }
    }
}

/// What the fold makes of body text. Authors cannot write a body with duplicate steps, so one
/// that has them comes from a peer that does not check; its steps stay plain markdown.
pub(crate) fn body_of(text: &str) -> Body {
    Body::parse(text).unwrap_or_else(|_| Body::plain(text))
}

/// The few fields kept visible for an archived item.
#[derive(Clone, Copy, Debug)]
pub struct ArchivedStub<'a> {
    item: &'a Item,
    archived_at: Hlc,
}

impl<'a> ArchivedStub<'a> {
    pub fn id(&self) -> ItemId {
        self.item.id
    }

    pub fn item_type(&self) -> &'a TypeName {
        &self.item.item_type
    }

    pub fn title(&self) -> &'a str {
        self.item.title.value()
    }

    pub fn archived_at(&self) -> Hlc {
        self.archived_at
    }

    pub fn standing(&self, policy: &'a Policy) -> Standing<'a> {
        self.item.standing(policy)
    }
}

/// What a reader may see of an item. Archived items expose only a stub; their body, notes and
/// history are loaded from the log on demand, so code cannot read them by accident.
#[derive(Clone, Copy, Debug)]
pub enum ItemView<'a> {
    Live(&'a Item),
    Archived(ArchivedStub<'a>),
}

impl<'a> ItemView<'a> {
    pub fn of(item: &'a Item) -> Self {
        match item.archived_at() {
            Some(archived_at) => ItemView::Archived(ArchivedStub { item, archived_at }),
            None => ItemView::Live(item),
        }
    }

    pub fn id(&self) -> ItemId {
        match self {
            ItemView::Live(item) => item.id,
            ItemView::Archived(stub) => stub.id(),
        }
    }

    pub fn item_type(&self) -> &'a TypeName {
        match self {
            ItemView::Live(item) => &item.item_type,
            ItemView::Archived(stub) => stub.item_type(),
        }
    }

    pub fn title(&self) -> &'a str {
        match self {
            ItemView::Live(item) => item.title.value(),
            ItemView::Archived(stub) => stub.title(),
        }
    }

    pub fn standing(&self, policy: &'a Policy) -> Standing<'a> {
        match self {
            ItemView::Live(item) => item.standing(policy),
            ItemView::Archived(stub) => stub.standing(policy),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_parses_case_insensitively() {
        assert_eq!("p1".parse(), Ok(Priority::P1));
        assert_eq!("P3".parse(), Ok(Priority::P3));
        assert_eq!("P9".parse::<Priority>(), Err(PriorityError));
        assert_eq!(Priority::P0.to_string(), "P0");
    }
}

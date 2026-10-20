use serde::Serialize;
use wf_core::graph::DependencyGraph;
use wf_core::id::CommentId;
use wf_core::search::{Hit, Match, MatchedField};
use wf_core::state::State;

use super::{ActorView, ItemSummary, actor_view, summary};

/// An item found by `wf item search`, with where the words occur in it.
#[derive(Clone, Serialize)]
pub struct SearchHitView {
    pub item: ItemSummary,
    /// Empty when only the item's id matched.
    pub matches: Vec<MatchView>,
}

#[derive(Clone, Serialize)]
pub struct MatchView {
    pub field: MatchFieldView,
    /// The comment, for a match in a comment.
    pub comment: Option<CommentId>,
    /// Who wrote the comment, for a match in a comment.
    pub by: Option<ActorView>,
    /// The first line of the field with a word in it, cropped around the first word.
    pub text: String,
    /// The words in `text`, as byte ranges from the start to the end of each.
    pub highlights: Vec<[usize; 2]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchFieldView {
    Title,
    Body,
    Comment,
}

pub fn search_hit_view(state: &State, graph: &DependencyGraph, hit: &Hit<'_>) -> SearchHitView {
    SearchHitView {
        item: summary(state, graph, hit.item),
        matches: hit
            .matches
            .iter()
            .map(|found| match_view(state, found))
            .collect(),
    }
}

fn match_view(state: &State, found: &Match) -> MatchView {
    let (field, comment, by) = match &found.field {
        MatchedField::Title => (MatchFieldView::Title, None, None),
        MatchedField::Body => (MatchFieldView::Body, None, None),
        MatchedField::Comment { id, by } => (
            MatchFieldView::Comment,
            Some(*id),
            Some(actor_view(state, by)),
        ),
    };
    MatchView {
        field,
        comment,
        by,
        text: found.context.clone(),
        highlights: found
            .highlights
            .iter()
            .map(|range| [range.start, range.end])
            .collect(),
    }
}

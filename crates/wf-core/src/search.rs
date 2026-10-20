//! Finding items by text: a plain scan over the folded state, no index.
//!
//! The comment texts are not in the state (the log keeps them), so the caller passes the ones
//! it loaded.

use std::collections::HashMap;
use std::ops::Range;

use crate::id::{CommentId, HashId, ItemId, MIN_PREFIX_LEN};
use crate::identity::Actor;
use crate::item::{Item, ItemEntry};
use crate::state::State;

/// How many characters of a line a match shows.
const CONTEXT_WIDTH: usize = 120;
/// How many characters before the first word of a long line stay in view.
const CONTEXT_LEAD: usize = 20;
const ELLIPSIS: &str = "…";

/// The words to look for. An item matches when each word occurs in it, in any of its fields, or
/// is the start of its id (written with or without `#`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Query {
    words: Vec<String>,
}

impl Query {
    /// `None` when `text` has no words.
    pub fn parse(text: &str) -> Option<Query> {
        let words: Vec<String> = text.split_whitespace().map(fold_case).collect();
        (!words.is_empty()).then_some(Query { words })
    }

    pub fn words(&self) -> &[String] {
        &self.words
    }

    fn matches_id(word: &str, id: ItemId) -> bool {
        let prefix = word.strip_prefix('#').unwrap_or(word);
        prefix.len() >= MIN_PREFIX_LEN && id.to_base32().starts_with(prefix)
    }
}

/// Which items a search looks through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchScope {
    Live,
    LiveAndArchived,
}

/// Where in an item a word was found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MatchedField {
    Title,
    Body,
    Comment { id: CommentId, by: Actor },
}

/// The line of a field where the words occur.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Match {
    pub field: MatchedField,
    /// The first line of the field with a word in it, cropped around the first word.
    pub context: String,
    /// The words in `context`, as byte ranges, in order and not overlapping.
    pub highlights: Vec<Range<usize>>,
}

/// An item that has all the words of a query.
#[derive(Clone, Debug)]
pub struct Hit<'a> {
    pub item: ItemEntry<'a>,
    /// The title first, then the body, then the comments in the order they were written. Empty
    /// when only the id matched.
    pub matches: Vec<Match>,
}

/// How strongly an item matches, the strongest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Strength {
    Id,
    Title,
    Body,
    Comment,
}

/// The items with all the words of `query`: those whose title matches first, then those whose
/// body does, then those with a match in comments only. An item named by the id in the query
/// comes before all of them. Within each, the order of `wf item list`: live items by priority,
/// then creation, archived ones after them by when they were archived.
pub fn search<'a>(
    state: &'a State,
    query: &Query,
    comment_texts: &HashMap<CommentId, String>,
    scope: SearchScope,
) -> Vec<Hit<'a>> {
    let mut found: Vec<(Strength, &Item, Vec<Match>)> = state
        .items_including_archived()
        .filter(|item| scope == SearchScope::LiveAndArchived || item.archived_at().is_none())
        .filter_map(|item| {
            let matches = matches_of(item, query, comment_texts)?;
            let strength = match matches.first().map(|first| &first.field) {
                None => Strength::Id,
                Some(MatchedField::Title) => Strength::Title,
                Some(MatchedField::Body) => Strength::Body,
                Some(MatchedField::Comment { .. }) => Strength::Comment,
            };
            Some((strength, item, matches))
        })
        .collect();
    found.sort_by(|(first_strength, first, _), (second_strength, second, _)| {
        first_strength.cmp(second_strength).then_with(|| {
            match (first.archived_at(), second.archived_at()) {
                (None, None) => first.cmp_by_priority(second),
                (None, Some(_)) => std::cmp::Ordering::Less,
                (Some(_), None) => std::cmp::Ordering::Greater,
                (Some(first_at), Some(second_at)) => {
                    (first_at, first.id()).cmp(&(second_at, second.id()))
                }
            }
        })
    });
    found
        .into_iter()
        .map(|(_, item, matches)| Hit {
            item: ItemEntry::of(item),
            matches,
        })
        .collect()
}

/// The matches of `item`, or `None` when some word occurs nowhere in it.
fn matches_of(
    item: &Item,
    query: &Query,
    comment_texts: &HashMap<CommentId, String>,
) -> Option<Vec<Match>> {
    let mut fields: Vec<(MatchedField, String)> = vec![
        (MatchedField::Title, item.title().to_string()),
        (MatchedField::Body, item.body().render()),
    ];
    let mut comments: Vec<_> = item.comments().iter().collect();
    comments.sort_by_key(|(id, comment)| (comment.at, **id));
    for (id, comment) in comments {
        if let Some(text) = comment_texts.get(id) {
            fields.push((
                MatchedField::Comment {
                    id: *id,
                    by: comment.by.clone(),
                },
                text.clone(),
            ));
        }
    }
    let folded: Vec<String> = fields.iter().map(|(_, text)| fold_case(text)).collect();
    let every_word_occurs = query.words().iter().all(|word| {
        Query::matches_id(word, item.id()) || folded.iter().any(|text| text.contains(word.as_str()))
    });
    if !every_word_occurs {
        return None;
    }
    Some(
        fields
            .into_iter()
            .zip(&folded)
            .filter_map(|((field, text), folded)| {
                let (context, highlights) = context_of(&text, folded, query.words())?;
                Some(Match {
                    field,
                    context,
                    highlights,
                })
            })
            .collect(),
    )
}

/// Lowercases without changing any character's length in bytes, so that offsets in the result
/// are offsets in `text`.
fn fold_case(text: &str) -> String {
    text.chars()
        .map(|character| {
            let mut lower = character.to_lowercase();
            match (lower.next(), lower.next()) {
                (Some(single), None) if single.len_utf8() == character.len_utf8() => single,
                _ => character,
            }
        })
        .collect()
}

/// The first line with a word in it, trimmed and cropped, and where the words are in it.
fn context_of(text: &str, folded: &str, words: &[String]) -> Option<(String, Vec<Range<usize>>)> {
    let mut line_start = 0;
    for line in text.split('\n') {
        let line_end = line_start + line.len();
        let folded_line = &folded[line_start..line_end];
        let mut ranges: Vec<Range<usize>> = words
            .iter()
            .flat_map(|word| {
                folded_line
                    .match_indices(word.as_str())
                    .map(|(start, found)| start..start + found.len())
            })
            .collect();
        line_start = line_end + 1;
        if ranges.is_empty() {
            continue;
        }
        ranges.sort_by_key(|range| (range.start, range.end));
        let mut merged: Vec<Range<usize>> = Vec::new();
        for range in ranges {
            match merged.last_mut() {
                Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
                _ => merged.push(range),
            }
        }
        return Some(crop(line, merged));
    }
    None
}

/// Trims `line` and shows at most [`CONTEXT_WIDTH`] characters of it around the first highlight.
fn crop(line: &str, highlights: Vec<Range<usize>>) -> (String, Vec<Range<usize>>) {
    let trimmed_start = line.len() - line.trim_start().len();
    let line = line.trim();
    let highlights: Vec<Range<usize>> = highlights
        .into_iter()
        .map(|range| range.start - trimmed_start..range.end - trimmed_start)
        .collect();
    if line.chars().count() <= CONTEXT_WIDTH {
        return (line.to_string(), highlights);
    }
    let first = highlights[0].start;
    let from = line[..first]
        .char_indices()
        .rev()
        .nth(CONTEXT_LEAD.saturating_sub(1))
        .map_or(0, |(index, _)| index);
    let to = line[from..]
        .char_indices()
        .nth(CONTEXT_WIDTH)
        .map_or(line.len(), |(index, _)| from + index);
    let lead = if from > 0 { ELLIPSIS } else { "" };
    let tail = if to < line.len() { ELLIPSIS } else { "" };
    let context = format!("{lead}{}{tail}", &line[from..to]);
    let highlights = highlights
        .into_iter()
        .filter(|range| range.start < to)
        .map(|range| range.start - from + lead.len()..range.end.min(to) - from + lead.len())
        .collect();
    (context, highlights)
}

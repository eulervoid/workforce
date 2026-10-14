//! The links in the text on screen: the ids and names drawn there that resolve against the
//! snapshot, found where they ended up after wrapping and styling.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use wf_core::id::{HashId, ItemId, SHORT_COMMENT_ID_LEN};

use super::app::{Spot, Target};
use super::snapshot::{Detail, Links};

/// A run of characters that could be an id or a name, and the columns it was drawn at.
struct Word {
    text: String,
    /// The column of its first character.
    start: u16,
    /// The column after its last character.
    end: u16,
}

/// The links on the rows of `area` as drawn into `buffer`. A comment id or a commit is about
/// `item`, the item of the text, or else an item linked on the same row. A link to `shown`, what
/// the text is about, is left out.
pub fn find(
    buffer: &Buffer,
    area: Rect,
    item: Option<ItemId>,
    shown: Option<&Detail>,
    links: &Links,
) -> Vec<Spot> {
    let area = area.intersection(buffer.area);
    let mut spots = Vec::new();
    for row in area.top()..area.bottom() {
        let words = words(buffer, area, row);
        let row_items = words
            .iter()
            .filter_map(|word| item_of(word, links).map(|(id, _)| id));
        let about: Vec<ItemId> = item.into_iter().chain(row_items).collect();
        for (index, word) in words.iter().enumerate() {
            let before = |back: usize| {
                index
                    .checked_sub(back)
                    .map(|index| words[index].text.as_str())
            };
            let Some(target) = resolve(word, before(1), before(2), &about, links) else {
                continue;
            };
            if matches!(&target, Target::Open { detail, .. } if Some(detail) == shown) {
                continue;
            }
            spots.push(Spot {
                target,
                area: Rect::new(word.start, row, word.end - word.start, 1),
            });
        }
    }
    spots
}

/// Whether `character` can be part of an id or a name: `#k3f9ab`, `@ada/grace/s4k2ab`.
fn is_word_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '#' | '@' | '/' | '-' | '_')
}

/// The words of a row, with their columns.
fn words(buffer: &Buffer, area: Rect, row: u16) -> Vec<Word> {
    let mut words = Vec::new();
    let mut current: Option<Word> = None;
    for column in area.left()..area.right() {
        let symbol = buffer[(column, row)].symbol();
        let mut characters = symbol.chars();
        let is_word = matches!(
            (characters.next(), characters.next()),
            (Some(character), None) if is_word_character(character)
        );
        match (&mut current, is_word) {
            (Some(word), true) => {
                word.text.push_str(symbol);
                word.end = column + 1;
            }
            (None, true) => {
                current = Some(Word {
                    text: symbol.to_owned(),
                    start: column,
                    end: column + 1,
                });
            }
            (_, false) => words.extend(current.take()),
        }
    }
    words.extend(current);
    words
}

/// The item `#k3f9ab` names, with how it is written; also `#k3f9ab/znft`, a comment of it.
fn item_of(word: &Word, links: &Links) -> Option<(ItemId, String)> {
    let short = word.text.strip_prefix('#')?.split('/').next()?;
    let item = links.items.get(short)?;
    Some((*item, format!("#{short}")))
}

/// What `word` links to, if it resolves, after the words `previous` and `before_previous`.
fn resolve(
    word: &Word,
    previous: Option<&str>,
    before_previous: Option<&str>,
    about: &[ItemId],
    links: &Links,
) -> Option<Target> {
    let text = word.text.as_str();
    if text.starts_with('#') {
        let (item, name) = item_of(word, links)?;
        return Some(Target::Open {
            detail: Detail::Item(item),
            name,
        });
    }
    if text.starts_with('@') {
        // `@ada` is a principal, which has no detail.
        let detail = match text.matches('/').count() {
            1 => Detail::Agent(links.agents.get(text)?.clone()),
            2 => Detail::Session(*links.sessions.get(text)?),
            _ => return None,
        };
        return Some(Target::Open {
            detail,
            name: text.to_owned(),
        });
    }
    if text.len() == SHORT_COMMENT_ID_LEN {
        let item = about
            .iter()
            .find(|item| links.comments.contains(&(**item, text.to_owned())))?;
        return Some(Target::Open {
            detail: Detail::Item(*item),
            name: format!("#{}", item.short()),
        });
    }
    let after_commit_word = matches!(
        (before_previous, previous),
        (Some("merged"), Some("as")) | (_, Some("submitted" | "merged"))
    );
    let is_commit = (7..=40).contains(&text.len())
        && text
            .chars()
            .all(|character| character.is_ascii_digit() || matches!(character, 'a'..='f'));
    if after_commit_word && is_commit {
        return Some(Target::Diff {
            item: *about.first()?,
            commit: text.to_owned(),
        });
    }
    None
}

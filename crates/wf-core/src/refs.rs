//! Derived references (`#item`) and mentions (`@principal/agent/session`) in Markdown text.
//!
//! The parser only finds candidate tokens, skipping code. Whether a token resolves to an item or
//! principal depends on the whole state, so resolution happens at query time in `State`.
//! Bump [`PARSER_VERSION`] whenever parsing changes; snapshots from older parsers are discarded.

use std::collections::BTreeSet;

use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use serde::{Deserialize, Serialize};

use crate::id::MIN_PREFIX_LEN;

pub const PARSER_VERSION: u16 = 1;

/// `@joshua`, `@joshua/coder` or `@joshua/coder/s4k2`, as written.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Mention {
    /// One segment. A principal handle, or a short form of an agent or session name.
    Short(String),
    Agent {
        principal: String,
        agent: String,
    },
    Session {
        principal: String,
        agent: String,
        session: String,
    },
}

impl std::fmt::Display for Mention {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Mention::Short(name) => write!(formatter, "@{name}"),
            Mention::Agent { principal, agent } => write!(formatter, "@{principal}/{agent}"),
            Mention::Session {
                principal,
                agent,
                session,
            } => write!(formatter, "@{principal}/{agent}/{session}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Token {
    /// A candidate item id prefix, lowercase base32, at least [`MIN_PREFIX_LEN`] long.
    ItemRef(String),
    Mention(Mention),
}

pub fn parse(markdown: &str) -> BTreeSet<Token> {
    let mut tokens = BTreeSet::new();
    let mut prose = String::new();
    let mut code_block_depth = 0usize;

    for event in Parser::new(markdown) {
        match event {
            Event::Text(text) if code_block_depth == 0 => prose.push_str(&text),
            Event::Text(_) => {}
            Event::Start(Tag::CodeBlock(_)) => {
                scan(&std::mem::take(&mut prose), &mut tokens);
                code_block_depth += 1;
            }
            Event::End(TagEnd::CodeBlock) => code_block_depth -= 1,
            Event::SoftBreak | Event::HardBreak => prose.push('\n'),
            _ => scan(&std::mem::take(&mut prose), &mut tokens),
        }
    }
    scan(&prose, &mut tokens);
    tokens
}

fn is_boundary_before(previous: Option<char>) -> bool {
    previous.is_none_or(|character| {
        character.is_whitespace() || matches!(character, '(' | '[' | '{' | '"' | '\'')
    })
}

fn is_base32(character: char) -> bool {
    character.is_ascii_lowercase() || ('2'..='7').contains(&character)
}

fn scan(text: &str, tokens: &mut BTreeSet<Token>) {
    let characters: Vec<(usize, char)> = text.char_indices().collect();
    let mut index = 0;
    while index < characters.len() {
        let (start, character) = characters[index];
        let previous = index.checked_sub(1).map(|before| characters[before].1);
        if matches!(character, '#' | '@') && is_boundary_before(previous) {
            let rest = &text[start + 1..];
            let (token, consumed) = match character {
                '#' => parse_item_ref(rest),
                _ => parse_mention(rest),
            };
            if let Some(token) = token {
                tokens.insert(token);
            }
            index += 1 + rest[..consumed].chars().count();
        } else {
            index += 1;
        }
    }
}

fn run_length(text: &str, allowed: impl Fn(char) -> bool) -> usize {
    text.char_indices()
        .find(|(_, character)| !allowed(*character))
        .map_or(text.len(), |(position, _)| position)
}

fn parse_item_ref(rest: &str) -> (Option<Token>, usize) {
    let length = run_length(rest, |character| {
        character.is_ascii_lowercase() || character.is_ascii_digit()
    });
    let candidate = &rest[..length];
    let valid = candidate.len() >= MIN_PREFIX_LEN && candidate.chars().all(is_base32);
    (valid.then(|| Token::ItemRef(candidate.to_string())), length)
}

fn parse_mention(rest: &str) -> (Option<Token>, usize) {
    let length = run_length(rest, |character| {
        character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || matches!(character, '-' | '_' | '/')
    });
    let candidate = rest[..length].trim_end_matches(['/', '-', '_']);
    let segments: Vec<&str> = candidate.split('/').collect();
    let well_formed = segments.iter().all(|segment| {
        segment
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
    });
    if !well_formed {
        return (None, length);
    }
    let owned = |segment: &&str| segment.to_string();
    let mention = match segments.as_slice() {
        [name] => Mention::Short(owned(name)),
        [principal, agent] => Mention::Agent {
            principal: owned(principal),
            agent: owned(agent),
        },
        [principal, agent, session] => Mention::Session {
            principal: owned(principal),
            agent: owned(agent),
            session: owned(session),
        },
        _ => return (None, length),
    };
    (Some(Token::Mention(mention)), length)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(prefix: &str) -> Token {
        Token::ItemRef(prefix.to_string())
    }

    fn short(name: &str) -> Token {
        Token::Mention(Mention::Short(name.to_string()))
    }

    #[test]
    fn finds_refs_and_mentions_in_prose() {
        let tokens = parse("see #q7x2, @coder please keep it small. cc @joshua/reviewer");
        assert_eq!(
            tokens,
            BTreeSet::from([
                item("q7x2"),
                short("coder"),
                Token::Mention(Mention::Agent {
                    principal: "joshua".into(),
                    agent: "reviewer".into()
                })
            ])
        );
    }

    #[test]
    fn finds_session_mentions() {
        let tokens = parse("@joshua/coder/s4k2 is stuck");
        assert_eq!(
            tokens,
            BTreeSet::from([Token::Mention(Mention::Session {
                principal: "joshua".into(),
                agent: "coder".into(),
                session: "s4k2".into()
            })])
        );
    }

    #[test]
    fn skips_code() {
        let text =
            "inline `#abcd @nobody` and\n\n```\n#efgh @ghost\n```\n\n    #ijkl\n\nreal #mnop";
        assert_eq!(parse(text), BTreeSet::from([item("mnop")]));
    }

    #[test]
    fn ignores_non_tokens() {
        let text = "#ab too short, #abc1 bad digits, mail me@example.com, \
                    https://example.com/#anchor, @-bad, @/x, #";
        assert_eq!(parse(text), BTreeSet::new());
    }

    #[test]
    fn works_inside_markdown_structure() {
        let text = "# Heading #abcd\n\n- list **@coder**\n- [link #efgh](http://x)\n> quote #mnop";
        assert_eq!(
            parse(text),
            BTreeSet::from([item("abcd"), short("coder"), item("efgh"), item("mnop")])
        );
    }

    #[test]
    fn a_ref_at_the_start_of_a_line_is_not_a_heading() {
        assert_eq!(parse("#k3f7a2"), BTreeSet::from([item("k3f7a2")]));
    }

    #[test]
    fn trims_trailing_separators() {
        assert_eq!(
            parse("(@joshua/) and @coder-"),
            BTreeSet::from([short("joshua"), short("coder")])
        );
    }

    #[test]
    fn too_many_segments_is_plain_text() {
        assert_eq!(parse("@a/b/c/d"), BTreeSet::new());
    }

    #[test]
    fn duplicates_collapse() {
        assert_eq!(parse("#abcd #abcd @x @x").len(), 2);
    }
}

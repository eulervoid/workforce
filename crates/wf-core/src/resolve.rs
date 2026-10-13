//! Resolving the tokens found by `refs` against the workforce, at query time.
//!
//! Resolution depends on everything known, so it is never stored: a token that does not resolve
//! today may resolve after the next sync, and tokens that do not resolve are plain text.

use std::collections::BTreeSet;

use crate::id::{
    CommentId, HashId, ItemId, MIN_PREFIX_LEN, PrefixError, SessionId, resolve_prefix,
};
use crate::identity::{AgentId, PrincipalKey};
use crate::item::{Item, WrittenIn};
use crate::refs::{Mention, Token};
use crate::state::State;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MentionTarget {
    Principal(PrincipalKey),
    Agent(AgentId),
}

/// What a `#` reference points at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Reference {
    Item(ItemId),
    Comment { item: ItemId, comment: CommentId },
}

impl Reference {
    /// The item referenced, or the item of the comment referenced.
    pub fn item(&self) -> ItemId {
        match self {
            Reference::Item(item) | Reference::Comment { item, .. } => *item,
        }
    }

    /// The comment referenced, if it is one.
    pub fn comment(&self) -> Option<CommentId> {
        match self {
            Reference::Item(_) => None,
            Reference::Comment { comment, .. } => Some(*comment),
        }
    }
}

/// A reference to an item or one of its comments, and where it was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Backlink {
    pub from: Source,
    /// The comment referenced, when the reference is to a comment rather than the item.
    pub comment: Option<CommentId>,
}

/// Where a token was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Source {
    pub item: ItemId,
    pub written_in: WrittenIn,
}

pub struct Resolver<'a> {
    state: &'a State,
    /// Base32 text of every item id, sorted, for prefix lookups.
    items: std::collections::BTreeMap<String, ItemId>,
    sessions: std::collections::BTreeMap<String, SessionId>,
}

impl State {
    pub fn resolver(&self) -> Resolver<'_> {
        Resolver {
            state: self,
            items: self
                .items_including_archived()
                .map(|item| (item.id().to_base32(), item.id()))
                .collect(),
            sessions: self
                .sessions()
                .map(|session| (session.id().to_base32(), session.id()))
                .collect(),
        }
    }
}

impl Resolver<'_> {
    /// Resolves an item id prefix like git: at least [`MIN_PREFIX_LEN`] characters and unique.
    /// Archived items resolve too, because dependencies on them still do.
    pub fn item(&self, prefix: &str) -> Result<ItemId, PrefixError> {
        let prefix = prefix.to_ascii_lowercase();
        if prefix.len() < MIN_PREFIX_LEN {
            return Err(PrefixError::TooShort(prefix));
        }
        let mut matches = self
            .items
            .range(prefix.clone()..)
            .take_while(|(candidate, _)| candidate.starts_with(&prefix))
            .map(|(_, id)| *id);
        match (matches.next(), matches.next()) {
            (None, _) => Err(PrefixError::NotFound(prefix)),
            (Some(id), None) => Ok(id),
            (Some(first), Some(second)) => Err(PrefixError::Ambiguous {
                candidates: [first, second].iter().map(HashId::short).collect(),
                prefix,
            }),
        }
    }

    /// Resolves a session id prefix the same way.
    pub fn session(&self, prefix: &str) -> Result<SessionId, PrefixError> {
        let prefix = prefix.to_ascii_lowercase();
        if prefix.len() < MIN_PREFIX_LEN {
            return Err(PrefixError::TooShort(prefix));
        }
        let mut matches = self
            .sessions
            .range(prefix.clone()..)
            .take_while(|(candidate, _)| candidate.starts_with(&prefix))
            .map(|(_, id)| *id);
        match (matches.next(), matches.next()) {
            (None, _) => Err(PrefixError::NotFound(prefix)),
            (Some(id), None) => Ok(id),
            (Some(first), Some(second)) => Err(PrefixError::Ambiguous {
                candidates: [first, second].iter().map(HashId::short).collect(),
                prefix,
            }),
        }
    }

    /// Resolves `#item/comment`: the item prefix as [`Resolver::item`] does, the comment prefix
    /// among the comments of that item.
    pub fn comment(
        &self,
        item_prefix: &str,
        comment_prefix: &str,
    ) -> Result<(ItemId, CommentId), PrefixError> {
        let item = self.item(item_prefix)?;
        let comments = self
            .state
            .item_including_archived(item)
            .into_iter()
            .flat_map(|item| item.comments().keys().copied());
        let comment = resolve_prefix(comment_prefix, comments)?;
        Ok((item, comment))
    }

    /// What an item or comment reference points at, if it resolves. Mentions are no references.
    pub fn reference(&self, token: &Token) -> Option<Reference> {
        match token {
            Token::ItemRef(prefix) => self.item(prefix).ok().map(Reference::Item),
            Token::CommentRef { item, comment } => self
                .comment(item, comment)
                .ok()
                .map(|(item, comment)| Reference::Comment { item, comment }),
            Token::Mention(_) => None,
        }
    }

    /// `@ada` is a principal. Without a matching handle, a short `@coder` names an agent if
    /// exactly one agent has that name. Sessions arrive in M2 and never resolve yet.
    pub fn mention(&self, mention: &Mention) -> Option<MentionTarget> {
        match mention {
            Mention::Short(name) => {
                if let Some((principal, _)) = self
                    .state
                    .members()
                    .find(|(_, member)| member.handle.as_str() == name)
                {
                    return Some(MentionTarget::Principal(*principal));
                }
                let mut agents = self
                    .state
                    .agents()
                    .filter(|(id, _)| id.name.as_str() == name);
                match (agents.next(), agents.next()) {
                    (Some((id, _)), None) => Some(MentionTarget::Agent(id.clone())),
                    _ => None,
                }
            }
            Mention::Agent { principal, agent } => {
                let (principal, _) = self
                    .state
                    .members()
                    .find(|(_, member)| member.handle.as_str() == principal)?;
                self.state
                    .agents()
                    .find(|(id, _)| id.principal == *principal && id.name.as_str() == agent)
                    .map(|(id, _)| MentionTarget::Agent(id.clone()))
            }
        }
    }
}

fn tokens_of(item: &Item) -> impl Iterator<Item = (WrittenIn, &BTreeSet<Token>)> {
    std::iter::once((WrittenIn::Body, item.body_tokens())).chain(
        item.comments()
            .iter()
            .map(|(id, comment)| (WrittenIn::Comment(*id), &comment.tokens)),
    )
}

impl State {
    /// Items, and comments on other items, referenced from the body and comments of `item`.
    pub fn references_from(&self, item: &Item) -> BTreeSet<Reference> {
        let resolver = self.resolver();
        tokens_of(item)
            .flat_map(|(_, tokens)| tokens)
            .filter_map(|token| resolver.reference(token))
            .filter(|reference| reference.item() != item.id())
            .collect()
    }

    /// Where `target` or one of its comments is referenced from, in live items only.
    pub fn backlinks(&self, target: ItemId) -> Vec<Backlink> {
        let resolver = self.resolver();
        let mut backlinks = BTreeSet::new();
        for item in self.live_items().filter(|item| item.id() != target) {
            for (written_in, tokens) in tokens_of(item) {
                for reference in tokens.iter().filter_map(|token| resolver.reference(token)) {
                    if reference.item() == target {
                        backlinks.insert(Backlink {
                            from: Source {
                                item: item.id(),
                                written_in,
                            },
                            comment: reference.comment(),
                        });
                    }
                }
            }
        }
        backlinks.into_iter().collect()
    }

    /// Everyone mentioned in the body and comments of `item`.
    pub fn mentions_in(&self, item: &Item) -> BTreeSet<MentionTarget> {
        let resolver = self.resolver();
        tokens_of(item)
            .flat_map(|(_, tokens)| tokens)
            .filter_map(|token| match token {
                Token::Mention(mention) => resolver.mention(mention),
                Token::ItemRef(_) | Token::CommentRef { .. } => None,
            })
            .collect()
    }

    /// Where `target` is mentioned, in live items only. Mentions are shown, not delivered, in M0.
    pub fn mentions_of(&self, target: &MentionTarget) -> Vec<Source> {
        let resolver = self.resolver();
        let mut sources = Vec::new();
        for item in self.live_items() {
            for (written_in, tokens) in tokens_of(item) {
                let mentioned = tokens.iter().any(|token| match token {
                    Token::Mention(mention) => resolver.mention(mention).as_ref() == Some(target),
                    Token::ItemRef(_) | Token::CommentRef { .. } => false,
                });
                if mentioned {
                    sources.push(Source {
                        item: item.id(),
                        written_in,
                    });
                }
            }
        }
        sources.sort();
        sources
    }
}

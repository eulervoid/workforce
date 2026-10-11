//! Resolving the tokens found by `refs` against the workforce, at query time.
//!
//! Resolution depends on everything known, so it is never stored: a token that does not resolve
//! today may resolve after the next sync, and tokens that do not resolve are plain text.

use std::collections::BTreeSet;

use crate::id::{HashId, ItemId, MIN_PREFIX_LEN, PrefixError, SessionId};
use crate::identity::{AgentId, PrincipalKey};
use crate::item::{Item, Origin};
use crate::refs::{Mention, Token};
use crate::state::State;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MentionTarget {
    Principal(PrincipalKey),
    Agent(AgentId),
}

/// Where a token was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Source {
    pub item: ItemId,
    pub origin: Origin,
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
                .raw_items()
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

    /// `@joshua` is a principal. Without a matching handle, a short `@coder` names an agent if
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
            Mention::Session { .. } => None,
        }
    }
}

fn tokens_of(item: &Item) -> impl Iterator<Item = (Origin, &BTreeSet<Token>)> {
    std::iter::once((Origin::Body, item.body_tokens())).chain(
        item.notes()
            .iter()
            .map(|(id, note)| (Origin::Note(*id), &note.tokens)),
    )
}

impl State {
    /// Items referenced from the body and notes of `item`.
    pub fn references_from(&self, item: &Item) -> BTreeSet<ItemId> {
        let resolver = self.resolver();
        tokens_of(item)
            .flat_map(|(_, tokens)| tokens)
            .filter_map(|token| match token {
                Token::ItemRef(prefix) => resolver.item(prefix).ok(),
                Token::Mention(_) => None,
            })
            .filter(|target| *target != item.id())
            .collect()
    }

    /// Where `target` is referenced from, in live items only.
    pub fn backlinks(&self, target: ItemId) -> Vec<Source> {
        let resolver = self.resolver();
        let mut sources = Vec::new();
        for item in self.live_items().filter(|item| item.id() != target) {
            for (origin, tokens) in tokens_of(item) {
                let references = tokens.iter().any(|token| match token {
                    Token::ItemRef(prefix) => resolver.item(prefix) == Ok(target),
                    Token::Mention(_) => false,
                });
                if references {
                    sources.push(Source {
                        item: item.id(),
                        origin,
                    });
                }
            }
        }
        sources.sort();
        sources
    }

    /// Everyone mentioned in the body and notes of `item`.
    pub fn mentions_in(&self, item: &Item) -> BTreeSet<MentionTarget> {
        let resolver = self.resolver();
        tokens_of(item)
            .flat_map(|(_, tokens)| tokens)
            .filter_map(|token| match token {
                Token::Mention(mention) => resolver.mention(mention),
                Token::ItemRef(_) => None,
            })
            .collect()
    }

    /// Where `target` is mentioned, in live items only. Mentions are shown, not delivered, in M0.
    pub fn mentions_of(&self, target: &MentionTarget) -> Vec<Source> {
        let resolver = self.resolver();
        let mut sources = Vec::new();
        for item in self.live_items() {
            for (origin, tokens) in tokens_of(item) {
                let mentioned = tokens.iter().any(|token| match token {
                    Token::Mention(mention) => resolver.mention(mention).as_ref() == Some(target),
                    Token::ItemRef(_) => false,
                });
                if mentioned {
                    sources.push(Source {
                        item: item.id(),
                        origin,
                    });
                }
            }
        }
        sources.sort();
        sources
    }
}

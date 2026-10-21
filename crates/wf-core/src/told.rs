//! Which comments a live session is told: those on its item that someone else wrote, and those
//! elsewhere that mention the session or its agent. The daemon that runs the session keeps what it
//! told, and tells the rest with the next prompt.

use std::collections::BTreeSet;

use crate::hlc::Hlc;
use crate::id::{CommentId, ItemId, SessionId};
use crate::refs::Token;
use crate::resolve::MentionTarget;
use crate::state::State;

/// A comment, with the item it is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ItemComment {
    pub item: ItemId,
    pub comment: CommentId,
}

impl State {
    /// The comments written for `session`, oldest first: every comment on its item that it did not
    /// write, every comment on another live item that mentions it, and every one there that
    /// mentions its agent and was written since the agent's previous session started. The brief
    /// tells all of them; later prompts tell those that came since.
    pub fn written_for(&self, session: SessionId) -> Vec<ItemComment> {
        self.written_for_at(session)
            .into_iter()
            .map(|(_, written)| written)
            .collect()
    }

    /// [`State::written_for`], each with when it was written.
    fn written_for_at(&self, session: SessionId) -> Vec<(Hlc, ItemComment)> {
        let Some(folded) = self.session(session) else {
            return Vec::new();
        };
        let agent = folded.agent();
        // The mentions of the agent before then were for its previous session, or its brief.
        let previous_start = self
            .sessions()
            .filter(|other| other.agent() == agent && other.started() < folded.started())
            .map(|other| other.started().hlc)
            .max();
        let resolver = self.resolver();
        let mut written = Vec::new();
        for item in self.live_items() {
            let own_item = item.id() == folded.item();
            for (id, comment) in item.comments() {
                if comment.session() == Some(session) {
                    continue;
                }
                let mentions_it = || {
                    comment.tokens.iter().any(|token| match token {
                        Token::Mention(mention) => match resolver.mention(mention) {
                            Some(MentionTarget::Session(mentioned)) => mentioned == session,
                            Some(MentionTarget::Agent(mentioned)) => {
                                mentioned == *agent
                                    && previous_start.is_none_or(|started| comment.at > started)
                            }
                            Some(MentionTarget::Principal(_)) | None => false,
                        },
                        Token::ItemRef(_) | Token::CommentRef { .. } => false,
                    })
                };
                if own_item || mentions_it() {
                    let written_comment = ItemComment {
                        item: item.id(),
                        comment: *id,
                    };
                    written.push((comment.at, written_comment));
                }
            }
        }
        written.sort_by_key(|(at, written)| (*at, written.comment));
        written
    }

    /// The comments written for `session` that are not among those it was `told`, oldest first.
    pub fn untold(&self, session: SessionId, told: &BTreeSet<CommentId>) -> Vec<ItemComment> {
        self.written_for(session)
            .into_iter()
            .filter(|written| !told.contains(&written.comment))
            .collect()
    }

    /// What `session` counts as told when only the time of its last prompt, `prompted_ms`, is
    /// known: every comment for it written until then.
    pub fn told_before(&self, session: SessionId, prompted_ms: u64) -> BTreeSet<CommentId> {
        self.written_for_at(session)
            .into_iter()
            .filter(|(at, _)| at.wall_ms() <= prompted_ms)
            .map(|(_, written)| written.comment)
            .collect()
    }
}

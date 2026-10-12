//! Who a blocker waits on, and which blockers an agent can answer. Pure functions of the state.

use crate::id::{CommentId, ItemId};
use crate::identity::{AgentId, PrincipalKey};
use crate::item::{Comment, Item};
use crate::name::LaneName;
use crate::refs::Token;
use crate::resolve::MentionTarget;
use crate::session::SessionRole;
use crate::state::State;

/// Whoever has to resolve a blocker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WaitsOn {
    /// No one is named and agents in the `supervision` lane exist: one of them answers it.
    Supervisor,
    /// The blocker mentions this agent first.
    Agent(AgentId),
    /// The blocker mentions this principal first, or no one is named and there is no supervisor.
    Principal(PrincipalKey),
}

/// An open blocker of an item and who it waits on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenBlocker {
    pub item: ItemId,
    pub comment: CommentId,
    pub waits_on: WaitsOn,
}

fn supervision() -> LaneName {
    LaneName::new("supervision").expect("a valid lane name")
}

impl State {
    /// Who the open blocker `comment` on `item` waits on: the first principal or agent it mentions,
    /// otherwise a supervisor, otherwise the principal of whoever wrote it.
    pub fn waits_on(&self, comment: &Comment) -> WaitsOn {
        let resolver = self.resolver();
        let mentioned = comment.tokens.iter().find_map(|token| match token {
            Token::Mention(mention) => resolver.mention(mention),
            Token::ItemRef(_) | Token::CommentRef { .. } => None,
        });
        match mentioned {
            Some(MentionTarget::Agent(agent)) => WaitsOn::Agent(agent),
            Some(MentionTarget::Principal(principal)) => WaitsOn::Principal(principal),
            None if self.has_supervisor() => WaitsOn::Supervisor,
            None => WaitsOn::Principal(comment.by.principal()),
        }
    }

    /// The open blockers of every live item, oldest first within an item.
    pub fn open_blockers(&self) -> Vec<OpenBlocker> {
        self.live_items()
            .flat_map(|item| self.open_blockers_of(item))
            .collect()
    }

    pub fn open_blockers_of(&self, item: &Item) -> Vec<OpenBlocker> {
        item.open_comments()
            .into_iter()
            .filter(|(_, comment)| comment.kind == crate::event::CommentKind::Blocker)
            .map(|(id, comment)| OpenBlocker {
                item: item.id(),
                comment: id,
                waits_on: self.waits_on(comment),
            })
            .collect()
    }

    /// Whether `agent` may answer `blocker`: it waits on a supervisor and the agent is one, or on
    /// the agent itself.
    pub fn answers(&self, agent: &AgentId, blocker: &OpenBlocker) -> bool {
        match &blocker.waits_on {
            WaitsOn::Supervisor => self
                .agent(&agent.name, agent.principal)
                .is_some_and(|definition| definition.lanes.contains(&supervision())),
            WaitsOn::Agent(waited_on) => waited_on == agent,
            WaitsOn::Principal(_) => false,
        }
    }

    /// The items with a blocker `agent` can answer in a supervise session, oldest blocker first:
    /// the agent never worked on the item, no supervise session is live on it, and the agent did
    /// not give up on supervising it since the blocker was written.
    pub fn supervisable(&self, agent: &AgentId) -> Vec<ItemId> {
        let mut items: Vec<(crate::hlc::Hlc, ItemId)> = Vec::new();
        for item in self.live_items() {
            let Some(oldest) = self
                .open_blockers_of(item)
                .into_iter()
                .filter(|blocker| self.answers(agent, blocker))
                .filter_map(|blocker| item.comments().get(&blocker.comment).map(|c| c.at))
                .min()
            else {
                continue;
            };
            let sessions: Vec<_> = self
                .sessions()
                .filter(|session| session.item() == item.id())
                .collect();
            let worked_on = sessions
                .iter()
                .any(|session| session.agent() == agent && session.role() == SessionRole::Work);
            let supervised_now = sessions
                .iter()
                .any(|session| session.is_live() && session.role() == SessionRole::Supervise);
            let allowed = self
                .recovery(item.id(), SessionRole::Supervise)
                .allows_attempt();
            if !worked_on && !supervised_now && allowed {
                items.push((oldest, item.id()));
            }
        }
        items.sort();
        items.into_iter().map(|(_, item)| item).collect()
    }

    /// Whether some agent is in the `supervision` lane.
    fn has_supervisor(&self) -> bool {
        self.agents()
            .any(|(_, definition)| definition.lanes.contains(&supervision()))
    }
}

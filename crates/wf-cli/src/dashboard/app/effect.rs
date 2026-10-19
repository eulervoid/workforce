//! What a key asks of the workforce or the terminal.

use wf_core::id::{CommentId, ItemId, SessionId};
use wf_core::item::ItemState;
use wf_core::name::AgentName;
use wf_core::session::CommitId;
use wf_harness::Permission;

use super::clickable::Request;
use super::prompt::Written;
use crate::dashboard::diff::LineComment;

/// What a key asks of the workforce or the terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    Quit,
    /// Answers a permission request, which the daemon refuses once it is not the oldest of its
    /// session.
    Permit {
        request: Request,
        answer: Permission,
    },
    /// A comment that resolves a blocker.
    Resolve {
        item: ItemId,
        blocker: CommentId,
        text: String,
    },
    /// Writes the comment that resolves a blocker in `$EDITOR`, starting from `text`.
    ResolveInEditor {
        item: ItemId,
        blocker: CommentId,
        text: String,
    },
    /// Posts the comments on lines of the review, then approves the work as of `reviewed`, which
    /// is refused when that is not what was submitted.
    Approve {
        item: ItemId,
        reviewed: Option<CommitId>,
        comments: Vec<LineComment>,
    },
    /// Posts the comments on lines of the review, then requests the changes. Without a change
    /// request among them, writes the change requests in `$EDITOR` first, one per line. Refused
    /// like [`Effect::Approve`] when `reviewed` is not what was submitted.
    RequestChanges {
        item: ItemId,
        reviewed: Option<CommitId>,
        comments: Vec<LineComment>,
    },
    /// Posts a comment about lines of the diff, while no review waits.
    PostLineComment {
        item: ItemId,
        comment: LineComment,
    },
    /// Writes a comment about lines of the diff in `$EDITOR`, starting from its text.
    LineCommentInEditor {
        item: ItemId,
        comment: LineComment,
        then: Written,
    },
    /// The work of an item through git, in its pager, for those who read diffs with their own
    /// tools, such as delta.
    PageDiff {
        item: ItemId,
    },
    /// Asks who starts work on an item: an agent, or the dispatcher for a draft.
    ChooseStart {
        item: ItemId,
    },
    /// Asks which state to move an item to.
    ChooseState {
        item: ItemId,
    },
    StartSession {
        item: ItemId,
        agent: AgentName,
    },
    StopSession {
        session: SessionId,
    },
    Move {
        item: ItemId,
        state: ItemState,
    },
    Comment {
        item: ItemId,
        text: String,
    },
    /// Writes a comment in `$EDITOR`, starting from `text`, then posts it.
    CommentInEditor {
        item: ItemId,
        text: String,
    },
    /// Edits an item in `$EDITOR`, as `wf item edit` does.
    Edit {
        item: ItemId,
    },
}

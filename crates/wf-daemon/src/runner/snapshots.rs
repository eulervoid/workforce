//! Snapshots of the work of work sessions, recorded in their transcripts (SPEC §10). They never
//! enter the log: no other device has their objects.

use std::io::Write;
use std::path::Path;

use jiff::Timestamp;
use wf_core::event::{Event, EventBody};
use wf_core::id::{CommentId, HashId, ItemId, SessionId, WorkforceId};
use wf_core::session::{CommentSnapshot, CommitId, GitOid, SessionRole, SnapshotChanges};
use wf_harness::{Direction, TranscriptEntry};
use wf_vcs::{GitCli, Snapshot};

use super::blocking;
use crate::live_diff::target_of;
use crate::{Daemon, Loaded};

/// Where in a work session a snapshot is taken.
#[derive(Clone, Copy, Debug)]
pub(super) enum SnapshotPoint {
    /// When a block of tool calls ended: the agent wrote text again after them, or made
    /// [`wf_harness::BLOCK_CALLS`] of them without text.
    Block,
    /// After the import of a comment by the session on its item.
    Comment(CommentId),
    /// At the end of a turn, which is also where a session parks.
    TurnEnd,
}

/// `{"snapshot": <tree>, "head": <commit>?, "base": <commit>?, "at": "block" | "comment" | "turn_end",
/// "comment": <id>?, "left_out": [...], "changes": {"against": <tree or commit>, "files": [...]}?}`,
/// written by wf. `head` is where the item's branch was, which the uncommitted part of the snapshot
/// is against; `base` is where it left the target then, which the committed part is against, also
/// once the item is merged. `changes` are the files it changed against the session's snapshot
/// before it, which the transcript shows; `None` for a first snapshot of a session that did not
/// keep where it started.
fn snapshot_entry(
    at: Timestamp,
    snapshot: &Snapshot,
    head: Option<CommitId>,
    base: Option<CommitId>,
    changes: Option<&SnapshotChanges>,
    point: SnapshotPoint,
) -> TranscriptEntry {
    let mut message = serde_json::json!({
        "snapshot": snapshot.tree.to_string(),
        "head": head.map(|head| head.to_string()),
        "base": base.map(|base| base.to_string()),
        "left_out": snapshot.left_out,
    });
    if let Some(changes) = changes {
        message["changes"] = serde_json::json!(changes);
    }
    match point {
        SnapshotPoint::Comment(comment) => {
            message["at"] = "comment".into();
            message["comment"] = comment.to_base32().into();
        }
        SnapshotPoint::Block => message["at"] = "block".into(),
        SnapshotPoint::TurnEnd => message["at"] = "turn_end".into(),
    }
    TranscriptEntry {
        at,
        direction: Direction::Wf,
        message,
    }
}

/// A snapshot as the transcript records it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RecordedSnapshot {
    pub(crate) tree: GitOid,
    /// Where the item's branch was; `None` when it had none.
    pub(crate) head: Option<CommitId>,
    /// The merge base of `head` with the target then; `None` when there was no head, no target or
    /// no common history, or the entry is older than this field.
    pub(crate) base: Option<CommitId>,
}

/// The snapshot that `at` names in `transcript`, the lines of the transcript of its session:
/// the one recorded at its comment, or the last one recorded from its blocker on before the agent
/// was prompted again. Lines that are not entries are passed over.
pub(crate) fn find_snapshot(transcript: &str, at: CommentSnapshot) -> Option<RecordedSnapshot> {
    let comment = match at {
        CommentSnapshot::Taken { comment, .. } => comment,
        CommentSnapshot::Parked { blocker, .. } => blocker,
    };
    let comment = comment.to_base32();
    let mut entries = transcript
        .lines()
        .filter_map(|line| serde_json::from_str::<TranscriptEntry>(line).ok());
    let at_comment = entries.by_ref().find_map(|entry| {
        let snapshot = recorded(&entry)?;
        (entry.message["comment"] == comment.as_str()).then_some(snapshot)
    })?;
    match at {
        CommentSnapshot::Taken { .. } => Some(at_comment),
        CommentSnapshot::Parked { .. } => Some(
            entries
                .take_while(|entry| !is_prompt(entry))
                .filter_map(|entry| recorded(&entry))
                .last()
                .unwrap_or(at_comment),
        ),
    }
}

/// Whether `transcript`, the lines of the transcript of a session, records the snapshot `tree`
/// with what it changed against `against`.
pub(crate) fn records_changes(transcript: &str, against: GitOid, tree: GitOid) -> bool {
    transcript
        .lines()
        .filter_map(|line| serde_json::from_str::<TranscriptEntry>(line).ok())
        .filter(|entry| recorded(entry).is_some_and(|snapshot| snapshot.tree == tree))
        .any(|entry| {
            serde_json::from_value::<SnapshotChanges>(entry.message["changes"].clone())
                .is_ok_and(|changes| changes.against == against)
        })
}

/// The snapshot `entry` records, when it is a snapshot entry.
fn recorded(entry: &TranscriptEntry) -> Option<RecordedSnapshot> {
    match entry.direction {
        Direction::Wf => {}
        Direction::ToAgent | Direction::FromAgent | Direction::AgentLog => return None,
    }
    let tree = entry.message["snapshot"].as_str()?.parse().ok()?;
    let commit = |field: &str| {
        entry.message[field]
            .as_str()
            .and_then(|commit| commit.parse().ok())
            .map(CommitId::Git)
    };
    Some(RecordedSnapshot {
        tree,
        head: commit("head"),
        base: commit("base"),
    })
}

/// Whether `entry` prompts the agent: a turn begins.
fn is_prompt(entry: &TranscriptEntry) -> bool {
    match entry.direction {
        Direction::ToAgent => entry.message["method"] == "session/prompt",
        Direction::FromAgent | Direction::AgentLog | Direction::Wf => false,
    }
}

/// What `snapshot` of `session` changed against its snapshot before, or for its first against the
/// commit the session started on; `None` when it did not keep that.
fn changes(
    workforce_directory: &Path,
    item: ItemId,
    session: SessionId,
    snapshot: &Snapshot,
) -> Result<Option<SnapshotChanges>, wf_vcs::VcsError> {
    let against = match snapshot.previous {
        Some(previous) => Some(previous),
        None => wf_vcs::started_on(workforce_directory, item, session)?
            .map(|CommitId::Git(commit)| commit),
    };
    let Some(against) = against else {
        return Ok(None);
    };
    let diff = wf_vcs::snapshot_changes(workforce_directory, item, against, snapshot.tree)?;
    Ok(Some(SnapshotChanges {
        against,
        files: wf_vcs::diff::parse(&diff)
            .iter()
            .map(wf_vcs::diff::FileDiff::changed)
            .collect(),
    }))
}

/// Appends `entry` to the transcript at `path` in one write, beside the harness, which appends
/// whole lines to the same file.
fn append(path: &Path, entry: &TranscriptEntry) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(entry)?;
    line.push(b'\n');
    std::fs::OpenOptions::new()
        .append(true)
        .open(path)?
        .write_all(&line)
}

impl Daemon {
    /// Takes a snapshot of the work of the work session `session` on `item` and records it in the
    /// session's transcript, at a comment always, so that the comment finds it, and otherwise when
    /// its tree differs from the session's previous recorded one. A snapshot that fails is only
    /// logged: the work goes on without it.
    pub(super) async fn snapshot_work(
        &self,
        workforce: WorkforceId,
        loaded: &Loaded,
        item: ItemId,
        session: SessionId,
        point: SnapshotPoint,
    ) {
        // The session's index and latest tree are written by one snapshot at a time.
        let _taking = self.snapshotting.lock(session).await;
        let workforce_directory = self.workforce_directory(workforce);
        let transcript = self.launch.data_dir.transcript(&workforce, session);
        let target = self.read(loaded, target_of).await.ok().flatten();
        let recorded = blocking(move || {
            let snapshot = wf_vcs::take_snapshot(&workforce_directory, item, session)
                .map_err(|error| wf_core::error_chain(&error))?;
            let unchanged = snapshot.previous == Some(snapshot.tree);
            match point {
                SnapshotPoint::Comment(_) => {}
                SnapshotPoint::Block | SnapshotPoint::TurnEnd if unchanged => return Ok(()),
                SnapshotPoint::Block | SnapshotPoint::TurnEnd => {}
            }
            // The import before the snapshot moved the branch to what the agent committed.
            let git = GitCli::open(&wf_vcs::bare_repository(&workforce_directory)).ok();
            let head = git
                .as_ref()
                .and_then(|git| git.branch_head(&wf_vcs::item_branch(item)).ok());
            let base = match (&git, head, &target) {
                (Some(git), Some(head), Some(target)) => git
                    .branch_head(target)
                    .and_then(|target| git.merge_base(target, head))
                    .ok()
                    .flatten(),
                _ => None,
            };
            let changes = changes(&workforce_directory, item, session, &snapshot)
                .map_err(|error| wf_core::error_chain(&error))?;
            let entry = snapshot_entry(
                Timestamp::now(),
                &snapshot,
                head,
                base,
                changes.as_ref(),
                point,
            );
            append(&transcript, &entry)
                .map_err(|error| format!("cannot write {}: {error}", transcript.display()))?;
            // Only a snapshot in the transcript is the previous one of the next, so that one not
            // written is taken again.
            wf_vcs::record_snapshot(&workforce_directory, item, session, snapshot.tree)
                .map_err(|error| wf_core::error_chain(&error))
        })
        .await;
        match recorded {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(
                "session {}: could not take a snapshot of the work: {error}",
                session.short()
            ),
            Err(error) => tracing::warn!("session {}: {error}", session.short()),
        }
    }

    /// Keeps `head`, where the branch was when the work session `session` started, for what its
    /// first snapshot changed to be against. One that fails is only logged: the transcript then
    /// shows no changes under the first snapshot.
    pub(super) async fn record_start(
        &self,
        workforce: WorkforceId,
        item: ItemId,
        session: SessionId,
        head: CommitId,
    ) {
        let workforce_directory = self.workforce_directory(workforce);
        let recorded =
            blocking(move || wf_vcs::record_start(&workforce_directory, item, session, head)).await;
        match recorded {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(
                "session {}: could not keep where its branch started: {}",
                session.short(),
                wf_core::error_chain(&error)
            ),
            Err(error) => tracing::warn!("session {}: {error}", session.short()),
        }
    }

    /// Takes a snapshot of the work after `event`, written for `session`, when it is a comment by
    /// the live work session on its own item: the one whose import [`Daemon::with_head`] made.
    pub(crate) async fn snapshot_at_comment(
        &self,
        workforce: WorkforceId,
        loaded: &Loaded,
        session: SessionId,
        event: &Event,
    ) {
        let EventBody::Commented(comment) = &event.content.body else {
            return;
        };
        let Ok(state) = self.snapshot(loaded).await else {
            return;
        };
        let Some(folded) = state.session(session) else {
            return;
        };
        if folded.role() != SessionRole::Work || folded.item() != comment.item || !folded.is_live()
        {
            return;
        }
        let point = SnapshotPoint::Comment(CommentId::from(event.id()));
        self.snapshot_work(workforce, loaded, comment.item, session, point)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use wf_core::id::EventId;

    use super::*;

    fn tree(byte: u8) -> GitOid {
        GitOid::from_bytes([byte; 20])
    }

    fn comment(byte: u8) -> CommentId {
        CommentId::from(EventId::of(&[byte]))
    }

    fn snapshot_at(point: SnapshotPoint, byte: u8) -> TranscriptEntry {
        let snapshot = Snapshot {
            tree: tree(byte),
            left_out: Vec::new(),
            previous: None,
        };
        let head = Some(CommitId::Git(tree(byte + 100)));
        let base = Some(CommitId::Git(tree(byte + 200)));
        snapshot_entry(Timestamp::UNIX_EPOCH, &snapshot, head, base, None, point)
    }

    fn to_agent(message: serde_json::Value) -> TranscriptEntry {
        TranscriptEntry {
            at: Timestamp::UNIX_EPOCH,
            direction: Direction::ToAgent,
            message,
        }
    }

    fn transcript(entries: &[TranscriptEntry]) -> String {
        entries
            .iter()
            .map(|entry| serde_json::to_string(entry).unwrap() + "\n")
            .collect()
    }

    fn found(tree_byte: u8) -> Option<RecordedSnapshot> {
        Some(RecordedSnapshot {
            tree: tree(tree_byte),
            head: Some(CommitId::Git(tree(tree_byte + 100))),
            base: Some(CommitId::Git(tree(tree_byte + 200))),
        })
    }

    #[test]
    fn a_comment_finds_its_snapshot_and_a_blocker_the_last_one_before_the_next_prompt() {
        let session = SessionId::from(EventId::of(b"session"));
        let prompt = to_agent(serde_json::json!({"method": "session/prompt", "id": 2}));
        let lines = transcript(&[
            snapshot_at(SnapshotPoint::Comment(comment(1)), 1),
            snapshot_at(SnapshotPoint::Comment(comment(2)), 2),
            to_agent(serde_json::json!({"method": "session/cancel"})),
            snapshot_at(SnapshotPoint::TurnEnd, 3),
            prompt.clone(),
            snapshot_at(SnapshotPoint::TurnEnd, 4),
            snapshot_at(SnapshotPoint::Comment(comment(5)), 5),
        ]);
        let taken = |comment| CommentSnapshot::Taken { session, comment };
        let parked = |blocker| CommentSnapshot::Parked { session, blocker };

        assert_eq!(find_snapshot(&lines, taken(comment(1))), found(1));
        assert_eq!(find_snapshot(&lines, taken(comment(5))), found(5));
        assert_eq!(find_snapshot(&lines, parked(comment(2))), found(3));
        assert_eq!(
            find_snapshot(&lines, parked(comment(5))),
            found(5),
            "still parked: the last one so far"
        );
        assert_eq!(find_snapshot(&lines, taken(comment(9))), None);
        assert_eq!(find_snapshot("not an entry\n", taken(comment(1))), None);
    }
}

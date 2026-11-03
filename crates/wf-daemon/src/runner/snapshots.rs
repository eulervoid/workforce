//! Snapshots of the work of work sessions, recorded in their transcripts (SPEC §10). They never
//! enter the log: no other device has their objects.

use std::io::Write;
use std::path::Path;

use jiff::Timestamp;
use wf_core::event::{Event, EventBody};
use wf_core::id::{CommentId, HashId, ItemId, SessionId, WorkforceId};
use wf_core::session::SessionRole;
use wf_harness::{Direction, TranscriptEntry};
use wf_vcs::Snapshot;

use super::blocking;
use crate::{Daemon, Loaded};

/// Where in a work session a snapshot is taken.
#[derive(Clone, Copy, Debug)]
pub(super) enum SnapshotPoint {
    /// After the import of a comment by the session on its item.
    Comment(CommentId),
    /// At the end of a turn, which is also where a session parks.
    TurnEnd,
}

/// `{"snapshot": <tree>, "at": "comment" | "turn_end", "comment": <id>?, "left_out": [...]}`,
/// written by wf.
fn snapshot_entry(at: Timestamp, snapshot: &Snapshot, point: SnapshotPoint) -> TranscriptEntry {
    let mut message = serde_json::json!({
        "snapshot": snapshot.tree.to_string(),
        "left_out": snapshot.left_out,
    });
    match point {
        SnapshotPoint::Comment(comment) => {
            message["at"] = "comment".into();
            message["comment"] = comment.to_base32().into();
        }
        SnapshotPoint::TurnEnd => message["at"] = "turn_end".into(),
    }
    TranscriptEntry {
        at,
        direction: Direction::Wf,
        message,
    }
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
    /// session's transcript, when its tree differs from the session's previous recorded one. A
    /// snapshot that fails is only logged: the work goes on without it.
    pub(super) async fn snapshot_work(
        &self,
        workforce: WorkforceId,
        item: ItemId,
        session: SessionId,
        point: SnapshotPoint,
    ) {
        // The session's index and latest tree are written by one snapshot at a time.
        let _taking = self.snapshotting.lock(session).await;
        let workforce_directory = self.workforce_directory(workforce);
        let transcript = self.launch.data_dir.transcript(&workforce, session);
        let recorded = blocking(move || {
            let snapshot = wf_vcs::take_snapshot(&workforce_directory, item, session)
                .map_err(|error| wf_core::error_chain(&error))?;
            if snapshot.previous == Some(snapshot.tree) {
                return Ok(());
            }
            append(
                &transcript,
                &snapshot_entry(Timestamp::now(), &snapshot, point),
            )
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
        self.snapshot_work(workforce, comment.item, session, point)
            .await;
    }
}

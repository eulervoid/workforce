//! The diff detail: the work of an item against its target, as `wf item diff` computes it, in
//! files of hunks of lines, with a cursor line, a selection of lines, and the comments on lines
//! under the lines they are about. Pure: the loop reads the diff, drawing shows it.

use std::collections::{HashMap, HashSet};
use std::ops::RangeInclusive;
use std::rc::Rc;

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Position, Rect};
use wf_core::diff::{Content, DiffLine, FileDiff};
use wf_core::event::{CommentKind, DiffSide, LineAnchor};
use wf_core::session::CommitId;

use super::app::FirstRow;

/// The work of an item as git read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemWork {
    /// The commit whose files the diff shows: the head of the item's branch, or once merged,
    /// the merge.
    pub commit: CommitId,
    pub changes: Changes,
}

/// What the work changes against its target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Changes {
    /// The branch has no commits ahead of the target yet.
    NoCommits,
    /// The files its commits change; none when they change nothing.
    Files(Vec<FileDiff>),
}

/// How many columns the text of a comment is wrapped to under the lines it is about, whatever the
/// width of the window, so that its rows do not depend on it.
const COMMENT_WIDTH: usize = 72;

/// A comment on lines of the diff, as written in the dashboard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineComment {
    /// A plain comment or a change request.
    pub kind: CommentKind,
    pub lines: LineAnchor,
    pub text: String,
}

/// A comment on lines of the diff of an item that is in the log already.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostedComment {
    /// `@ada`, `coder`.
    pub by: String,
    pub comment: LineComment,
    /// A change request a later comment resolved.
    pub resolved: bool,
}

/// One of the comments the diff shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShownComment {
    /// Of the review being written, by its place among the drafts.
    Draft(usize),
    /// In the log, by its place among the posted ones.
    Posted(usize),
}

/// A line of the diff detail, as it is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffRow {
    /// The header of a file: its path and how many lines it adds and removes.
    File(usize),
    /// Where a hunk starts, with its heading.
    Hunk { file: usize, hunk: usize },
    Line {
        file: usize,
        hunk: usize,
        line: usize,
    },
    /// What a file without hunks changed: binary, or only its mode or name.
    NoLines(usize),
    /// A row of a comment under the lines it is about, or under the header of its file when
    /// it is about lines of another commit: its header on row 0, then its text.
    Comment {
        file: usize,
        shown: ShownComment,
        row: usize,
    },
    /// Between two files.
    Blank,
}

impl DiffRow {
    /// Whether the cursor stops on it: the files, the lines, and the drafts, to edit or drop.
    fn takes_cursor(self) -> bool {
        match self {
            DiffRow::File(_)
            | DiffRow::Line { .. }
            | DiffRow::Comment {
                shown: ShownComment::Draft(_),
                row: 0,
                ..
            } => true,
            DiffRow::Hunk { .. }
            | DiffRow::NoLines(_)
            | DiffRow::Comment { .. }
            | DiffRow::Blank => false,
        }
    }

    pub fn file(self) -> Option<usize> {
        match self {
            DiffRow::File(file)
            | DiffRow::Hunk { file, .. }
            | DiffRow::Line { file, .. }
            | DiffRow::NoLines(file)
            | DiffRow::Comment { file, .. } => Some(file),
            DiffRow::Blank => None,
        }
    }
}

/// The diff of an item on screen, and where the cursor is in it.
#[derive(Clone, Debug)]
pub struct DiffPane {
    /// The work as last read, or why it could not be; `None` until it is read.
    work: Option<Result<ItemWork, String>>,
    /// The row the cursor is on.
    pub cursor: usize,
    /// The first row on screen, and whether it follows the cursor.
    pub top: FirstRow,
    /// The files folded to their header, by path, so they stay folded as the work moves on.
    folded: HashSet<String>,
    /// The row a selection of lines started on, which reaches from there to the cursor; `None`
    /// while no lines are selected.
    pub selection: Option<usize>,
    /// The line the left button was pressed on, which a drag selects from.
    pressed: Option<usize>,
    /// Where the rows were drawn, the first of them at the top; `None` while the diff is not on
    /// screen.
    pub area: Option<Rect>,
    /// The comments on lines of the review being written, which the verdict sends.
    drafts: Vec<LineComment>,
    /// The comments on lines of the item in the log, the oldest first.
    posted: Vec<PostedComment>,
    /// What the rows depend on, laid out again whenever one of the above it depends on changes.
    layout: Layout,
}

/// The rows of a diff and the wrapped text of its comments, kept until the work, the folds or
/// the comments change.
#[derive(Clone, Debug, Default)]
struct Layout {
    rows: Rc<[DiffRow]>,
    /// The rows of each draft under its header, by place among the drafts.
    drafts: Vec<Vec<String>>,
    /// The same of each posted comment.
    posted: Vec<Vec<String>>,
}

impl Default for DiffPane {
    fn default() -> Self {
        Self::new()
    }
}

impl DiffPane {
    pub fn new() -> Self {
        Self {
            work: None,
            cursor: 0,
            top: FirstRow::Follows(0),
            folded: HashSet::new(),
            selection: None,
            pressed: None,
            area: None,
            drafts: Vec::new(),
            posted: Vec::new(),
            layout: Layout::default(),
        }
    }

    /// The work as last read, or why it could not be; `None` until it is read.
    pub fn work(&self) -> Option<&Result<ItemWork, String>> {
        self.work.as_ref()
    }

    /// Whether the file at `path` is folded to its header.
    pub fn is_folded(&self, path: &str) -> bool {
        self.folded.contains(path)
    }

    /// The comments on lines of the review being written.
    pub fn drafts(&self) -> &[LineComment] {
        &self.drafts
    }

    /// Lets go of all drafts, as when the review is sent or dropped.
    pub fn clear_drafts(&mut self) {
        self.drafts.clear();
        self.lay_out();
    }

    /// Lets go of the first `count` drafts, as when they were posted.
    pub fn drop_first_drafts(&mut self, count: usize) {
        self.drafts.drain(..count.min(self.drafts.len()));
        self.lay_out();
    }

    /// The comments on lines of the item in the log.
    pub fn posted(&self) -> &[PostedComment] {
        &self.posted
    }

    /// Takes in the comments on lines of the item in the log, laying out again only if they
    /// differ from those known.
    pub fn set_posted(&mut self, posted: &[PostedComment]) {
        if self.posted != posted {
            self.posted = posted.to_vec();
            self.lay_out();
        }
    }

    /// The commit whose diff is shown, once read.
    pub fn commit(&self) -> Option<CommitId> {
        match &self.work {
            Some(Ok(work)) => Some(work.commit),
            Some(Err(_)) | None => None,
        }
    }

    /// Takes in the work as read again. The cursor stays on its row, as near as there is one.
    /// A selection is of lines that may be gone.
    pub fn read(&mut self, work: Result<ItemWork, String>) {
        self.work = Some(work);
        self.selection = None;
        self.pressed = None;
        self.lay_out();
        let rows = self.rows();
        self.cursor = self.cursor.min(rows.len().saturating_sub(1));
        self.cursor = nearest_cursor_row(&rows, self.cursor);
    }

    pub fn files(&self) -> &[FileDiff] {
        match &self.work {
            Some(Ok(ItemWork {
                changes: Changes::Files(files),
                ..
            })) => files,
            Some(Ok(ItemWork {
                changes: Changes::NoCommits,
                ..
            }))
            | Some(Err(_))
            | None => &[],
        }
    }

    /// The rows of the diff, from the top.
    pub fn rows(&self) -> Rc<[DiffRow]> {
        Rc::clone(&self.layout.rows)
    }

    /// Lays the rows and the comments out again.
    fn lay_out(&mut self) {
        let bodies = |comments: &mut dyn Iterator<Item = ShownComment>| -> Vec<Vec<String>> {
            comments.map(|shown| self.wrapped_comment(shown)).collect()
        };
        let drafts = bodies(&mut (0..self.drafts.len()).map(ShownComment::Draft));
        let posted = bodies(&mut (0..self.posted.len()).map(ShownComment::Posted));
        self.layout = Layout {
            rows: Rc::default(),
            drafts,
            posted,
        };
        self.layout.rows = self.build_rows().into();
    }

    fn build_rows(&self) -> Vec<DiffRow> {
        let (under_lines, under_files) = self.placed_comments();
        let mut rows = Vec::new();
        let comment_rows = |rows: &mut Vec<DiffRow>, file: usize, shown: &[ShownComment]| {
            for shown in shown {
                let height = 1 + self.comment_body(*shown).len();
                rows.extend((0..height).map(|row| DiffRow::Comment {
                    file,
                    shown: *shown,
                    row,
                }));
            }
        };
        for (index, file) in self.files().iter().enumerate() {
            if index > 0 {
                rows.push(DiffRow::Blank);
            }
            rows.push(DiffRow::File(index));
            if self.folded.contains(&file.path) {
                continue;
            }
            if let Some(shown) = under_files.get(&index) {
                comment_rows(&mut rows, index, shown);
            }
            match &file.content {
                Content::Text(hunks) if !hunks.is_empty() => {
                    for (hunk_index, hunk) in hunks.iter().enumerate() {
                        rows.push(DiffRow::Hunk {
                            file: index,
                            hunk: hunk_index,
                        });
                        for line in 0..hunk.lines.len() {
                            rows.push(DiffRow::Line {
                                file: index,
                                hunk: hunk_index,
                                line,
                            });
                            if let Some(shown) = under_lines.get(&(index, hunk_index, line)) {
                                comment_rows(&mut rows, index, shown);
                            }
                        }
                    }
                }
                Content::Text(_) | Content::Binary => rows.push(DiffRow::NoLines(index)),
            }
        }
        rows
    }

    /// The comment `shown` is of.
    pub fn comment(&self, shown: ShownComment) -> &LineComment {
        match shown {
            ShownComment::Draft(index) => &self.drafts[index],
            ShownComment::Posted(index) => &self.posted[index].comment,
        }
    }

    /// Whether the comment is about lines of another commit than the one on screen.
    pub fn is_outdated(&self, shown: ShownComment) -> bool {
        Some(self.comment(shown).lines.commit) != self.commit()
    }

    /// The rows of a comment under its header: the lines it quotes when they are of another
    /// commit, then its text, wrapped.
    pub fn comment_body(&self, shown: ShownComment) -> &[String] {
        match shown {
            ShownComment::Draft(index) => &self.layout.drafts[index],
            ShownComment::Posted(index) => &self.layout.posted[index],
        }
    }

    fn wrapped_comment(&self, shown: ShownComment) -> Vec<String> {
        let comment = self.comment(shown);
        let mut body = Vec::new();
        if self.is_outdated(shown) {
            body.extend(comment.lines.quote.lines().map(str::to_owned));
        }
        for paragraph in comment.text.lines() {
            if paragraph.trim().is_empty() {
                body.push(String::new());
                continue;
            }
            body.extend(
                textwrap::wrap(paragraph, COMMENT_WIDTH)
                    .into_iter()
                    .map(|line| line.into_owned()),
            );
        }
        body
    }

    /// Where each comment goes: under the last line it is about, by file, hunk and line; or,
    /// when it is about another commit or lines not in the diff, under the header of its file.
    /// A comment on a file the diff no longer changes is not shown.
    #[allow(clippy::type_complexity)]
    fn placed_comments(
        &self,
    ) -> (
        HashMap<(usize, usize, usize), Vec<ShownComment>>,
        HashMap<usize, Vec<ShownComment>>,
    ) {
        let mut under_lines: HashMap<_, Vec<ShownComment>> = HashMap::new();
        let mut under_files: HashMap<_, Vec<ShownComment>> = HashMap::new();
        let shown = (0..self.posted.len())
            .map(ShownComment::Posted)
            .chain((0..self.drafts.len()).map(ShownComment::Draft));
        for shown in shown {
            let lines = &self.comment(shown).lines;
            let Some(file) = self.files().iter().position(|file| file.path == lines.path) else {
                continue;
            };
            let line = (!self.is_outdated(shown))
                .then(|| self.line_numbered(file, lines.side, lines.last))
                .flatten();
            match line {
                Some(line) => under_lines.entry(line).or_default().push(shown),
                None => under_files.entry(file).or_default().push(shown),
            }
        }
        (under_lines, under_files)
    }

    /// The hunk and line of `file` with the number `number` on `side`.
    fn line_numbered(
        &self,
        file: usize,
        side: DiffSide,
        number: u32,
    ) -> Option<(usize, usize, usize)> {
        let Content::Text(hunks) = &self.files()[file].content else {
            return None;
        };
        hunks.iter().enumerate().find_map(|(hunk_index, hunk)| {
            let line = hunk.lines.iter().position(|line| match side {
                DiffSide::New => line.new_number() == Some(number),
                DiffSide::Old => {
                    matches!(line, DiffLine::Removed { .. }) && line.old_number() == Some(number)
                }
            })?;
            Some((file, hunk_index, line))
        })
    }

    /// The lines the selection holds, or the line of the cursor without one, as a comment names
    /// them; `None` when neither is on lines.
    pub fn selected_lines(&self) -> Option<LineAnchor> {
        let commit = self.commit()?;
        let rows = self.rows();
        let range = self.selected().unwrap_or(self.cursor..=self.cursor);
        let mut path = None;
        let lines: Vec<&DiffLine> = rows
            .get(range)?
            .iter()
            .filter_map(|row| {
                let DiffRow::Line { file, hunk, line } = *row else {
                    return None;
                };
                let file = &self.files()[file];
                path = Some(file.path.clone());
                let Content::Text(hunks) = &file.content else {
                    return None;
                };
                Some(&hunks[hunk].lines[line])
            })
            .collect();
        // Lines removed count on the old side only when they are all there is.
        let new: Vec<u32> = lines.iter().filter_map(|line| line.new_number()).collect();
        let (side, numbers) = if new.is_empty() {
            let old = lines.iter().filter_map(|line| line.old_number()).collect();
            (DiffSide::Old, old)
        } else {
            (DiffSide::New, new)
        };
        let quote = lines
            .iter()
            .map(|line| format!("{}{}", line.marker(), line.text()))
            .collect::<Vec<_>>()
            .join("\n");
        Some(LineAnchor {
            commit,
            path: path?,
            side,
            first: *numbers.iter().min()?,
            last: *numbers.iter().max()?,
            quote,
        })
    }

    /// The draft the cursor is on.
    pub fn draft_at_cursor(&self) -> Option<usize> {
        match self.rows().get(self.cursor)? {
            DiffRow::Comment {
                shown: ShownComment::Draft(index),
                ..
            } => Some(*index),
            DiffRow::Comment {
                shown: ShownComment::Posted(_),
                ..
            }
            | DiffRow::File(_)
            | DiffRow::Hunk { .. }
            | DiffRow::Line { .. }
            | DiffRow::NoLines(_)
            | DiffRow::Blank => None,
        }
    }

    /// Takes in a comment written about lines: a new draft, or one written again in place of
    /// the draft `replacing`. The selection it was written about ends.
    pub fn drafted(&mut self, comment: LineComment, replacing: Option<usize>) {
        match replacing.and_then(|index| self.drafts.get_mut(index)) {
            Some(draft) => *draft = comment,
            None => self.drafts.push(comment),
        }
        self.selection = None;
        self.lay_out();
    }

    /// Drops the draft `index`; the cursor stays where it is, as near as there is a row.
    pub fn drop_draft(&mut self, index: usize) {
        if index < self.drafts.len() {
            self.drafts.remove(index);
            self.lay_out();
        }
        let rows = self.rows();
        self.cursor = nearest_cursor_row(&rows, self.cursor.min(rows.len().saturating_sub(1)));
    }

    /// What the keys of the diff do; `false` for a key it does not take. A page is `page` rows.
    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "crossterm's `KeyCode` has keys no binding uses"
    )]
    pub fn key(&mut self, key: KeyEvent, page: usize) -> bool {
        let rows = self.rows();
        if rows.is_empty() {
            return false;
        }
        let last = rows.len() - 1;
        match key.code {
            KeyCode::Char('v') => {
                self.selection = match self.selection {
                    Some(_) => None,
                    None => matches!(rows.get(self.cursor), Some(DiffRow::Line { .. }))
                        .then_some(self.cursor),
                };
                return true;
            }
            KeyCode::Esc if self.selection.is_some() => {
                self.selection = None;
                return true;
            }
            // A selection is of the lines of one hunk: jumping or folding ends it.
            KeyCode::Char(']' | '[') | KeyCode::Enter => self.selection = None,
            _ => {}
        }
        let cursor = match key.code {
            KeyCode::Down | KeyCode::Char('j') => next_cursor_row(&rows, self.cursor, 1),
            KeyCode::Up | KeyCode::Char('k') => previous_cursor_row(&rows, self.cursor, 1),
            KeyCode::PageDown | KeyCode::Char(' ') => next_cursor_row(&rows, self.cursor, page),
            KeyCode::PageUp => previous_cursor_row(&rows, self.cursor, page),
            KeyCode::Char('g') => nearest_cursor_row(&rows, 0),
            KeyCode::Char('G') => nearest_cursor_row(&rows, last),
            KeyCode::Char(']') => (self.cursor + 1..rows.len())
                .find(|row| matches!(rows[*row], DiffRow::File(_)))
                .unwrap_or(self.cursor),
            KeyCode::Char('[') => (0..self.cursor)
                .rev()
                .find(|row| matches!(rows[*row], DiffRow::File(_)))
                .unwrap_or(self.cursor),
            KeyCode::Enter => {
                self.toggle_fold(&rows);
                return true;
            }
            _ => return false,
        };
        self.cursor = match self.selection {
            Some(start) => keep_to_hunk(&rows, start, cursor),
            None => cursor,
        };
        self.top = self.top.follow();
        true
    }

    /// The rows selected, from the first to the last; `None` while none are.
    pub fn selected(&self) -> Option<RangeInclusive<usize>> {
        let start = self.selection?;
        Some(start.min(self.cursor)..=start.max(self.cursor))
    }

    /// The row on screen at `y`, as last drawn; `None` off the diff.
    fn row_at(&self, rows: &[DiffRow], position: Position) -> Option<usize> {
        let area = self.area.filter(|area| area.contains(position))?;
        let row = self.top.line() + usize::from(position.y - area.y);
        (row < rows.len()).then_some(row)
    }

    /// The left button pressed at `position`: the cursor goes to the row there, and a selection
    /// ends. Returns whether that was on the diff.
    pub fn press(&mut self, position: Position) -> bool {
        let rows = self.rows();
        let Some(row) = self.row_at(&rows, position) else {
            return false;
        };
        self.cursor = nearest_cursor_row(&rows, row);
        self.selection = None;
        self.pressed = matches!(rows[self.cursor], DiffRow::Line { .. }).then_some(self.cursor);
        self.top = self.top.follow();
        true
    }

    /// The pointer dragged to `position` with the left button down: the lines from where it was
    /// pressed to there are selected, within the hunk it was pressed in.
    pub fn drag(&mut self, position: Position) {
        let Some(start) = self.pressed else {
            return;
        };
        let rows = self.rows();
        let Some(area) = self.area else {
            return;
        };
        // Above or below the diff, the drag reaches its first or last row on screen.
        let y = position
            .y
            .clamp(area.top(), area.bottom().saturating_sub(1));
        let Some(row) = self.row_at(&rows, Position::new(area.x, y)) else {
            return;
        };
        self.selection = Some(start);
        self.cursor = keep_to_hunk(&rows, start, row);
        self.top = self.top.follow();
    }

    /// Folds the file the cursor is in to its header, or unfolds it; the cursor goes to the
    /// header.
    fn toggle_fold(&mut self, rows: &[DiffRow]) {
        let Some(file) = rows.get(self.cursor).and_then(|row| row.file()) else {
            return;
        };
        let path = self.files()[file].path.clone();
        if !self.folded.remove(&path) {
            self.folded.insert(path);
        }
        self.lay_out();
        if let Some(header) = self
            .rows()
            .iter()
            .position(|row| *row == DiffRow::File(file))
        {
            self.cursor = header;
        }
        self.top = self.top.follow();
    }

    /// The wheel scrolled the diff to `top`; the cursor is kept on screen by drawing.
    pub fn scroll(&mut self, top: FirstRow) {
        self.top = top;
    }

    /// Takes in where drawing put the rows, the top and the cursor.
    pub fn shown(&mut self, area: Rect, top: usize, cursor: usize) {
        self.area = Some(area);
        self.top = self.top.at(top);
        self.cursor = cursor;
    }
}

/// `row`, or the nearest line to it of the hunk that `start` is a line of.
fn keep_to_hunk(rows: &[DiffRow], start: usize, row: usize) -> usize {
    let DiffRow::Line { file, hunk, .. } = rows[start] else {
        return start;
    };
    // The comments under its lines are within it too.
    let in_hunk = |row: &usize| match rows[*row] {
        DiffRow::Line {
            file: line_file,
            hunk: line_hunk,
            ..
        } => line_file == file && line_hunk == hunk,
        DiffRow::Comment { .. } => true,
        DiffRow::File(_) | DiffRow::Hunk { .. } | DiffRow::NoLines(_) | DiffRow::Blank => false,
    };
    let first = (0..=start)
        .rev()
        .take_while(in_hunk)
        .last()
        .unwrap_or(start);
    let last = (start..rows.len())
        .take_while(in_hunk)
        .last()
        .unwrap_or(start);
    row.clamp(first, last)
}

/// The row `count` rows of the cursor further down from `from`, or the last there is.
fn next_cursor_row(rows: &[DiffRow], from: usize, count: usize) -> usize {
    let mut at = from;
    for row in (from + 1..rows.len()).filter(|row| rows[*row].takes_cursor()) {
        at = row;
        if row >= from + count {
            break;
        }
    }
    at
}

/// The row `count` rows of the cursor further up from `from`, or the first there is.
fn previous_cursor_row(rows: &[DiffRow], from: usize, count: usize) -> usize {
    let mut at = from;
    for row in (0..from).rev().filter(|row| rows[*row].takes_cursor()) {
        at = row;
        if row + count <= from {
            break;
        }
    }
    at
}

/// The row the cursor stops on nearest to `row`, looking down first.
pub fn nearest_cursor_row(rows: &[DiffRow], row: usize) -> usize {
    let down = (row..rows.len()).find(|row| rows[*row].takes_cursor());
    let up = (0..row.min(rows.len()))
        .rev()
        .find(|row| rows[*row].takes_cursor());
    down.or(up).unwrap_or(0)
}

/// The first row on screen of `rows` drawn `height` high, from `top` as it was: kept to the
/// cursor when it follows it, else where the wheel put it; and the cursor, kept on screen.
pub fn placed(rows: &[DiffRow], top: FirstRow, cursor: usize, height: usize) -> (usize, usize) {
    let last_top = rows.len().saturating_sub(height);
    match top {
        FirstRow::Follows(previous) => {
            let mut top = previous.min(last_top);
            // A row of context above the cursor, where there is one.
            if cursor < top + 1 {
                top = cursor.saturating_sub(1);
            }
            if cursor + 2 > top + height {
                top = (cursor + 2).saturating_sub(height).min(last_top);
            }
            (top, cursor)
        }
        FirstRow::Scrolled(top) => {
            let top = top.min(last_top);
            let bottom = (top + height).min(rows.len());
            let cursor = if (top..bottom).contains(&cursor) {
                cursor
            } else {
                let kept = cursor.clamp(top, bottom.saturating_sub(1));
                let on_screen = |row: &usize| (top..bottom).contains(row);
                Some(nearest_cursor_row(rows, kept))
                    .filter(on_screen)
                    .unwrap_or(cursor)
            };
            (top, cursor)
        }
    }
}

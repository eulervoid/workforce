//! The diff detail: the work of an item against its target, as `wf item diff` computes it, in
//! files of hunks of lines, with a cursor line. Pure: the loop reads the diff, drawing shows it.

use std::collections::HashSet;

use std::ops::RangeInclusive;

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Position, Rect};
use wf_core::id::ItemId;
use wf_core::session::CommitId;
use wf_vcs::diff::{Content, FileDiff};

use super::app::ListTop;

/// The work of an item as git read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemWork {
    /// The commit whose diff it is: the head of the item's branch.
    pub commit: CommitId,
    pub files: Vec<FileDiff>,
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
    Note(usize),
    /// Between two files.
    Blank,
}

impl DiffRow {
    /// Whether the cursor stops on it.
    fn takes_cursor(self) -> bool {
        match self {
            DiffRow::File(_) | DiffRow::Line { .. } => true,
            DiffRow::Hunk { .. } | DiffRow::Note(_) | DiffRow::Blank => false,
        }
    }

    pub fn file(self) -> Option<usize> {
        match self {
            DiffRow::File(file)
            | DiffRow::Hunk { file, .. }
            | DiffRow::Line { file, .. }
            | DiffRow::Note(file) => Some(file),
            DiffRow::Blank => None,
        }
    }
}

/// The diff of an item on screen, and where the cursor is in it.
#[derive(Clone, Debug)]
pub struct DiffPane {
    pub item: ItemId,
    /// The work as last read, or why it could not be; `None` until it is read.
    pub work: Option<Result<ItemWork, String>>,
    /// The row the cursor is on.
    pub cursor: usize,
    /// The first row on screen, and whether it follows the cursor.
    pub top: ListTop,
    /// The files folded to their header, by path, so they stay folded as the work moves on.
    pub folded: HashSet<String>,
    /// The row a selection of lines started on, which reaches from there to the cursor; `None`
    /// while no lines are selected.
    pub selection: Option<usize>,
    /// The line the left button was pressed on, which a drag selects from.
    pressed: Option<usize>,
    /// Where the rows were drawn, the first of them at the top; `None` while the diff is not on
    /// screen.
    pub area: Option<Rect>,
}

impl DiffPane {
    pub fn new(item: ItemId) -> Self {
        Self {
            item,
            work: None,
            cursor: 0,
            top: ListTop::FollowsSelection(0),
            folded: HashSet::new(),
            selection: None,
            pressed: None,
            area: None,
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
        let rows = self.rows();
        self.cursor = self.cursor.min(rows.len().saturating_sub(1));
        self.cursor = nearest_cursor_row(&rows, self.cursor);
    }

    pub fn files(&self) -> &[FileDiff] {
        match &self.work {
            Some(Ok(work)) => &work.files,
            Some(Err(_)) | None => &[],
        }
    }

    /// The rows of the diff, from the top.
    pub fn rows(&self) -> Vec<DiffRow> {
        let mut rows = Vec::new();
        for (index, file) in self.files().iter().enumerate() {
            if index > 0 {
                rows.push(DiffRow::Blank);
            }
            rows.push(DiffRow::File(index));
            if self.folded.contains(&file.path) {
                continue;
            }
            match &file.content {
                Content::Text(hunks) if !hunks.is_empty() => {
                    for (hunk_index, hunk) in hunks.iter().enumerate() {
                        rows.push(DiffRow::Hunk {
                            file: index,
                            hunk: hunk_index,
                        });
                        rows.extend((0..hunk.lines.len()).map(|line| DiffRow::Line {
                            file: index,
                            hunk: hunk_index,
                            line,
                        }));
                    }
                }
                Content::Text(_) | Content::Binary => rows.push(DiffRow::Note(index)),
            }
        }
        rows
    }

    /// What the keys of the diff do; `false` for a key it does not take. A page is `page` rows.
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
        self.top = self.top.follow_selection();
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
        self.top = self.top.follow_selection();
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
        self.top = self.top.follow_selection();
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
        if let Some(header) = self
            .rows()
            .iter()
            .position(|row| *row == DiffRow::File(file))
        {
            self.cursor = header;
        }
        self.top = self.top.follow_selection();
    }

    /// The wheel scrolled the diff to `top`; the cursor is kept on screen by drawing.
    pub fn scroll(&mut self, top: ListTop) {
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
    let in_hunk = |row: &usize| {
        matches!(rows[*row], DiffRow::Line { file: line_file, hunk: line_hunk, .. }
            if line_file == file && line_hunk == hunk)
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
pub fn placed(rows: &[DiffRow], top: ListTop, cursor: usize, height: usize) -> (usize, usize) {
    let last_top = rows.len().saturating_sub(height);
    match top {
        ListTop::FollowsSelection(previous) => {
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
        ListTop::Scrolled(top) => {
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

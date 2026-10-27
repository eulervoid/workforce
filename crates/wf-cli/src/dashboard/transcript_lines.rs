//! The lines of a transcript as the session detail draws them, one entry for each block, with a
//! cursor on one entry. Blocks are only ever appended to a transcript, so each is laid out once
//! and its lines kept, rather than the whole transcript laid out again for every frame; an entry
//! opened or closed lays out again only its own block.
//!
//! Timed by hand on a real transcript of 170 blocks and 15418 lines at 100 columns, in a release
//! build: laying it all out took about 12 ms a frame, before ratatui went through all of it;
//! with the lines kept, a frame takes about 25 µs.

use std::collections::HashMap;
use std::ops::Range;

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Position, Rect};
use ratatui::text::Line;

use super::app::Scroll;
use crate::transcript::{Block, Layout, Spacing, Transcript, Verbosity};

/// The columns left of the lines of a transcript: the bar of the cursor, then the chevron of an
/// entry and a space.
pub const MARKS: u16 = 3;

/// How much of an entry is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Extent {
    /// All of it, which abridging leaves as it is.
    Whole,
    /// Cut short; in full it shows more.
    Abridged,
    Full,
}

/// One block of the transcript as laid out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Its lines, without the blank line before it; empty when it shows nothing.
    pub lines: Range<usize>,
    pub extent: Extent,
}

/// The entry the cursor is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryCursor {
    /// The last entry shown, whichever that is as the transcript grows; the end is followed.
    Last,
    /// The entry of this block.
    On(usize),
}

/// What a click on the lines landed on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pressed {
    /// The block of the entry clicked, which the cursor is on now.
    pub block: usize,
    /// Whether the click was on its first line, the one with the chevron.
    pub first_line: bool,
}

/// The lines of the blocks of a transcript, as far as they are laid out, and the cursor.
pub struct TranscriptLines {
    layout: Layout,
    /// The entries opened or closed by hand since `f` last set them all, by their block, and how
    /// much each shows.
    by_hand: HashMap<usize, Verbosity>,
    /// One for each block laid out, by its place in the transcript.
    entries: Vec<Entry>,
    lines: Vec<Line<'static>>,
    /// How the last block that showed anything left off.
    last: Spacing,
    cursor: EntryCursor,
    /// Where the lines were last drawn, and the line drawn at the top; `None` while they are not
    /// on screen.
    drawn: Option<(Rect, usize)>,
}

impl TranscriptLines {
    pub fn new(layout: Layout) -> Self {
        Self {
            layout,
            by_hand: HashMap::new(),
            entries: Vec::new(),
            lines: Vec::new(),
            last: Spacing::Nothing,
            cursor: EntryCursor::Last,
            drawn: None,
        }
    }

    /// Lays out the blocks added to `transcript` since the last time; all of them again for a
    /// layout other than the last one, each as opened or closed by hand.
    pub fn update(&mut self, layout: Layout, transcript: &Transcript) {
        let blocks = transcript.blocks();
        // A transcript with fewer blocks than were laid out is another one.
        if blocks.len() < self.entries.len() {
            *self = Self::new(layout);
        } else if self.layout != layout {
            self.layout = layout;
            self.entries.clear();
            self.lines.clear();
            self.last = Spacing::Nothing;
        }
        for (index, block) in blocks.iter().enumerate().skip(self.entries.len()) {
            let (text, extent) = self.lay_out(index, block);
            let lines = add(
                &mut self.lines,
                &mut self.last,
                block.opens(),
                block.leaves(),
                &text,
            );
            self.entries.push(Entry { lines, extent });
        }
    }

    /// The text of the block at `index`, as much of it as opened or closed by hand, else as the
    /// layout says, and how much of it that is.
    fn lay_out(&self, index: usize, block: &Block) -> (String, Extent) {
        let verbosity = self
            .by_hand
            .get(&index)
            .copied()
            .unwrap_or(self.layout.verbosity());
        let layout = |verbosity| {
            if verbosity == self.layout.verbosity() {
                self.layout.clone()
            } else {
                self.layout.with_verbosity(verbosity)
            }
        };
        let text = layout(verbosity).block(block);
        if !block.can_abridge() || text.is_empty() {
            return (text, Extent::Whole);
        }
        let other = layout(match verbosity {
            Verbosity::Abridged => Verbosity::Full,
            Verbosity::Full => Verbosity::Abridged,
        })
        .block(block);
        // An entry that abridged shows nothing, as the harness's own lines, is not opened by hand.
        let extent = if other.is_empty() || other == text {
            Extent::Whole
        } else {
            match verbosity {
                Verbosity::Abridged => Extent::Abridged,
                Verbosity::Full => Extent::Full,
            }
        };
        (text, extent)
    }

    /// The lines of the finished blocks.
    pub fn lines(&self) -> &[Line<'static>] {
        &self.lines
    }

    /// The entry of the block at `index`, once laid out.
    pub fn entry(&self, index: usize) -> Option<&Entry> {
        self.entries.get(index)
    }

    /// The entry the line at `line` is of, by its block; `None` for a blank line between them.
    pub fn entry_at(&self, line: usize) -> Option<usize> {
        let index = self
            .entries
            .partition_point(|entry| entry.lines.end <= line);
        self.entries
            .get(index)
            .filter(|entry| entry.lines.contains(&line))
            .map(|_| index)
    }

    /// The lines of what is still open in `transcript` (its prose, its running calls), which go
    /// below [`TranscriptLines::lines`]. They change until they are done, so they are laid out
    /// for each frame.
    pub fn open_lines(&self, transcript: &Transcript) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        let mut last = self.last;
        if let Some(prose) = transcript.open_prose() {
            let text = self.layout.prose(prose.kind, &prose.text);
            add(&mut lines, &mut last, Spacing::Prose, Spacing::Prose, &text);
        }
        for call in transcript.open_calls() {
            let text = self.layout.running(call);
            add(&mut lines, &mut last, Spacing::Tool, Spacing::Tool, &text);
        }
        lines
    }

    /// Opens the entry of the block at `index` in full, or closes it back to abridged, whatever
    /// `f` set; returns its first line, or `None` for an entry with nothing to abridge. Only
    /// that block is laid out again.
    pub fn toggle(&mut self, index: usize, transcript: &Transcript) -> Option<usize> {
        let (entry, block) = self
            .entries
            .get(index)
            .zip(transcript.blocks().get(index))?;
        let verbosity = match entry.extent {
            Extent::Whole => return None,
            Extent::Abridged => Verbosity::Full,
            Extent::Full => Verbosity::Abridged,
        };
        let old = entry.lines.clone();
        self.by_hand.insert(index, verbosity);
        let (text, extent) = self.lay_out(index, block);
        // Abridged or in full, it shows something: the blank lines around it stay as they are.
        let lines = super::draw::ansi_text(&text).lines;
        let new = old.start..old.start + lines.len();
        self.lines.splice(old.clone(), lines);
        self.entries[index] = Entry {
            lines: new.clone(),
            extent,
        };
        for later in &mut self.entries[index + 1..] {
            later.lines =
                later.lines.start - old.end + new.end..later.lines.end - old.end + new.end;
        }
        Some(new.start)
    }

    /// `f`: every entry shows as much as the layout says, whatever was opened or closed by hand,
    /// from the end.
    pub fn set_all(&mut self) {
        self.by_hand.clear();
        self.cursor = EntryCursor::Last;
    }

    /// The blocks of the entries that show something.
    fn shown_blocks(&self) -> impl DoubleEndedIterator<Item = usize> + '_ {
        (0..self.entries.len()).filter(|index| !self.entries[*index].lines.is_empty())
    }

    /// The block of the entry `cursor` is on; `None` while no entry shows anything.
    pub fn block_of(&self, cursor: EntryCursor) -> Option<usize> {
        match cursor {
            EntryCursor::Last => self.shown_blocks().next_back(),
            EntryCursor::On(index) => Some(index),
        }
    }

    /// The block of the entry the cursor is on.
    pub fn cursor_block(&self) -> Option<usize> {
        self.block_of(self.cursor)
    }

    /// The cursor on the entry of the block at `index`: [`EntryCursor::Last`] on the last one.
    fn cursor_on(&self, index: usize) -> EntryCursor {
        if self.shown_blocks().next_back() == Some(index) {
            EntryCursor::Last
        } else {
            EntryCursor::On(index)
        }
    }

    /// What the keys of a transcript do to its cursor, and to `scroll`: `j` and `k` move the
    /// cursor from entry to entry, but through an entry taller than the screen line by line
    /// first. A page is `page` lines. `None` for a key they do not take.
    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "crossterm's `KeyCode` has keys no binding uses"
    )]
    pub fn key(&mut self, key: KeyEvent, scroll: Scroll, page: usize) -> Option<Scroll> {
        let Some((area, top)) = self.drawn else {
            return scroll.key(key, page);
        };
        let height = usize::from(area.height);
        let current = self.cursor_block();
        Some(match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                let Some(current) = current else {
                    return Some(scroll.down(1));
                };
                if self.entries[current].lines.end > top + height {
                    return Some(Scroll::At(top + 1));
                }
                let Some(next) = self.shown_blocks().find(|index| *index > current) else {
                    return Some(scroll.down(1));
                };
                self.cursor = self.cursor_on(next);
                // Down only as far as shows the entry, or as much of it as fits.
                let next = &self.entries[next].lines;
                Scroll::At(top.max(next.start.min(next.end.saturating_sub(height))))
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let Some(current) = current else {
                    return Some(scroll.up(1));
                };
                if self.entries[current].lines.start < top {
                    return Some(Scroll::At(top - 1));
                }
                let Some(previous) = self.shown_blocks().rev().find(|index| *index < current)
                else {
                    return Some(Scroll::At(top.saturating_sub(1)));
                };
                self.cursor = EntryCursor::On(previous);
                // Up only as far as shows the entry, or as much of its end as fits.
                let previous = &self.entries[previous].lines;
                Scroll::At(top.min(previous.start.max(previous.end.saturating_sub(height))))
            }
            KeyCode::Char('g') => {
                let first = self.shown_blocks().next();
                if let Some(first) = first {
                    self.cursor = self.cursor_on(first);
                }
                Scroll::At(0)
            }
            KeyCode::Char('G') => {
                self.cursor = EntryCursor::Last;
                Scroll::END
            }
            _ => return scroll.key(key, page),
        })
    }

    /// Where the transcript is drawn `height` lines high from `scroll`, with `more` open lines
    /// below the entries: the scroll kept to what the lines allow, following the end only while
    /// the cursor is on the last entry, and the cursor kept on screen.
    pub fn placed(&self, scroll: Scroll, more: usize, height: usize) -> (Scroll, EntryCursor) {
        let last_top = (self.lines.len() + more).saturating_sub(height);
        let scroll = match self.cursor {
            EntryCursor::Last => scroll.within(last_top),
            EntryCursor::On(_) => Scroll::At(scroll.top().min(last_top)),
        };
        let Some(current) = self.cursor_block() else {
            return (scroll, self.cursor);
        };
        let screen = scroll.top()..scroll.top() + height;
        let on_screen = |index: &usize| {
            let lines = &self.entries[*index].lines;
            !lines.is_empty() && lines.start < screen.end && lines.end > screen.start
        };
        if on_screen(&current) {
            return (scroll, self.cursor);
        }
        let nearest = if self.entries[current].lines.start >= screen.end {
            self.shown_blocks().rev().find(on_screen)
        } else {
            self.shown_blocks().find(on_screen)
        };
        let cursor = nearest.map_or(self.cursor, |index| self.cursor_on(index));
        (scroll, cursor)
    }

    /// Takes in where drawing put the lines, the line at the top and the cursor.
    pub fn shown(&mut self, area: Rect, top: usize, cursor: EntryCursor) {
        self.drawn = Some((area, top));
        self.cursor = cursor;
    }

    /// The lines are not on screen any more.
    pub fn hidden(&mut self) {
        self.drawn = None;
    }

    /// The left button pressed at `position`: the cursor goes to the entry there. `None` off the
    /// lines, or between entries.
    pub fn press(&mut self, position: Position) -> Option<Pressed> {
        let (area, top) = self.drawn.filter(|(area, _)| area.contains(position))?;
        let line = top + usize::from(position.y - area.y);
        let block = self.entry_at(line)?;
        self.cursor = self.cursor_on(block);
        Some(Pressed {
            block,
            first_line: self.entries[block].lines.start == line,
        })
    }
}

/// Adds the lines of `text` to `lines`, spaced from what came before, which left off as `last`;
/// returns where they are, empty when it shows nothing.
fn add(
    lines: &mut Vec<Line<'static>>,
    last: &mut Spacing,
    opens: Spacing,
    leaves: Spacing,
    text: &str,
) -> Range<usize> {
    if text.is_empty() {
        return lines.len()..lines.len();
    }
    if !last.before(opens).is_empty() {
        lines.push(Line::default());
    }
    let start = lines.len();
    lines.extend(super::draw::ansi_text(text).lines);
    *last = leaves;
    start..lines.len()
}

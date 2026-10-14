//! The lines of a transcript as the session detail draws them. Blocks are only ever appended to
//! a transcript, so each is laid out once and its lines kept, rather than the whole transcript
//! laid out again for every frame.
//!
//! Timed by hand on a real transcript of 170 blocks and 15418 lines at 100 columns, in a release
//! build: laying it all out took about 12 ms a frame, before ratatui went through all of it;
//! with the lines kept, a frame takes about 25 µs.

use ansi_to_tui::IntoText;
use ratatui::text::Line;

use crate::transcript::{Layout, Spacing, Transcript};

/// The lines of the blocks of a transcript, as far as they are laid out.
pub struct TranscriptLines {
    layout: Layout,
    /// How many of the transcript's blocks the lines hold.
    blocks: usize,
    lines: Vec<Line<'static>>,
    /// How the last block that showed anything left off.
    last: Spacing,
}

impl TranscriptLines {
    pub fn new(layout: Layout) -> Self {
        Self {
            layout,
            blocks: 0,
            lines: Vec::new(),
            last: Spacing::Nothing,
        }
    }

    /// Lays out the blocks added to `transcript` since the last time; all of them again for a
    /// layout other than the last one.
    pub fn update(&mut self, layout: Layout, transcript: &Transcript) {
        let blocks = transcript.blocks();
        // A transcript with fewer blocks than were laid out is another one.
        if self.layout != layout || blocks.len() < self.blocks {
            *self = Self::new(layout);
        }
        for block in &blocks[self.blocks..] {
            let text = self.layout.block(block);
            self.add(block.opens(), block.leaves(), &text);
        }
        self.blocks = blocks.len();
    }

    /// The lines of the finished blocks.
    pub fn lines(&self) -> &[Line<'static>] {
        &self.lines
    }

    /// The lines of what is still open in `transcript` (its prose, its running calls), which go
    /// below [`TranscriptLines::lines`]. They change until they are done, so they are laid out
    /// for each frame.
    pub fn open_lines(&self, transcript: &Transcript) -> Vec<Line<'static>> {
        let mut open = Self {
            layout: self.layout.clone(),
            blocks: self.blocks,
            lines: Vec::new(),
            last: self.last,
        };
        if let Some(prose) = transcript.open_prose() {
            let text = open.layout.prose(prose.kind, &prose.text);
            open.add(Spacing::Prose, Spacing::Prose, &text);
        }
        for call in transcript.open_calls() {
            let text = open.layout.running(call);
            open.add(Spacing::Tool, Spacing::Tool, &text);
        }
        open.lines
    }

    /// Adds the lines of `text`, spaced from what came before.
    fn add(&mut self, opens: Spacing, leaves: Spacing, text: &str) {
        if text.is_empty() {
            return;
        }
        if !self.last.before(opens).is_empty() {
            self.lines.push(Line::default());
        }
        self.lines
            .extend(text.into_text().unwrap_or_default().lines);
        self.last = leaves;
    }
}

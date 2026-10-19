//! A diff as data: the files it changes, their hunks, and each line with its numbers on the sides
//! it is on. Wf-vcs reads it from what `git diff` prints.

use serde::{Deserialize, Serialize};

/// What a diff changes in one file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiff {
    /// The path the file has after the change; before it, for a file deleted.
    pub path: String,
    pub change: FileChange,
    pub content: Content,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileChange {
    Added,
    Deleted,
    Modified,
    /// Moved from `from`, maybe changed too.
    Renamed {
        from: String,
    },
}

/// How a diff shows what changed in a file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Content {
    /// The hunks of the lines that changed; none when only the mode or the name did.
    Text(Vec<Hunk>),
    /// A file git does not diff as text.
    Binary,
}

/// The lines that changed in one place of a file, with the lines around them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hunk {
    /// What git names the place by, after the `@@`: the function or section it is in, often
    /// empty.
    pub heading: String,
    pub lines: Vec<DiffLine>,
}

/// A line of a hunk, with its number, from 1, on each side it is on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffLine {
    /// The same on both sides.
    Context {
        old: u32,
        new: u32,
        text: String,
    },
    Added {
        new: u32,
        text: String,
    },
    Removed {
        old: u32,
        text: String,
    },
}

impl DiffLine {
    /// The line without its newline.
    pub fn text(&self) -> &str {
        match self {
            DiffLine::Context { text, .. }
            | DiffLine::Added { text, .. }
            | DiffLine::Removed { text, .. } => text,
        }
    }

    /// Its number before the change; `None` for a line added.
    pub fn old_number(&self) -> Option<u32> {
        match self {
            DiffLine::Context { old, .. } | DiffLine::Removed { old, .. } => Some(*old),
            DiffLine::Added { .. } => None,
        }
    }

    /// Its number after the change; `None` for a line removed.
    pub fn new_number(&self) -> Option<u32> {
        match self {
            DiffLine::Context { new, .. } | DiffLine::Added { new, .. } => Some(*new),
            DiffLine::Removed { .. } => None,
        }
    }

    /// The mark it has in a unified diff: `+`, `-` or a space.
    pub fn marker(&self) -> char {
        match self {
            DiffLine::Context { .. } => ' ',
            DiffLine::Added { .. } => '+',
            DiffLine::Removed { .. } => '-',
        }
    }
}

impl FileDiff {
    /// How many lines it adds and removes.
    pub fn counts(&self) -> (usize, usize) {
        let Content::Text(hunks) = &self.content else {
            return (0, 0);
        };
        let lines = hunks.iter().flat_map(|hunk| &hunk.lines);
        lines.fold((0, 0), |(added, removed), line| match line {
            DiffLine::Added { .. } => (added + 1, removed),
            DiffLine::Removed { .. } => (added, removed + 1),
            DiffLine::Context { .. } => (added, removed),
        })
    }
}

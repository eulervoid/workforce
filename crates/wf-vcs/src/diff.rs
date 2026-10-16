//! A diff as data: the files it changes, their hunks, and each line with its numbers on the sides
//! it is on. Read from what `git diff` prints, with the options [`ARGS`] sets so that the user's
//! diff config does not change its form.

/// What `git diff` is asked for, so that its output is the unified form this module reads,
/// whatever the git config says: no colours, no external diff or text conversion, the usual
/// prefixes, renames found, and paths relative to the top of the repository.
pub(crate) const ARGS: [&str; 8] = [
    "--no-color",
    "--no-ext-diff",
    "--no-textconv",
    "--no-relative",
    "--find-renames",
    "--src-prefix=a/",
    "--dst-prefix=b/",
    "--unified=3",
];

/// What a diff changes in one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDiff {
    /// The path the file has after the change; before it, for a file deleted.
    pub path: String,
    pub change: FileChange,
    pub content: Content,
}

#[derive(Clone, Debug, PartialEq, Eq)]
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Content {
    /// The hunks of the lines that changed; none when only the mode or the name did.
    Text(Vec<Hunk>),
    /// A file git does not diff as text.
    Binary,
}

/// The lines that changed in one place of a file, with the lines around them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    /// What git names the place by, after the `@@`: the function or section it is in, often
    /// empty.
    pub heading: String,
    pub lines: Vec<DiffLine>,
}

/// A line of a hunk, with its number, from 1, on each side it is on.
#[derive(Clone, Debug, PartialEq, Eq)]
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
    pub fn old(&self) -> Option<u32> {
        match self {
            DiffLine::Context { old, .. } | DiffLine::Removed { old, .. } => Some(*old),
            DiffLine::Added { .. } => None,
        }
    }

    /// Its number after the change; `None` for a line removed.
    pub fn new(&self) -> Option<u32> {
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

/// Where a hunk is on each side, as its `@@ -12,3 +12,4 @@ heading` line says, and as far as its
/// lines are read: the number of the next line, and how many are left.
struct HunkCursor {
    old: u32,
    new: u32,
    old_left: u32,
    new_left: u32,
}

impl HunkCursor {
    fn is_done(&self) -> bool {
        self.old_left == 0 && self.new_left == 0
    }
}

/// The cursor at the start of the hunk a line begins, and its heading.
fn hunk_start(line: &str) -> Option<(HunkCursor, String)> {
    let rest = line.strip_prefix("@@ -")?;
    let (ranges, heading) = rest.split_once(" @@")?;
    let (old, new) = ranges.split_once(" +")?;
    // `12,3` is 3 lines from 12; `12` one line.
    let range = |range: &str| -> Option<(u32, u32)> {
        match range.split_once(',') {
            Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
            None => Some((range.parse().ok()?, 1)),
        }
    };
    let (old, old_left) = range(old)?;
    let (new, new_left) = range(new)?;
    let cursor = HunkCursor {
        old,
        new,
        old_left,
        new_left,
    };
    Some((cursor, heading.trim().to_owned()))
}

/// A path as git writes it: in double quotes with C escapes when it has unusual characters,
/// as `"tab\there"`; plain otherwise. Bytes escaped in octal make up UTF-8 again.
fn unquote(path: &str) -> String {
    let Some(quoted) = path
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    else {
        return path.to_owned();
    };
    let mut bytes = Vec::new();
    let mut characters = quoted.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '\\' {
            let mut buffer = [0; 4];
            bytes.extend(character.encode_utf8(&mut buffer).as_bytes());
            continue;
        }
        match characters.next() {
            Some('n') => bytes.push(b'\n'),
            Some('t') => bytes.push(b'\t'),
            Some('r') => bytes.push(b'\r'),
            Some('a') => bytes.push(0x07),
            Some('b') => bytes.push(0x08),
            Some('f') => bytes.push(0x0c),
            Some('v') => bytes.push(0x0b),
            Some(digit @ '0'..='7') => {
                let mut value = digit.to_digit(8).unwrap_or_default();
                for _ in 0..2 {
                    if let Some(next) = characters.peek().and_then(|next| next.to_digit(8)) {
                        value = value * 8 + next;
                        characters.next();
                    }
                }
                bytes.push(value as u8);
            }
            Some(other) => {
                let mut buffer = [0; 4];
                bytes.extend(other.encode_utf8(&mut buffer).as_bytes());
            }
            None => bytes.push(b'\\'),
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The path of a `--- a/path` or `+++ b/path` line without its prefix; `None` for `/dev/null`.
fn side_path(rest: &str, prefix: &str) -> Option<String> {
    // Git ends a name that has a space with a tab, as GNU diff does.
    let rest = rest.strip_suffix('\t').unwrap_or(rest);
    if rest == "/dev/null" {
        return None;
    }
    let path = unquote(rest);
    Some(
        path.strip_prefix(prefix)
            .map_or(path.clone(), str::to_owned),
    )
}

/// The path of a file from its `diff --git a/path b/path` line, for a change without `---` and
/// `+++` lines, such as one of the mode only. Both sides name the same path then, which splits
/// the line even when the path has spaces.
fn header_path(rest: &str) -> String {
    if rest.starts_with('"') {
        // Quoted: the first side ends at the first quote that is not escaped.
        let mut escaped = false;
        for (index, character) in rest.char_indices().skip(1) {
            match character {
                '\\' if !escaped => escaped = true,
                '"' if !escaped => {
                    let path = unquote(&rest[..=index]);
                    return path.strip_prefix("a/").map_or(path.clone(), str::to_owned);
                }
                _ => escaped = false,
            }
        }
    }
    let half = rest.len().saturating_sub(1) / 2;
    let first = rest.get(..half).unwrap_or(rest);
    first.strip_prefix("a/").unwrap_or(first).to_owned()
}

/// A file as far as its lines are read.
struct Reading {
    old_path: Option<String>,
    new_path: Option<String>,
    header_path: String,
    added: bool,
    deleted: bool,
    renamed_from: Option<String>,
    renamed_to: Option<String>,
    binary: bool,
    hunks: Vec<Hunk>,
}

impl Reading {
    fn new(header: &str) -> Self {
        Self {
            old_path: None,
            new_path: None,
            header_path: header_path(header),
            added: false,
            deleted: false,
            renamed_from: None,
            renamed_to: None,
            binary: false,
            hunks: Vec::new(),
        }
    }

    fn finish(self) -> FileDiff {
        let change = if self.added {
            FileChange::Added
        } else if self.deleted {
            FileChange::Deleted
        } else if let Some(from) = self.renamed_from.clone().or_else(|| {
            // A rename whose names come only from the `---` and `+++` lines.
            match (&self.old_path, &self.new_path) {
                (Some(old), Some(new)) if old != new => Some(old.clone()),
                _ => None,
            }
        }) {
            FileChange::Renamed { from }
        } else {
            FileChange::Modified
        };
        let path = self
            .new_path
            .or(self.renamed_to)
            .or(self.old_path)
            .unwrap_or(self.header_path);
        let content = if self.binary {
            Content::Binary
        } else {
            Content::Text(self.hunks)
        };
        FileDiff {
            path,
            change,
            content,
        }
    }
}

/// The files of a diff, from the output of `git diff` with [`ARGS`].
pub fn parse(output: &str) -> Vec<FileDiff> {
    let mut files = Vec::new();
    let mut file: Option<Reading> = None;
    let mut hunk: Option<HunkCursor> = None;
    for line in output.lines() {
        if let Some(header) = line.strip_prefix("diff --git ") {
            files.extend(file.take().map(Reading::finish));
            file = Some(Reading::new(header));
            hunk = None;
            continue;
        }
        let Some(reading) = file.as_mut() else {
            continue;
        };
        if let Some(cursor) = hunk.as_mut()
            && let Some(lines) = reading.hunks.last_mut().map(|hunk| &mut hunk.lines)
        {
            let (marker, text) = line.split_at(line.chars().next().map_or(0, char::len_utf8));
            let text = text.to_owned();
            match marker {
                "+" => {
                    lines.push(DiffLine::Added {
                        new: cursor.new,
                        text,
                    });
                    cursor.new += 1;
                    cursor.new_left = cursor.new_left.saturating_sub(1);
                }
                "-" => {
                    lines.push(DiffLine::Removed {
                        old: cursor.old,
                        text,
                    });
                    cursor.old += 1;
                    cursor.old_left = cursor.old_left.saturating_sub(1);
                }
                // `\ No newline at end of file`, after the line of either side it is about.
                "\\" => {}
                // An empty line is an empty context line, with `diff.suppressBlankEmpty`.
                _ => {
                    lines.push(DiffLine::Context {
                        old: cursor.old,
                        new: cursor.new,
                        text,
                    });
                    cursor.old += 1;
                    cursor.new += 1;
                    cursor.old_left = cursor.old_left.saturating_sub(1);
                    cursor.new_left = cursor.new_left.saturating_sub(1);
                }
            }
            if cursor.is_done() {
                hunk = None;
            }
            continue;
        }
        if let Some((cursor, heading)) = hunk_start(line) {
            reading.hunks.push(Hunk {
                heading,
                lines: Vec::new(),
            });
            hunk = (!cursor.is_done()).then_some(cursor);
        } else if let Some(rest) = line.strip_prefix("--- ") {
            reading.old_path = side_path(rest, "a/");
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            reading.new_path = side_path(rest, "b/");
        } else if line.starts_with("new file mode ") {
            reading.added = true;
        } else if line.starts_with("deleted file mode ") {
            reading.deleted = true;
        } else if let Some(from) = line.strip_prefix("rename from ") {
            reading.renamed_from = Some(unquote(from));
        } else if let Some(to) = line.strip_prefix("rename to ") {
            reading.renamed_to = Some(unquote(to));
        } else if line.starts_with("Binary files ") || line == "GIT binary patch" {
            reading.binary = true;
        }
        // Anything else, such as `index` or `similarity index`, says nothing the files need.
    }
    files.extend(file.map(Reading::finish));
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_have_their_numbers_on_both_sides() {
        let output = "\
diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -10,4 +10,4 @@ fn main() {
 keep
-old
+new
+more
 end
";
        let files = parse(output);
        assert_eq!(files.len(), 1);
        let file = &files[0];
        assert_eq!(file.path, "src/lib.rs");
        assert_eq!(file.change, FileChange::Modified);
        assert_eq!(file.counts(), (2, 1));
        let Content::Text(hunks) = &file.content else {
            panic!("a text diff: {file:?}");
        };
        assert_eq!(hunks[0].heading, "fn main() {");
        let numbers: Vec<_> = hunks[0]
            .lines
            .iter()
            .map(|line| (line.marker(), line.old(), line.new(), line.text()))
            .collect();
        assert_eq!(
            numbers,
            [
                (' ', Some(10), Some(10), "keep"),
                ('-', Some(11), None, "old"),
                ('+', None, Some(11), "new"),
                ('+', None, Some(12), "more"),
                (' ', Some(12), Some(13), "end"),
            ]
        );
    }

    #[test]
    fn added_deleted_renamed_and_binary_files_are_told_apart() {
        let output = "\
diff --git a/new.txt b/new.txt
new file mode 100644
index 0000000..1111111
--- /dev/null
+++ b/new.txt
@@ -0,0 +1 @@
+hello
\\ No newline at end of file
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
index 1111111..0000000
--- a/gone.txt
+++ /dev/null
@@ -1,2 +0,0 @@
-bye
\\ No newline at end of file
-really
diff --git a/old name.txt b/new name.txt
similarity index 100%
rename from old name.txt
rename to new name.txt
diff --git a/image.png b/image.png
index 1111111..2222222 100644
Binary files a/image.png and b/image.png differ
diff --git a/run me.sh b/run me.sh
old mode 100644
new mode 100755
";
        let files = parse(output);
        let summary: Vec<_> = files
            .iter()
            .map(|file| (file.path.as_str(), file.change.clone(), file.counts()))
            .collect();
        assert_eq!(
            summary,
            [
                ("new.txt", FileChange::Added, (1, 0)),
                ("gone.txt", FileChange::Deleted, (0, 2)),
                (
                    "new name.txt",
                    FileChange::Renamed {
                        from: "old name.txt".into()
                    },
                    (0, 0)
                ),
                ("image.png", FileChange::Modified, (0, 0)),
                ("run me.sh", FileChange::Modified, (0, 0)),
            ]
        );
        assert_eq!(files[3].content, Content::Binary);
        assert_eq!(files[4].content, Content::Text(Vec::new()));
    }

    #[test]
    fn quoted_paths_are_unquoted() {
        assert_eq!(unquote("\"tab\\there\""), "tab\there");
        assert_eq!(unquote("\"caf\\303\\251.txt\""), "café.txt");
        assert_eq!(unquote("plain.txt"), "plain.txt");
        assert_eq!(header_path("\"a/x\\\"y\" \"b/x\\\"y\""), "x\"y");
    }

    #[test]
    fn a_hunk_ends_after_as_many_lines_as_its_header_says() {
        let output = "\
diff --git a/a.txt b/a.txt
--- a/a.txt
+++ b/a.txt
@@ -1,2 +1,2 @@
-a
+b

@@ -9 +9 @@
-x
+y
";
        let files = parse(output);
        let Content::Text(hunks) = &files[0].content else {
            panic!("a text diff");
        };
        assert_eq!(hunks.len(), 2);
        // The empty line is context, as `diff.suppressBlankEmpty` writes it.
        assert_eq!(hunks[0].lines.len(), 3);
        assert_eq!(hunks[1].lines[0].old(), Some(9));
    }
}

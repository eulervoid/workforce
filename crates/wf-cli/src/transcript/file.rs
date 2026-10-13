//! Reading a transcript file while the agent still writes it.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::Context;
use wf_harness::TranscriptEntry;

/// Follows a transcript file: each read returns the entries written since the last one.
pub struct Reader {
    path: PathBuf,
    file: File,
    /// The start of a line whose end has not been written yet.
    pending: String,
    /// The start of a character whose other bytes have not been written yet.
    unfinished_bytes: Vec<u8>,
}

impl Reader {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let file = File::open(path).with_context(|| {
            format!(
                "opening {}; is this the machine the session ran on?",
                path.display()
            )
        })?;
        Ok(Self {
            path: path.to_owned(),
            file,
            pending: String::new(),
            unfinished_bytes: Vec::new(),
        })
    }

    /// The whole entries written since the last read; none when the agent wrote nothing new.
    pub fn read(&mut self) -> anyhow::Result<Vec<TranscriptEntry>> {
        let mut buffer = [0u8; 8192];
        loop {
            let read = self.file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            self.unfinished_bytes.extend_from_slice(&buffer[..read]);
            let valid = match std::str::from_utf8(&self.unfinished_bytes) {
                Ok(text) => text.len(),
                Err(error) => error.valid_up_to(),
            };
            self.pending
                .push_str(std::str::from_utf8(&self.unfinished_bytes[..valid])?);
            self.unfinished_bytes.drain(..valid);
        }
        let mut entries = Vec::new();
        while let Some(newline) = self.pending.find('\n') {
            let line: String = self.pending.drain(..=newline).collect();
            let entry: TranscriptEntry = serde_json::from_str(line.trim_end())
                .with_context(|| format!("reading {}", self.path.display()))?;
            entries.push(entry);
        }
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn entries_are_read_once_they_are_whole() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        let mut file = File::create(&path).unwrap();
        let mut reader = Reader::open(&path).unwrap();
        assert!(reader.read().unwrap().is_empty());
        let line = r#"{"at":"2026-10-14T00:00:00Z","direction":"agent_log","message":"ü"}"#;
        let bytes = line.as_bytes();
        // Cut inside the two bytes of `ü`.
        let cut = line.find('ü').unwrap() + 1;
        file.write_all(&bytes[..cut]).unwrap();
        assert!(reader.read().unwrap().is_empty());
        file.write_all(&bytes[cut..]).unwrap();
        file.write_all(b"\n").unwrap();
        let entries = reader.read().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].message, "ü");
        assert!(reader.read().unwrap().is_empty());
    }
}

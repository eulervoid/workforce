use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, bail};
use wf_harness::{Renderer, TranscriptEntry};

use crate::cli::{SessionCommand, SessionWatchArgs};

const FOLLOW_INTERVAL: Duration = Duration::from_millis(200);

pub fn session(json: bool, command: SessionCommand, _cwd: &Path) -> anyhow::Result<()> {
    if json {
        bail!("`wf session` has no --json output yet; the transcript file is JSON lines");
    }
    match command {
        SessionCommand::Watch(args) => watch(args),
    }
}

fn watch(args: SessionWatchArgs) -> anyhow::Result<()> {
    let mut file = File::open(&args.transcript)
        .with_context(|| format!("opening {}", args.transcript.display()))?;
    let mut renderer = Renderer::default();
    let mut pending = String::new();
    let mut buffer = [0u8; 8192];
    let mut unfinished_bytes = Vec::new();
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            if args.no_follow {
                return Ok(());
            }
            std::thread::sleep(FOLLOW_INTERVAL);
            continue;
        }
        unfinished_bytes.extend_from_slice(&buffer[..read]);
        let valid = match std::str::from_utf8(&unfinished_bytes) {
            Ok(text) => text.len(),
            Err(error) => error.valid_up_to(),
        };
        pending.push_str(std::str::from_utf8(&unfinished_bytes[..valid])?);
        unfinished_bytes.drain(..valid);
        while let Some(newline) = pending.find('\n') {
            let line: String = pending.drain(..=newline).collect();
            let entry: TranscriptEntry = serde_json::from_str(line.trim_end())
                .with_context(|| format!("reading {}", args.transcript.display()))?;
            anstream::print!("{}", renderer.render(&entry));
            std::io::stdout().flush()?;
        }
    }
}

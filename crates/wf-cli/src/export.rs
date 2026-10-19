//! `wf export` and `wf import`: the log as JSON lines, one event per line.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use anyhow::{Context, bail};
use data_encoding::BASE64;
use serde::{Deserialize, Serialize};
use wf_core::event::{Event, EventBody};
use wf_core::id::WorkforceId;
use wf_store::{RedbStore, Replica};

use crate::commands::emit;
use crate::workspace::{self, Workspace};
use wf_daemon::DataDir;

/// One line of an export. `content` is there to be read; `event` is what an import reads.
#[derive(Serialize, Deserialize)]
struct ExportedEvent {
    id: String,
    kind: String,
    content: serde_json::Value,
    /// The signed event in its canonical encoding, base64.
    event: String,
}

pub fn export(workspace: &Workspace) -> anyhow::Result<()> {
    let mut out = std::io::stdout().lock();
    for event in workspace.events()? {
        let line = ExportedEvent {
            id: event.id().to_string(),
            kind: event.content.body.name().to_string(),
            content: readable(&event)?,
            event: BASE64.encode(&event.encode()?),
        };
        serde_json::to_writer(&mut out, &line)?;
        out.write_all(b"\n")?;
    }
    Ok(())
}

/// The signed content with the body as its fields, where the wire form has its encoding.
fn readable(event: &Event) -> anyhow::Result<serde_json::Value> {
    struct Payload<'a>(&'a EventBody);
    impl Serialize for Payload<'_> {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            self.0.serialize_payload(serializer)
        }
    }
    let mut content = serde_json::to_value(&event.content)?;
    content["body"] = serde_json::to_value(Payload(&event.content.body))?;
    Ok(content)
}

#[derive(Serialize)]
struct ImportView {
    workforce: String,
    read: usize,
    applied: usize,
    rejected: usize,
}

pub fn import(json: bool, file: &Path, data: &DataDir) -> anyhow::Result<()> {
    if std::os::unix::net::UnixStream::connect(data.daemon_socket()).is_ok() {
        bail!("the daemon owns the stores while it runs; stop it first with `wf daemon stop`");
    }
    let reader = BufReader::new(
        std::fs::File::open(file).with_context(|| format!("opening {}", file.display()))?,
    );
    let mut events = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let number = index + 1;
        let exported: ExportedEvent = serde_json::from_str(&line)
            .with_context(|| format!("line {number} is not an exported event"))?;
        let bytes = BASE64
            .decode(exported.event.as_bytes())
            .with_context(|| format!("the event on line {number} is not base64"))?;
        let event = Event::decode(&bytes)
            .with_context(|| format!("the event on line {number} does not decode"))?;
        events.push(event);
    }
    let Some(genesis) = events
        .iter()
        .find(|event| matches!(event.content.body, EventBody::Genesis(_)))
    else {
        bail!(
            "{} holds no genesis event, so not a whole workforce",
            file.display()
        );
    };
    let workforce = WorkforceId::from(genesis.id());
    if let EventBody::Genesis(signed) = &genesis.content.body
        && signed.statement.forked_from.is_some()
    {
        bail!("importing a fork is not supported yet");
    }
    let read = events.len();
    let store = RedbStore::open(workspace::store_path(data, &workforce)?)?;
    let mut replica = Replica::open(store, workforce)?;
    let received = replica.receive_batch(events)?;
    emit(
        json,
        &ImportView {
            workforce: workforce.to_string(),
            read,
            applied: received.applied,
            rejected: received.rejected,
        },
        |view| {
            let mut out = format!(
                "workforce {}: {} of {} events were new",
                view.workforce, view.applied, view.read
            );
            if view.rejected > 0 {
                out.push_str(&format!(", {} were rejected", view.rejected));
            }
            out.push('\n');
            out
        },
    )
}

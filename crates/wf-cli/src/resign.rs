//! `wf import --resign`: the export of a workforce of event version 1 signed again as a new
//! workforce of the current version, with what this machine keeps of it by id moved over. The
//! directory of the old workforce is left as it is, as the backup. Delete this module with
//! `wf_core::resign` once no log of version 1 is left.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::{Context, bail};
use serde::Serialize;
use wf_core::id::{ItemId, SessionId};
use wf_core::resign::{Export, ExportedEvent, Resigned};
use wf_store::{RedbStore, Replica};

use crate::commands::emit;
use crate::fork::Unfinished;
use crate::workspace::{self, WORKFORCE_FILE, WorkforceFile};
use wf_daemon::DataDir;

/// The file beside the new store that maps every old id to its new one.
pub const ID_MAP_FILE: &str = "resigned-ids.tsv";

/// What the file of the record of a session ends with, after its id and a dot.
const RECORD_SUFFIX: &str = "agent-session.json";

/// What the directory of a workforce holds that is not carried over, and is not worth saying so:
/// the store and the key, which the new workforce has its own of, and what is made again.
const NOT_CARRIED: [&str; 5] = [
    "store.redb",
    "principal.key",
    "check",
    "sandbox",
    "worktrees",
];

#[derive(Serialize)]
pub struct ResignView {
    pub old_workforce: String,
    pub workforce: String,
    pub events: usize,
    /// The events not carried over, by kind.
    pub dropped_events: BTreeMap<String, usize>,
    pub id_map: String,
    /// What was moved to the new workforce, by where it is now.
    pub moved: Vec<String>,
    /// What was not, and why.
    pub dropped: Vec<String>,
    /// Whether the `.workforce.toml` of this checkout now names the new workforce.
    pub workforce_file: bool,
}

pub fn import(json: bool, file: &Path, data: &DataDir, cwd: &Path) -> anyhow::Result<()> {
    data.require_no_daemon("signing a workforce again")?;
    let export = read(file)?;
    let old = export.workforce()?;
    let principal = data.principal(&old)?.with_context(|| {
        format!(
            "this machine has no principal key of workforce {old}; sign it again on the device \
             that founded it"
        )
    })?;
    let device = data.device()?.context("no device key on this machine")?;
    let resigned = export.resign(&device, &principal)?;
    let new = resigned.workforce;
    let directory = data.workforce(&new);
    if directory.exists() {
        bail!("{} exists already", directory.display());
    }
    let mut unfinished = Unfinished::new(directory.clone());

    data.store_principal(&new, &principal)?;
    let store = RedbStore::open(workspace::store_path(data, &new)?)?;
    let mut replica = Replica::open(store, new)?;
    let received = replica.receive_batch(resigned.events.clone())?;
    if received.applied != resigned.events.len() {
        bail!(
            "the store took {} of {} events signed again",
            received.applied,
            resigned.events.len()
        );
    }
    let target = replica
        .state()
        .policy()
        .context("the new workforce has no policy")?
        .policy
        .merge
        .target
        .clone();
    drop(replica);
    let id_map = directory.join(ID_MAP_FILE);
    std::fs::write(&id_map, id_map_text(&resigned))?;

    let (workforce_file, checkout) = match workspace::find_workforce(cwd) {
        Ok((path, id)) if id == old => (Some(path), wf_vcs::GitCli::checkout_root(cwd).ok()),
        Ok(_) | Err(_) => (None, None),
    };
    let mut moved_data = Moved::default();
    move_repository(
        data,
        &resigned,
        &target,
        checkout.as_deref(),
        &mut moved_data,
    )?;
    move_agent_repositories(data, &resigned, &mut moved_data)?;
    move_transcripts(data, &resigned, &mut moved_data)?;
    for name in ["check-logs", "checks-passed"] {
        let from = data.workforce(&old).join(name);
        if from.is_dir() {
            copy_tree(&from, &directory.join(name))?;
            moved_data.moved.push(name.to_string());
        }
    }
    for entry in std::fs::read_dir(data.workforce(&old))? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        let carried = [
            "git",
            "repos",
            "transcripts",
            "transcript-copies",
            "check-logs",
            "checks-passed",
        ];
        if !carried.contains(&name.as_str()) && !NOT_CARRIED.contains(&name.as_str()) {
            moved_data
                .dropped
                .push(format!("{name}: not something wf keeps by id"));
        }
    }
    if let Some(path) = &workforce_file {
        std::fs::write(path, toml::to_string(&WorkforceFile { id: new })?)?;
    }
    unfinished.keep();

    emit(
        json,
        &ResignView {
            old_workforce: old.to_string(),
            workforce: new.to_string(),
            events: resigned.events.len(),
            dropped_events: resigned.dropped.clone(),
            id_map: id_map.display().to_string(),
            moved: moved_data.moved,
            dropped: moved_data.dropped,
            workforce_file: workforce_file.is_some(),
        },
        |view| {
            let mut out = format!(
                "signed {} events again as workforce {}, from {}\n",
                view.events, view.workforce, view.old_workforce
            );
            if !view.dropped_events.is_empty() {
                let kinds: Vec<String> = view
                    .dropped_events
                    .iter()
                    .map(|(kind, count)| format!("{count} {kind}"))
                    .collect();
                out.push_str(&format!("left out: {}\n", kinds.join(", ")));
            }
            out.push_str(&format!("old ids and their new ones: {}\n", view.id_map));
            for moved in &view.moved {
                out.push_str(&format!("moved {moved}\n"));
            }
            for dropped in &view.dropped {
                out.push_str(&format!("not moved {dropped}\n"));
            }
            if view.workforce_file {
                out.push_str(&format!(
                    "{WORKFORCE_FILE} and the remote `{}` of this checkout now name the new \
                     workforce\n",
                    wf_vcs::REMOTE
                ));
            } else {
                out.push_str(&format!(
                    "point {WORKFORCE_FILE} of the checkout at {} and run `wf daemon start`\n",
                    view.workforce
                ));
            }
            out.push_str(&format!(
                "the directory of {} is left as it was, as the backup\n",
                view.old_workforce
            ));
            out
        },
    )
}

fn read(file: &Path) -> anyhow::Result<Export> {
    let reader = BufReader::new(
        std::fs::File::open(file).with_context(|| format!("opening {}", file.display()))?,
    );
    let mut events = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let exported: ExportedEvent = serde_json::from_str(&line)
            .with_context(|| format!("line {} is not an exported event", index + 1))?;
        events.push(exported);
    }
    Ok(Export::read(events)?)
}

/// One line per id, the old one, the new one and the kind of event it names, after one line for
/// the workforce.
fn id_map_text(resigned: &Resigned) -> String {
    let mut text = format!(
        "{}\t{}\tWorkforce\n",
        resigned.old_workforce, resigned.workforce
    );
    for (old, (new, kind)) in &resigned.renamed {
        text.push_str(&format!("{old}\t{new}\t{kind}\n"));
    }
    text
}

#[derive(Default)]
struct Moved {
    moved: Vec<String>,
    dropped: Vec<String>,
}

/// The repository of the workforce, cloned with all its objects, its item branches renamed after
/// their items. It becomes the remote of `checkout`.
fn move_repository(
    data: &DataDir,
    resigned: &Resigned,
    target: &str,
    checkout: Option<&Path>,
    moved: &mut Moved,
) -> anyhow::Result<()> {
    let old = wf_vcs::bare_repository(&data.workforce(&resigned.old_workforce));
    if !old.join("HEAD").is_file() {
        return Ok(());
    }
    let new = wf_vcs::GitCli::create_fork_repository(
        &wf_vcs::bare_repository(&data.workforce(&resigned.workforce)),
        target,
        &old,
        checkout,
    )?;
    for (old_item, new_item) in resigned.items() {
        new.rename_branch(
            &wf_vcs::item_branch(old_item),
            &wf_vcs::item_branch(new_item),
        )?;
    }
    moved
        .moved
        .push("git, with the branch of each item renamed".to_string());
    Ok(())
}

/// The repository of each item; the directories of the sessions on items, which end with their
/// session, are not moved.
fn move_agent_repositories(
    data: &DataDir,
    resigned: &Resigned,
    moved: &mut Moved,
) -> anyhow::Result<()> {
    let from = data.repositories(&resigned.old_workforce);
    if !from.is_dir() {
        return Ok(());
    }
    let items: BTreeMap<String, (ItemId, ItemId)> = resigned
        .items()
        .map(|(old, new)| (wf_vcs::item_name(old), (old, new)))
        .collect();
    let bare = wf_vcs::GitCli::open(&wf_vcs::bare_repository(
        &data.workforce(&resigned.workforce),
    ))
    .ok();
    for entry in std::fs::read_dir(&from)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        match (items.get(&name), &bare) {
            (Some((old, new)), Some(bare)) => {
                let path = data
                    .repositories(&resigned.workforce)
                    .join(wf_vcs::item_name(*new));
                copy_tree(&entry.path(), &path)?;
                bare.adopt_agent_repository(&path, *old, *new)?;
                moved
                    .moved
                    .push(format!("repos/{name} to repos/{}", wf_vcs::item_name(*new)));
            }
            (Some(_), None) => moved
                .dropped
                .push(format!("repos/{name}: the workforce has no repository")),
            (None, _) => moved
                .dropped
                .push(format!("repos/{name}: not the repository of an item")),
        }
    }
    Ok(())
}

/// The transcripts of sessions, with the records beside them, and the copies of transcripts of
/// sessions on other devices.
fn move_transcripts(data: &DataDir, resigned: &Resigned, moved: &mut Moved) -> anyhow::Result<()> {
    let sessions: BTreeMap<SessionId, SessionId> = resigned.sessions().collect();
    let (old, new) = (resigned.old_workforce, resigned.workforce);
    let mut count = 0;
    for (name, from, to) in [
        (
            "transcripts",
            data.transcripts(&old),
            data.transcripts(&new),
        ),
        (
            "transcript-copies",
            data.transcript_copies(&old),
            data.transcript_copies(&new),
        ),
    ] {
        if !from.is_dir() {
            continue;
        }
        std::fs::create_dir_all(&to)?;
        for entry in std::fs::read_dir(&from)? {
            let file = entry?.file_name().to_string_lossy().into_owned();
            let renamed = file.split_once('.').and_then(|(session, rest)| {
                let session: SessionId = session.parse().ok()?;
                Some((session, *sessions.get(&session)?, rest))
            });
            let Some((old_session, new_session, rest)) = renamed else {
                moved
                    .dropped
                    .push(format!("{name}/{file}: not of a session of the export"));
                continue;
            };
            if rest == RECORD_SUFFIX {
                wf_daemon::copy_session_record(&from, old_session, &to, new_session, |comment| {
                    resigned.comment(comment)
                })?;
            } else {
                std::fs::copy(from.join(&file), to.join(format!("{new_session}.{rest}")))?;
            }
            count += 1;
        }
    }
    if count > 0 {
        moved
            .moved
            .push(format!("{count} files of transcripts and session records"));
    }
    Ok(())
}

/// Copies the directory `from` to `to`, links as links. On a file system that can, each file is
/// a clone that shares its blocks until either is written.
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let (source, destination) = (entry.path(), to.join(entry.file_name()));
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_tree(&source, &destination)?;
        } else if kind.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(&source)?, &destination)?;
        } else if kind.is_file() {
            std::fs::copy(&source, &destination)?;
        }
    }
    Ok(())
}

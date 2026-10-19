//! The caches in the worktrees of a workforce, such as build output, which the daemon keeps under
//! the budget of `[storage]` in the machine config. It does not know what made them: a deleted
//! cache only costs a cold build the next time.

use std::collections::{BTreeSet, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// The disk the caches in the worktrees of a workforce take on this machine, and the budget the
/// daemon keeps them under.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct CacheUsage {
    pub used_bytes: u64,
    pub budget_bytes: u64,
}

/// The caches of one worktree, measured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeCaches {
    pub worktree: PathBuf,
    /// The disk they take, each file counted once even when it has several links.
    pub bytes: u64,
    /// When anything in them was last written, such as by a build.
    pub last_used: SystemTime,
}

/// The caches of each directory in `worktrees` that has any, where `caches` are the paths of
/// caches inside a worktree. A file linked from several places, as cargo links its outputs, is
/// counted once.
pub fn measure(worktrees: &Path, caches: &[PathBuf]) -> Vec<WorktreeCaches> {
    let Ok(entries) = std::fs::read_dir(worktrees) else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    let mut measured = Vec::new();
    for entry in entries.flatten() {
        let worktree = entry.path();
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let mut found = None;
        for cache in caches {
            for file in walkdir::WalkDir::new(worktree.join(cache))
                .into_iter()
                .flatten()
            {
                let Ok(metadata) = file.metadata() else {
                    continue;
                };
                if metadata.nlink() > 1 && !seen.insert((metadata.dev(), metadata.ino())) {
                    continue;
                }
                let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                let (bytes, last_used) = found.get_or_insert((0, modified));
                *bytes += metadata.blocks() * 512;
                *last_used = (*last_used).max(modified);
            }
        }
        if let Some((bytes, last_used)) = found {
            measured.push(WorktreeCaches {
                worktree,
                bytes,
                last_used,
            });
        }
    }
    measured
}

/// The disk all `measured` caches take.
pub fn total(measured: &[WorktreeCaches]) -> u64 {
    measured.iter().map(|caches| caches.bytes).sum()
}

/// The worktrees whose caches to delete, least recently used first, until what is left fits
/// `budget`. The caches of worktrees in `busy`, which sessions run in, are never deleted.
pub fn over_budget<'a>(
    measured: &'a [WorktreeCaches],
    budget: u64,
    busy: &BTreeSet<PathBuf>,
) -> Vec<&'a Path> {
    let mut used = total(measured);
    let mut idle: Vec<&WorktreeCaches> = measured
        .iter()
        .filter(|caches| !busy.contains(&caches.worktree))
        .collect();
    idle.sort_by_key(|caches| caches.last_used);
    let mut deleted = Vec::new();
    for caches in idle {
        if used <= budget {
            break;
        }
        used -= caches.bytes;
        deleted.push(caches.worktree.as_path());
    }
    deleted
}

/// Deletes the caches of `worktree`, whole.
pub fn delete(worktree: &Path, caches: &[PathBuf]) -> std::io::Result<()> {
    for cache in caches {
        let path = worktree.join(cache);
        let removed = match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(&path),
            Ok(_) => std::fs::remove_file(&path),
            Err(error) => Err(error),
        };
        match removed {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn caches(worktree: &str, bytes: u64, last_used_secs: u64) -> WorktreeCaches {
        WorktreeCaches {
            worktree: PathBuf::from(worktree),
            bytes,
            last_used: SystemTime::UNIX_EPOCH + Duration::from_secs(last_used_secs),
        }
    }

    #[test]
    fn the_least_recently_used_idle_caches_go_until_the_rest_fits() {
        let measured = [
            caches("/w/new", 40, 300),
            caches("/w/busy", 50, 100),
            caches("/w/old", 30, 200),
            caches("/w/older", 10, 150),
        ];
        let busy = BTreeSet::from([PathBuf::from("/w/busy")]);
        assert_eq!(over_budget(&measured, 200, &busy), Vec::<&Path>::new());
        assert_eq!(over_budget(&measured, 125, &busy), [Path::new("/w/older")]);
        assert_eq!(
            over_budget(&measured, 100, &busy),
            [Path::new("/w/older"), Path::new("/w/old")]
        );
        assert_eq!(
            over_budget(&measured, 0, &busy),
            [
                Path::new("/w/older"),
                Path::new("/w/old"),
                Path::new("/w/new")
            ],
            "a busy worktree keeps its caches even over the budget"
        );
    }

    #[test]
    fn caches_are_measured_per_worktree_and_deleted_whole() {
        let root = tempfile::tempdir().unwrap();
        let worktrees = root.path().join("worktrees");
        let one = worktrees.join("one");
        std::fs::create_dir_all(one.join("target/debug")).unwrap();
        std::fs::create_dir_all(one.join("src")).unwrap();
        std::fs::write(one.join("target/debug/app"), vec![1; 64 * 1024]).unwrap();
        std::fs::hard_link(one.join("target/debug/app"), one.join("target/app")).unwrap();
        std::fs::write(one.join("src/main.rs"), vec![1; 64 * 1024]).unwrap();
        std::fs::create_dir_all(worktrees.join("bare/src")).unwrap();
        let caches = [PathBuf::from("target"), PathBuf::from("web/node_modules")];

        let measured = measure(&worktrees, &caches);
        assert_eq!(measured.len(), 1, "{measured:?}");
        assert_eq!(measured[0].worktree, one);
        let bytes = measured[0].bytes;
        assert!(
            (64 * 1024..2 * 64 * 1024).contains(&bytes),
            "the linked file counts once and the source not at all: {bytes}"
        );

        delete(&one, &caches).unwrap();
        assert!(!one.join("target").exists());
        assert!(one.join("src/main.rs").exists());
        assert!(measure(&worktrees, &caches).is_empty());
    }
}

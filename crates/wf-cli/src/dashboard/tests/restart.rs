//! Dashboard tests: what a daemon restart calls for.

use crate::dashboard::restart::{Executable, Mismatch, on_mismatch};

#[test]
fn a_newer_daemon_is_met_by_the_rebuilt_executable() {
    assert_eq!(on_mismatch(18, 17, true), Mismatch::RunRebuilt);
}

#[test]
fn a_newer_daemon_with_the_same_executable_says_to_rebuild() {
    assert_eq!(on_mismatch(18, 17, false), Mismatch::RebuildAndStart);
}

#[test]
fn an_older_daemon_says_to_restart_it_even_when_the_executable_changed() {
    assert_eq!(on_mismatch(16, 17, false), Mismatch::RestartDaemon);
    assert_eq!(on_mismatch(16, 17, true), Mismatch::RestartDaemon);
}

#[test]
fn an_executable_replaced_by_a_build_is_changed() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("wf");
    std::fs::write(&path, "old").unwrap();
    let executable = Executable::at_path(path.clone());
    assert!(!executable.changed());
    // A build writes a new file and renames it over the old one, as a linker does.
    let rebuilt = directory.path().join("wf.new");
    std::fs::write(&rebuilt, "new").unwrap();
    std::fs::rename(&rebuilt, &path).unwrap();
    assert!(executable.changed());
}

#[test]
fn a_deleted_suffix_from_proc_is_trimmed() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("wf");
    std::fs::write(&path, "old").unwrap();
    let executable = Executable::at_path(directory.path().join("wf (deleted)"));
    assert_eq!(executable.path(), path);
}

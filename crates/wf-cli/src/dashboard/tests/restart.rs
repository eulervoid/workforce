//! Dashboard tests: what a daemon restart calls for.

use crate::dashboard::restart::{Mismatch, on_mismatch};

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

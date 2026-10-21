//! The daemon raises its open-file limit as it starts, so that the sessions, checks and harnesses
//! it spawns inherit it. A daemon started from a terminal on macOS has a soft limit of 256, which
//! many sessions, connections and git processes outgrow.

/// The most open files the daemon asks for, however high the hard limit.
const MOST_OPEN_FILES: u64 = 65536;

/// Raises the soft open-file limit of this process to its hard limit, but no higher than
/// [`MOST_OPEN_FILES`] (and on macOS `kern.maxfilesperproc`); logs it if it cannot.
pub fn raise_open_file_limit() {
    match rlimit::increase_nofile_limit(MOST_OPEN_FILES) {
        Ok(limit) => tracing::info!(limit, "the daemon may open this many files"),
        Err(error) => tracing::warn!(
            %error,
            "the daemon could not raise its open-file limit; it and its sessions keep the one it started with"
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    /// Set for the child process the test starts with a low soft limit.
    const CHILD: &str = "WF_TEST_OPEN_FILES_CHILD";
    const LOW: u64 = 256;

    #[test]
    fn a_child_of_the_daemon_inherits_the_raised_limit() {
        let (_, hard) = rlimit::Resource::NOFILE.get().unwrap();
        if hard <= LOW {
            return;
        }
        let test_binary = std::env::current_exe().unwrap();
        let output = Command::new("sh")
            .arg("-c")
            .arg(format!("ulimit -Sn {LOW} && exec \"$0\" \"$@\""))
            .arg(test_binary)
            .args([
                "--exact",
                "open_files::tests::raises_the_limit_in_a_process_that_starts_low",
                "--include-ignored",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "the child failed:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "the child did not run the test:\n{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    /// Runs only as the child of the test above.
    #[test]
    #[ignore = "runs as the child of a_child_of_the_daemon_inherits_the_raised_limit"]
    fn raises_the_limit_in_a_process_that_starts_low() {
        if std::env::var_os(CHILD).is_none() {
            return;
        }
        let (soft, hard) = rlimit::Resource::NOFILE.get().unwrap();
        assert_eq!(soft, LOW);

        super::raise_open_file_limit();

        let (raised, _) = rlimit::Resource::NOFILE.get().unwrap();
        assert!(raised > LOW, "the soft limit stayed at {raised}");
        assert!(raised <= hard.min(super::MOST_OPEN_FILES));
        let inherited = Command::new("sh")
            .args(["-c", "ulimit -Sn"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&inherited.stdout).trim(),
            raised.to_string()
        );
    }
}

//! What a program the daemon runs inherits of it: stdin, stdout and stderr, nothing else.

/// Has `command` inherit no descriptor beyond stdin, stdout and stderr. On macOS std creates
/// pipes and sockets close-on-exec in two steps, as there is no `pipe2`; a program spawned on one
/// thread in between inherits what another thread opened, and holds a pipe some other process
/// reads from open until it exits. Agents are started by `wf_harness`, which sees to it there.
pub(crate) fn only_stdio(command: &mut tokio::process::Command) {
    // SAFETY: setting close-on-exec only calls functions that are async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            close_fds::set_fds_cloexec(3, &[]);
            Ok(())
        });
    }
}

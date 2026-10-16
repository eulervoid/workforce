//! `wf daemon`: runs the daemon of this machine, in this terminal or in the background.

use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use rustix::process::{Pid, Signal};
use serde::Serialize;
use wf_net::Network;

use crate::cli::{DaemonArgs, DaemonCommand};
use crate::commands::emit;
use crate::data_dir::DataDir;
use crate::network::network_for;

/// How long `wf daemon start` waits for the daemon to answer.
const START_WAIT: Duration = Duration::from_secs(30);
/// How long `wf daemon stop` waits for the daemon to exit.
const STOP_WAIT: Duration = Duration::from_secs(45);
/// What the daemon logs unless `WF_LOG` says otherwise.
const DEFAULT_LOG_FILTER: &str = "warn,wf_daemon=info,wf_net=info,wf_cli=info";

pub fn daemon(json: bool, command: DaemonCommand, data: DataDir) -> anyhow::Result<()> {
    match command {
        DaemonCommand::Start(args) => start(json, args, &data),
        DaemonCommand::Stop => stop(json, &data),
        DaemonCommand::Status => emit(json, &status(&data), render_status),
        DaemonCommand::Log => log(&data),
        DaemonCommand::Run(args) => run(json, args, data),
    }
}

#[derive(Serialize)]
struct DaemonStatus {
    running: bool,
    /// Only known for a daemon that runs.
    pid: Option<u32>,
    socket: PathBuf,
    log: PathBuf,
}

fn status(data: &DataDir) -> DaemonStatus {
    let socket = data.daemon_socket();
    let running = answers(&socket);
    DaemonStatus {
        running,
        pid: running.then(|| running_pid(data)).flatten(),
        socket,
        log: data.daemon_log(),
    }
}

fn render_status(status: &DaemonStatus) -> String {
    match (status.running, status.pid) {
        (true, Some(pid)) => format!(
            "the daemon runs (pid {pid}); it logs to {}\n",
            status.log.display()
        ),
        (true, None) => format!(
            "the daemon runs in a terminal, listening on {}\n",
            status.socket.display()
        ),
        (false, _) => "the daemon does not run; start it with `wf daemon start`\n".to_string(),
    }
}

fn log(data: &DataDir) -> anyhow::Result<()> {
    let path = data.daemon_log();
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => bail!(
            "there is no daemon log yet at {}; `wf daemon start` writes it",
            path.display()
        ),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    crate::interactive::page(&String::from_utf8_lossy(&bytes))
}

fn start(json: bool, args: DaemonArgs, data: &DataDir) -> anyhow::Result<()> {
    if answers(&data.daemon_socket()) {
        bail!("the daemon already runs; `wf daemon status` says where it logs");
    }
    std::fs::create_dir_all(data.root())
        .with_context(|| format!("creating {}", data.root().display()))?;
    let log_path = data.daemon_log();
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;
    let mut command = Command::new(std::env::current_exe().context("finding the wf binary")?);
    command.args(["daemon", "run"]);
    if let Some(relay) = &args.relay {
        command.args(["--relay", relay]);
    }
    if args.no_network {
        command.arg("--no-network");
    }
    // Its own process group, so an interrupt in this terminal does not stop it.
    command
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .process_group(0);
    // The daemon outlives this command; nothing waits for it here.
    #[allow(clippy::zombie_processes)]
    let mut child = command.spawn().context("starting the daemon")?;
    let deadline = Instant::now() + START_WAIT;
    while !answers(&data.daemon_socket()) {
        if let Some(exit) = child.try_wait()? {
            bail!(
                "the daemon stopped as it started ({exit}); the end of {}:\n{}",
                log_path.display(),
                tail(&log_path, 20)
            );
        }
        if Instant::now() > deadline {
            bail!(
                "the daemon did not answer within {}s; see {}",
                START_WAIT.as_secs(),
                log_path.display()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    emit(json, &status(data), render_status)
}

fn stop(json: bool, data: &DataDir) -> anyhow::Result<()> {
    let Some(pid) = running_pid(data) else {
        if answers(&data.daemon_socket()) {
            bail!("the daemon runs in a terminal; interrupt it there");
        }
        return emit(json, &status(data), render_status);
    };
    rustix::process::kill_process(pid_of(pid)?, Signal::TERM)
        .with_context(|| format!("stopping the daemon (pid {pid})"))?;
    let deadline = Instant::now() + STOP_WAIT;
    while alive(pid) {
        if Instant::now() > deadline {
            bail!(
                "the daemon (pid {pid}) did not stop within {}s",
                STOP_WAIT.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    emit(json, &status(data), render_status)
}

fn run(json: bool, args: DaemonArgs, data: DataDir) -> anyhow::Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_env("WF_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(DEFAULT_LOG_FILTER));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();
    let mut panics = wf_daemon::watch_panics();

    let device = data
        .device(false)?
        .context("no device key; run `wf init` or `wf join` first")?;
    let socket = data.daemon_socket();
    if answers(&socket) {
        bail!("a daemon is already listening on {}", socket.display());
    }
    let config = wf_daemon::Config {
        data_dir: data.root().to_path_buf(),
        socket: socket.clone(),
        network: if args.no_network {
            Network::Direct
        } else {
            network_for(args.relay.as_deref())?
        },
        harnesses: Default::default(),
        machine_config: wf_daemon::machine_config::MachineConfig::path_from_env(),
        wf_binary: Some(std::env::current_exe().context("finding the wf binary")?),
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let pid_file = data.daemon_pid();
    runtime.block_on(async {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let daemon = wf_daemon::Daemon::start(config, device.clone()).await?;
        std::fs::write(&pid_file, std::process::id().to_string())
            .with_context(|| format!("writing {}", pid_file.display()))?;
        tracing::info!(
            device = %device.key(),
            socket = %socket.display(),
            "the daemon runs"
        );
        emit(
            json,
            &DaemonView {
                device: device.key().to_string(),
                socket: socket.display().to_string(),
            },
            |view| {
                format!(
                    "daemon of device {} listening on {}; interrupt to stop\n",
                    view.device, view.socket
                )
            },
        )?;
        let mut panicked = None;
        let served = std::sync::Arc::clone(&daemon)
            .serve(async {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = terminate.recv() => {}
                    report = panics.first() => panicked = Some(report),
                }
            })
            .await;
        tracing::info!("the daemon stops; its sessions resume when it starts again");
        daemon.halt().await;
        if read_pid(&pid_file) == Some(std::process::id()) {
            let _ = std::fs::remove_file(&pid_file);
        }
        served?;
        match panicked {
            Some(report) => bail!("the daemon stopped because a task of it panicked: {report}"),
            None => Ok(()),
        }
    })
}

#[derive(Serialize)]
struct DaemonView {
    device: String,
    socket: String,
}

/// Whether a daemon listens on `socket`.
fn answers(socket: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket).is_ok()
}

/// The process id in the pid file, if that process still runs.
fn running_pid(data: &DataDir) -> Option<u32> {
    read_pid(&data.daemon_pid()).filter(|pid| alive(*pid))
}

fn read_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn alive(pid: u32) -> bool {
    pid_of(pid).is_ok_and(|pid| rustix::process::test_kill_process(pid).is_ok())
}

fn pid_of(pid: u32) -> anyhow::Result<Pid> {
    i32::try_from(pid)
        .ok()
        .and_then(Pid::from_raw)
        .with_context(|| format!("{pid} is not a process id"))
}

/// The last `lines` lines of the file at `path`.
fn tail(path: &Path, lines: usize) -> String {
    let Ok(file) = std::fs::File::open(path) else {
        return String::new();
    };
    let all: Vec<String> = BufReader::new(file).lines().map_while(Result::ok).collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

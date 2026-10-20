use wf_daemon::DataDir;

fn main() {
    let cli = wf_cli::cli::parse();
    // Runs inside `git fetch`, whose stderr is the daemon's log: no theme, config or data needed.
    if matches!(cli.command, Some(wf_cli::cli::Command::GitBridge)) {
        if let Err(error) = wf_daemon::run_git_bridge() {
            eprintln!("error: {error:#}");
            std::process::exit(1);
        }
        return;
    }
    wf_cli::theme::load();
    let result = (|| {
        let data = DataDir::from_env()?;
        let cwd = std::env::current_dir()?;
        wf_cli::commands::run(cli, data, &cwd)
    })();
    if let Err(error) = result {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

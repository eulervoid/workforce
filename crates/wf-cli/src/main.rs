use wf_daemon::DataDir;

fn main() {
    let cli = wf_cli::cli::parse();
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

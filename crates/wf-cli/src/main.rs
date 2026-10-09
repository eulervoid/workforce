use clap::Parser;
use wf_cli::cli::Cli;
use wf_cli::keys::DataDir;

fn main() {
    let cli = Cli::parse();
    let result = DataDir::from_env().and_then(|data| {
        let cwd = std::env::current_dir()?;
        wf_cli::commands::run(cli, data, &cwd)
    });
    if let Err(error) = result {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

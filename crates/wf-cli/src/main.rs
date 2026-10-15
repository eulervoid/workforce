use wf_cli::data_dir::DataDir;

fn main() {
    let cli = wf_cli::cli::parse();
    wf_cli::theme::load();
    let result = DataDir::from_env().and_then(|data| {
        let cwd = std::env::current_dir()?;
        wf_cli::commands::run(cli, data, &cwd)
    });
    if let Err(error) = result {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

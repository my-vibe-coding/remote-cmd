mod cli;
mod client;
mod common;
mod embed;
mod server;

use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = cli::Cli::parse();
    match cli.command {
        Some(cli::Command::Serve {
            port,
            password,
            data_dir,
        }) => server::serve(port, password, data_dir).await,
        Some(cli::Command::Run { alias, args }) => client::run_cmd::run(&alias, &args).await,
        Some(cli::Command::GenScript { alias }) => client::script::gen_scripts(alias.as_deref()),
        None => client::start().await,
    }
}

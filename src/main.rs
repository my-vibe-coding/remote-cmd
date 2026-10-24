mod cli;
mod client;
mod common;
mod embed;
mod server;

use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 别名 shim 模式：bin 目录下以别名命名的 exe 副本（脚本生成功能创建），
    // 必须在 clap 解析之前拦截，避免别名参数被误认为选项
    if let Some(alias) = client::script::shim_alias() {
        let args: Vec<String> = std::env::args().skip(1).collect();
        return client::run_cmd::run(&alias, &args).await;
    }

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

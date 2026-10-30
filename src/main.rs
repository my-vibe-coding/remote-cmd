mod cli;
mod client;
mod common;
mod embed;
mod server;

use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 联调诊断：panic 全量落盘（PTY 场景 stderr 不可见；进程无声死亡时用于区分 panic 与外部击杀）
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        crate::dlog!("PANIC: {info}");
        default_hook(info);
    }));

    // 自更新清理模式：删除上一版本备份。旧进程退出后文件锁才释放，需重试删除。
    // 由 update 流程 spawn 新二进制时触发，参数为待删除的备份文件路径。
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--self-cleanup") {
        let paths: Vec<String> = args
            .iter()
            .skip(1)
            .filter(|a| a.as_str() != "--self-cleanup")
            .cloned()
            .collect();
        common::self_update::self_cleanup(&paths);
        return Ok(());
    }

    // 别名 shim 模式：bin 目录下以别名命名的 exe 副本（脚本生成功能创建），
    // 必须在 clap 解析之前拦截，避免别名参数被误认为选项
    if let Some(alias) = client::script::shim_alias() {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let res = client::run_cmd::run(&alias, &args, false).await;
        if let Err(e) = &res {
            crate::dlog!("[main] shim run 失败: {e:#}");
        }
        return res;
    }

    let cli = cli::Cli::parse();
    match cli.command {
        Some(cli::Command::Serve {
            port,
            password,
            data_dir,
        }) => server::serve(port, password, data_dir).await,
        Some(cli::Command::Run { alias, args }) => {
            let res = client::run_cmd::run(&alias, &args, true).await;
            if let Err(e) = &res {
                crate::dlog!("[main] run 失败: {e:#}");
            }
            res
        }
        Some(cli::Command::GenScript { alias }) => client::script::gen_scripts(alias.as_deref()),
        Some(cli::Command::Update { check }) => {
            if check {
                common::self_update::check().await
            } else {
                common::self_update::update().await
            }
        }
        None => client::start().await,
    }
}

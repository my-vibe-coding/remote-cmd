mod cli;
mod client;
mod common;
mod embed;
mod server;

use clap::Parser;

fn main() -> anyhow::Result<()> {
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

    // 上传进度弹窗宿主：由上传方 spawn（--progress-dialog），主线程跑 xdialog 事件
    // 循环，stdin 行式 JSON 驱动进度（见 dialog::run_progress_host）；stdin EOF
    // （上传方退出/崩溃）时自动关闭，无窗口残留
    if args.iter().any(|a| a == "--progress-dialog") {
        let code = xdialog::XDialogBuilder::new().run_i32(|| {
            client::dialog::run_progress_host();
            0
        });
        std::process::exit(code);
    }

    // 别名 shim 模式：bin 目录下以别名命名的 exe 副本（脚本生成功能创建），
    // 必须在 clap 解析之前拦截，避免别名参数被误认为选项；run 为 async，
    // 同步 main 中先判定再转入 tokio 主流程
    let shimmed = client::script::shim_alias().is_some();

    let cli = if shimmed { None } else { Some(cli::Cli::parse()) };

    // 自更新命令：xdialog 的事件循环必须占用主线程（其 Builder 文档约束），需在
    // tokio 接管前调度——更新逻辑运行在 xdialog 拉起的线程上；无显示环境（无头
    // Linux）时弹窗自动退化为 no-op，静默更新不受影响。--check 仅查版本，无弹窗
    if let Some(cli) = &cli {
        if let Some(cli::Command::Update { check: false }) = &cli.command {
            let code = xdialog::XDialogBuilder::new().run_i32(update_entry);
            std::process::exit(code);
        }
    }

    tokio_main(cli)
}

/// update 命令入口：运行在 xdialog 拉起的逻辑线程上（主线程已被其事件循环占用），
/// 以 current-thread 运行时执行更新，退出码经 run_i32 返回给进程
fn update_entry() -> i32 {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("创建 tokio 运行时失败");
    match rt.block_on(common::self_update::update()) {
        Ok(()) => 0,
        Err(e) => {
            macro_log::e!("更新失败: {e:#}");
            1
        }
    }
}

/// 常规命令分发：pre_parsed 为 None 时处于 shim 模式（须先于 clap 处理），
/// 否则为 main 中已解析好的 cli
#[tokio::main]
async fn tokio_main(pre_parsed: Option<cli::Cli>) -> anyhow::Result<()> {
    if let Some(alias) = client::script::shim_alias() {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let res = client::run_cmd::run(&alias, &args, false).await;
        if let Err(e) = &res {
            crate::dlog!("[main] shim run 失败: {e:#}");
        }
        return res;
    }

    let cli = pre_parsed.unwrap_or_else(cli::Cli::parse);
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

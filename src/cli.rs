use clap::{Parser, Subcommand};

/// remote-cmd: 远程命令执行工具（服务端 / 客户端一体化）
#[derive(Parser)]
#[command(name = "remote-cmd", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// 以服务模式运行：HTTPS 管理后台 + 注册审批 + 命令代执行
    Serve {
        /// 监听端口（默认 3233）
        #[arg(long, default_value_t = 3233)]
        port: u16,
        /// 管理员密码；不提供则自动生成随机密码并打印
        #[arg(long)]
        password: Option<String>,
        /// 数据目录（默认 %USERPROFILE%\.remote-cmd\server）
        #[arg(long)]
        data_dir: Option<String>,
    },
    /// 执行已配置别名的远程命令（供 bat 脚本调用）
    Run {
        /// 命令别名
        alias: String,
        /// 交互模式：本地终端逐行输入转发到远端 stdin（需放在别名之前）
        #[arg(short = 'i', long)]
        interactive: bool,
        /// 传给远程命令的参数（含 - 开头的选项，原样透传不被 clap 解析）
        #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// 为指定别名生成 exe 脚本到 %USERPROFILE%\.remote-cmd\bin
    GenScript {
        /// 命令别名（省略则为所有别名生成）
        alias: Option<String>,
    },
}

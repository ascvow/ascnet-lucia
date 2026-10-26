//! Lucia 交互式 TUI 的可执行入口。
//!
//! 本文件只负责解析命令行并把控制权交给对应子命令；所有装配、状态与渲染逻辑分布在
//! `app`、`session`、`command`、`plugin` 与 `ui` 模块中。

mod app;
mod bus;
mod cli;
mod command;
mod config;
mod demo;
mod doctor;
mod event;
#[cfg(feature = "plugins")]
mod plugin;
mod session;
mod startup;
mod stream_probe;
mod terminal;
mod theme;
mod ui;
mod usage_ledger;

use anyhow::Result;
use clap::Parser;
use cli::{Args, Command};

/// 安装 TUI 的 panic hook：先恢复终端，再把消息交给默认 hook 打印。
///
/// `TerminalGuard` 的 `Drop` 已经会在栈展开时恢复终端，但那发生在 panic 消息打印
/// 之后：消息会先落进备用屏幕，随退出备用屏幕一起被清掉，用户只看到程序闪退。
/// 恢复动作是幂等的，`Drop` 随后再恢复一次不会产生副作用。
fn install_terminal_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        ratatui::restore();
        default_hook(info);
    }));
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = Args::parse();
    match args.command.take() {
        Some(Command::Doctor(options)) => doctor::run(&args, options).await,
        #[cfg(feature = "plugins")]
        Some(Command::Plugins(options)) => plugin::recovery::run(options),
        None => {
            install_terminal_panic_hook();
            startup::run(args).await
        }
    }
}

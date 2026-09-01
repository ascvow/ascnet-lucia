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
mod theme;
mod ui;

use anyhow::Result;
use clap::Parser;
use cli::{Args, Command};

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = Args::parse();
    match args.command.take() {
        Some(Command::Doctor(options)) => doctor::run(&args, options).await,
        None => startup::run(args).await,
    }
}

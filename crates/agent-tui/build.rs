//! 为 Lucia TUI 注入可供 Genome 复核的编译时源码身份。

use std::process::Command;

/// 构建脚本入口。
///
/// 发布系统可以通过 `LUCIA_BUILD_GIT_COMMIT` 与 `LUCIA_BUILD_GIT_DIRTY` 固定源码身份；本地
/// 构建默认读取当前 Git 工作树。源码归档没有 Git 元数据时使用显式 `unknown`，不伪造提交号。
fn main() {
    emit_rerun_contract();
    let target = std::env::var("TARGET").expect("Cargo 必须向 build script 提供 TARGET");
    let commit = std::env::var("LUCIA_BUILD_GIT_COMMIT")
        .ok()
        .filter(|value| valid_commit(value))
        .or_else(git_commit)
        .unwrap_or_else(|| "unknown".to_string());
    let dirty = std::env::var("LUCIA_BUILD_GIT_DIRTY")
        .ok()
        .and_then(|value| parse_dirty(&value))
        .or_else(git_dirty)
        .unwrap_or(true);

    println!("cargo:rustc-env=LUCIA_BUILD_TARGET={target}");
    println!("cargo:rustc-env=LUCIA_BUILD_GIT_COMMIT={commit}");
    println!("cargo:rustc-env=LUCIA_BUILD_GIT_DIRTY={dirty}");
}

/// 声明会影响编译时源码身份的输入，确保提交、暂存或 Rust crate 变化后重新计算。
fn emit_rerun_contract() {
    println!("cargo:rerun-if-env-changed=LUCIA_BUILD_GIT_COMMIT");
    println!("cargo:rerun-if-env-changed=LUCIA_BUILD_GIT_DIRTY");
    println!("cargo:rerun-if-env-changed=TARGET");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");
    println!("cargo:rerun-if-changed=../");
}

/// 从当前仓库读取完整 Git 提交号；命令失败或输出非法时返回 `None`。
fn git_commit() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--verify", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let commit = String::from_utf8(output.stdout).ok()?.trim().to_string();
    valid_commit(&commit).then_some(commit)
}

/// 判断当前工作树是否包含已跟踪或未跟踪改动；无法读取时返回 `None`。
fn git_dirty() -> Option<bool> {
    let output = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=normal"])
        .output()
        .ok()?;
    output.status.success().then_some(!output.stdout.is_empty())
}

/// 校验提交号只包含 Git 十六进制对象 ID，或使用源码归档的 `unknown` 标记。
fn valid_commit(value: &str) -> bool {
    value == "unknown"
        || ((7..=64).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

/// 解析发布系统提供的严格布尔工作树状态。
fn parse_dirty(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

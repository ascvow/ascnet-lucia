//! 原生文件与进程工具的越权防护回归测试。
//!
//! 覆盖 M0-04 要求的攻击面：路径穿越、绝对路径、symlink 逃逸、工作目录逃逸、
//! 环境变量泄漏、子进程逃逸、超时与大量输出。
//!
//! 这些测试不依赖 permission 插件，验证的是 Host 级防线本身。

use agent_tool::{
    builtins::{ListDirectoryTool, ReadFileTool, SearchFilesTool, ShellTool, WriteFileTool},
    ExecutionPolicy, Tool, ToolAccess, ToolCall, ToolErrorKind, ToolResult, WorkspaceGuard,
};
use serde_json::json;
use std::{fs, path::PathBuf, time::Duration};

/// 在临时目录下搭建 `workspace/` 与 `secret/` 两个同级目录。
///
/// `secret/` 代表工作区之外的敏感内容（Hidden Dataset、凭据文件等），
/// 任何工具都不应该能读到它。
fn fixture(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("lucia-sec-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(base.join("workspace")).expect("应创建工作区");
    fs::create_dir_all(base.join("secret")).expect("应创建工作区外目录");
    fs::write(base.join("workspace/inside.txt"), "工作区内容").expect("应写入内部文件");
    fs::write(base.join("secret/dataset.json"), "隐藏数据集").expect("应写入外部文件");
    base
}

/// 在独立运行时上执行一次工具调用。
fn run(tool: &dyn Tool, call: ToolCall) -> ToolResult {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("应创建测试运行时")
        .block_on(tool.call(call))
        .expect("工具调用不应返回 Err")
}

/// 断言结果是被拒绝的越权访问，而不是恰好读不到内容。
fn assert_denied(result: &ToolResult) {
    assert!(
        result.is_error,
        "越权访问必须返回错误：{:?}",
        result.content
    );
    let text = result.content.to_string();
    assert!(
        text.contains("超出允许的工作区范围") || text.contains("不允许"),
        "错误原因应指明越权，实际为：{text}"
    );
}

#[test]
fn read_file_rejects_parent_traversal() {
    let base = fixture("read-traversal");
    let tool = ReadFileTool::new(WorkspaceGuard::rooted(base.join("workspace")).expect("守卫"));

    let result = run(
        &tool,
        ToolCall::new("c", "read_file", json!({"path": "../secret/dataset.json"})),
    );

    assert_denied(&result);
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn read_file_rejects_absolute_path_outside_workspace() {
    let base = fixture("read-absolute");
    let tool = ReadFileTool::new(WorkspaceGuard::rooted(base.join("workspace")).expect("守卫"));
    let target = base.join("secret/dataset.json");

    let result = run(
        &tool,
        ToolCall::new("c", "read_file", json!({"path": target.to_string_lossy()})),
    );

    assert_denied(&result);
    let _ = fs::remove_dir_all(&base);
}

#[cfg(unix)]
#[test]
fn read_file_rejects_symlink_escape() {
    let base = fixture("read-symlink");
    std::os::unix::fs::symlink(
        base.join("secret/dataset.json"),
        base.join("workspace/link.json"),
    )
    .expect("应创建符号链接");
    let tool = ReadFileTool::new(WorkspaceGuard::rooted(base.join("workspace")).expect("守卫"));

    let result = run(
        &tool,
        ToolCall::new("c", "read_file", json!({"path": "link.json"})),
    );

    assert_denied(&result);
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn write_file_rejects_escape() {
    let base = fixture("write-escape");
    let tool = WriteFileTool::new(WorkspaceGuard::rooted(base.join("workspace")).expect("守卫"));

    let result = run(
        &tool,
        ToolCall::new(
            "c",
            "write_file",
            json!({"path": "../secret/injected.txt", "content": "越权写入"}),
        ),
    );

    assert_denied(&result);
    assert!(
        !base.join("secret/injected.txt").exists(),
        "越权写入不应落盘"
    );
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn list_directory_rejects_escape() {
    let base = fixture("list-escape");
    let tool =
        ListDirectoryTool::new(WorkspaceGuard::rooted(base.join("workspace")).expect("守卫"));

    let result = run(
        &tool,
        ToolCall::new("c", "list_directory", json!({"path": "../secret"})),
    );

    assert_denied(&result);
    let _ = fs::remove_dir_all(&base);
}

#[cfg(unix)]
#[test]
fn search_does_not_follow_symlink_outside_workspace() {
    let base = fixture("search-symlink");
    std::os::unix::fs::symlink(base.join("secret"), base.join("workspace/linked"))
        .expect("应创建目录符号链接");
    let tool = SearchFilesTool::new(WorkspaceGuard::rooted(base.join("workspace")).expect("守卫"));

    let result = run(
        &tool,
        ToolCall::new("c", "search_files", json!({"pattern": "隐藏数据集"})),
    );

    assert!(!result.is_error, "搜索本身应成功");
    assert_eq!(
        result.content["count"], 0,
        "不应通过符号链接命中工作区外内容：{:?}",
        result.content
    );
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn shell_rejects_working_directory_outside_workspace() {
    let base = fixture("shell-cwd");
    let tool = ShellTool::new(WorkspaceGuard::rooted(base.join("workspace")).expect("守卫"));

    let result = run(
        &tool,
        ToolCall::new(
            "c",
            "shell",
            json!({
                "command": "pwd",
                "working_directory": base.join("secret").to_string_lossy(),
            }),
        ),
    );

    assert_denied(&result);
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn shell_defaults_to_workspace_root() {
    let base = fixture("shell-default-cwd");
    let root = base.join("workspace").canonicalize().expect("根应可解析");
    let tool = ShellTool::new(WorkspaceGuard::rooted(&root).expect("守卫"));

    let result = run(
        &tool,
        ToolCall::new("c", "shell", json!({"command": "pwd"})),
    );

    let stdout = result.content["stdout"].as_str().unwrap_or_default();
    assert_eq!(
        stdout.trim(),
        root.to_string_lossy(),
        "默认应在工作区根执行"
    );
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn shell_does_not_leak_unlisted_environment_variables() {
    let base = fixture("shell-env");
    // 模拟宿主进程中的凭据；白名单之外的变量不应进入子进程。
    std::env::set_var("LUCIA_TEST_FAKE_TOKEN", "super-secret-value");
    let tool = ShellTool::new(WorkspaceGuard::rooted(base.join("workspace")).expect("守卫"));

    let result = run(
        &tool,
        ToolCall::new(
            "c",
            "shell",
            json!({
                "command": "printf 'direct=%s\\n' \"${LUCIA_TEST_FAKE_TOKEN:-absent}\"; printf 'subshell=%s\\n' \"$(printenv LUCIA_TEST_FAKE_TOKEN 2>/dev/null || printf absent)\"; env; echo \"path=${PATH:+set}\""
            }),
        ),
    );

    let stdout = result.content["stdout"].as_str().unwrap_or_default();
    assert!(
        stdout.contains("absent"),
        "白名单外的变量必须缺失，实际输出：{stdout}"
    );
    assert!(
        !stdout.contains("super-secret-value"),
        "Secret 不应出现在子进程环境中"
    );
    assert!(stdout.contains("path=set"), "白名单内的 PATH 应保留");

    std::env::remove_var("LUCIA_TEST_FAKE_TOKEN");
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn shell_reports_timeout() {
    let base = fixture("shell-timeout");
    let tool = ShellTool::new(WorkspaceGuard::rooted(base.join("workspace")).expect("守卫"));

    let result = run(
        &tool,
        ToolCall::new(
            "c",
            "shell",
            json!({"command": "sleep 5", "timeout_ms": 200}),
        ),
    );

    assert!(result.is_error, "超时必须返回错误");
    assert!(
        result.content.to_string().contains("超时"),
        "错误应说明超时：{:?}",
        result.content
    );
    let _ = fs::remove_dir_all(&base);
}

/// 超时后不能只杀掉 sh 本身，其派生的后代必须一并回收。
#[cfg(unix)]
#[test]
fn shell_kills_descendant_processes_on_timeout() {
    let base = fixture("shell-tree");
    let workspace = base.join("workspace");
    let marker = workspace.join("leaked.txt");
    let tool = ShellTool::new(WorkspaceGuard::rooted(&workspace).expect("守卫"));

    let result = run(
        &tool,
        ToolCall::new(
            "c",
            "shell",
            json!({
                "command": "(sleep 1; echo leaked > leaked.txt) & sleep 30",
                "timeout_ms": 250,
            }),
        ),
    );
    assert!(result.is_error, "命令应因超时失败");

    // 后代若存活，会在 1 秒后写出标记文件。
    std::thread::sleep(Duration::from_millis(2000));
    assert!(
        !marker.exists(),
        "超时后子进程树必须被回收，不应留下后代继续写文件"
    );
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn shell_truncates_oversized_output() {
    let base = fixture("shell-output");
    let tool = ShellTool::new(WorkspaceGuard::rooted(base.join("workspace")).expect("守卫"));

    // 生成远超 100 KiB 上限的输出。
    let result = run(
        &tool,
        ToolCall::new(
            "c",
            "shell",
            json!({
                "command": "yes 0123456789012345678901234567890123456789 | head -n 20000",
                "timeout_ms": 30000,
            }),
        ),
    );

    let stdout = result.content["stdout"].as_str().unwrap_or_default();
    assert!(stdout.contains("[输出已截断]"), "超限输出应被截断");
    assert!(
        stdout.len() < 200 * 1024,
        "截断后长度应受控，实际 {} 字节",
        stdout.len()
    );
    let _ = fs::remove_dir_all(&base);
}

/// Evaluation 平面默认关闭进程类工具，这是 shell 无法被工作区完全约束时的兜底。
#[test]
fn evaluation_profile_disables_process_tools() {
    let policy = ExecutionPolicy::evaluation("/tmp");

    assert!(!policy.permits_tool("shell"));
    assert!(!policy.permits_tool("process_exec"));
}

/// 直接调用真实 Shell 入口也不能绕过 Evaluation 的进程、注入与环境隔离。
#[test]
fn evaluation_shell_boundary_blocks_injection_and_secret_extraction() {
    let base = fixture("evaluation-shell-boundary");
    let workspace = base.join("workspace");
    let marker = workspace.join("injected.txt");
    std::env::set_var("LUCIA_EVALUATION_FAKE_SECRET", "evaluation-secret-value");

    let mut policy = ExecutionPolicy::evaluation(&workspace);
    // 模拟错误配置同时放开工具名和公开能力字段，验证私有 Profile 仍拥有最终否决权。
    policy.tools = ToolAccess::allowlist(["shell"]);
    policy.allow_process = true;
    policy.allow_secrets = true;
    let guard = WorkspaceGuard::from_policy(&policy).expect("应按评测策略创建守卫");
    let tool = ShellTool::with_execution_policy(guard, policy);

    let result = run(
        &tool,
        ToolCall::new(
            "evaluation-shell",
            "shell",
            json!({
                "command": "printf '%s' \"$(printenv LUCIA_EVALUATION_FAKE_SECRET)\"; printf injected > injected.txt"
            }),
        ),
    );

    assert!(result.is_error, "Evaluation 的真实 Shell 入口必须拒绝调用");
    assert_eq!(
        result.error_kind,
        Some(ToolErrorKind::ProcessBoundaryViolation)
    );
    assert!(
        !result
            .content
            .to_string()
            .contains("evaluation-secret-value"),
        "拒绝结果不得包含宿主 Secret"
    );
    assert!(!marker.exists(), "注入载荷不得产生文件副作用");

    std::env::remove_var("LUCIA_EVALUATION_FAKE_SECRET");
    let _ = fs::remove_dir_all(&base);
}

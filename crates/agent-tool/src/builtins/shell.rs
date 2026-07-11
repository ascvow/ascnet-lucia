//! 执行 shell 命令工具。
//! Shell command execution tool.

use crate::{Tool, ToolCall, ToolResult, ToolSpec};
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;

const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MAX_TIMEOUT_MS: u64 = 600_000;
/// 单个输出流（stdout / stderr）最大保留字节数。
const MAX_OUTPUT_BYTES: usize = 100 * 1024;

/// 执行 shell 命令并返回 stdout、stderr 和退出码。
/// Execute a shell command and return stdout, stderr, and exit code.
pub struct ShellTool {
    /// 全局默认超时（毫秒），通过构造函数配置。
    default_timeout_ms: u64,
}

impl Default for ShellTool {
    fn default() -> Self {
        Self {
            default_timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    }
}

impl ShellTool {
    /// 使用自定义默认超时创建 ShellTool。
    /// Create a ShellTool with a custom default timeout.
    pub fn with_timeout_ms(timeout_ms: u64) -> Self {
        Self {
            default_timeout_ms: timeout_ms.min(MAX_TIMEOUT_MS),
        }
    }
}

#[derive(Deserialize)]
struct Args {
    command: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    working_directory: Option<String>,
}

/// 截断过长输出，末尾附加提示。
fn truncate_output(s: &str) -> String {
    if s.len() <= MAX_OUTPUT_BYTES {
        return s.to_string();
    }
    let mut truncated = s[..MAX_OUTPUT_BYTES].to_string();
    truncated.push_str("\n... [输出已截断]");
    truncated
}

#[async_trait]
impl Tool for ShellTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "shell",
            "执行 shell 命令，返回 stdout、stderr 和退出码。",
            json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "要执行的 shell 命令"
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "description": "超时时间（毫秒），默认 30000，最大 600000",
                        "minimum": 1,
                        "maximum": 600000
                    },
                    "working_directory": {
                        "type": "string",
                        "description": "工作目录，默认为当前目录"
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        )
    }

    async fn call(&self, call: ToolCall) -> Result<ToolResult> {
        let args: Args = call.args_as()?;
        let timeout_ms = args
            .timeout_ms
            .unwrap_or(self.default_timeout_ms)
            .min(MAX_TIMEOUT_MS);

        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg(&args.command);

        if let Some(ref dir) = args.working_directory {
            cmd.current_dir(dir);
        }

        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let result = tokio::time::timeout(Duration::from_millis(timeout_ms), cmd.output()).await;

        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let exit_code = output.status.code().unwrap_or(-1);
                let is_error = !output.status.success();

                let payload = json!({
                    "stdout": truncate_output(&stdout),
                    "stderr": truncate_output(&stderr),
                    "exit_code": exit_code,
                });

                if is_error {
                    Ok(ToolResult {
                        call_id: call.id,
                        name: call.name,
                        content: payload,
                        is_error: true,
                        details: None,
                    })
                } else {
                    Ok(ToolResult::success(call.id, call.name, payload))
                }
            }
            Ok(Err(e)) => Ok(ToolResult::error(
                call.id,
                call.name,
                format!("命令执行失败: {e}"),
            )),
            Err(_) => Ok(ToolResult::error(
                call.id,
                call.name,
                format!("命令超时（{timeout_ms}ms）"),
            )),
        }
    }
}

//! 执行 shell 命令工具。
//! Shell command execution tool.

use crate::{
    NoopToolOutputSink, Tool, ToolCall, ToolOutputDelta, ToolOutputSink, ToolOutputStream,
    ToolResult, ToolSpec,
};
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use std::{sync::Arc, time::Duration};
use tokio::io::{AsyncRead, AsyncReadExt};

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

/// 单个输出流的有限缓冲结果；读取仍会持续到 EOF，避免子进程管道阻塞。
struct CapturedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

impl CapturedOutput {
    /// 转换为最终结果文本，并保留原有截断提示语义。
    fn into_text(self) -> String {
        let mut text = String::from_utf8_lossy(&self.bytes).into_owned();
        if self.truncated {
            text.push_str("\n... [输出已截断]");
        }
        text
    }
}

/// 持续读取一个子进程输出流，有限保留并同步发布可展示的增量。
async fn capture_stream<R>(
    mut reader: R,
    call_id: String,
    stream: ToolOutputStream,
    output: Arc<dyn ToolOutputSink>,
) -> std::io::Result<CapturedOutput>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let mut captured = Vec::new();
    let mut truncated = false;
    let mut buffer = [0_u8; 4096];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = MAX_OUTPUT_BYTES.saturating_sub(captured.len());
        let retained = read.min(remaining);
        if retained > 0 {
            captured.extend_from_slice(&buffer[..retained]);
            output.emit(ToolOutputDelta {
                call_id: call_id.clone(),
                stream,
                delta: String::from_utf8_lossy(&buffer[..retained]).into_owned(),
            });
        }
        if retained < read && !truncated {
            truncated = true;
            output.emit(ToolOutputDelta {
                call_id: call_id.clone(),
                stream,
                delta: "\n... [输出已截断]".into(),
            });
        }
    }
    Ok(CapturedOutput {
        bytes: captured,
        truncated,
    })
}

#[async_trait]
impl Tool for ShellTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "shell",
            "Run a shell command through sh -c and return stdout, stderr, and the exit code. Use this for builds, tests, version-control operations, or tasks without a dedicated tool. Prefer the dedicated tools for reading, writing, listing, and searching files. Each output stream is limited to 100 KiB.",
            json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Complete command passed to sh -c; the command may cause system side effects"
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "description": "Command timeout in milliseconds; default 30000, maximum 600000",
                        "minimum": 1,
                        "maximum": 600000
                    },
                    "working_directory": {
                        "type": "string",
                        "description": "Working directory for the command; defaults to the current process directory"
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        )
    }

    async fn call(&self, call: ToolCall) -> Result<ToolResult> {
        self.call_with_output(call, Arc::new(NoopToolOutputSink))
            .await
    }

    /// 启动 Shell 并在等待退出期间持续转发 stdout 与 stderr。
    async fn call_with_output(
        &self,
        call: ToolCall,
        output: Arc<dyn ToolOutputSink>,
    ) -> Result<ToolResult> {
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
        cmd.kill_on_drop(true);

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(error) => {
                return Ok(ToolResult::error(
                    call.id,
                    call.name,
                    format!("命令执行失败: {error}"),
                ));
            }
        };
        let stdout = child.stdout.take().expect("Shell stdout 已配置为管道");
        let stderr = child.stderr.take().expect("Shell stderr 已配置为管道");
        let stdout_task = tokio::spawn(capture_stream(
            stdout,
            call.id.clone(),
            ToolOutputStream::Stdout,
            Arc::clone(&output),
        ));
        let stderr_task = tokio::spawn(capture_stream(
            stderr,
            call.id.clone(),
            ToolOutputStream::Stderr,
            output,
        ));
        let status = tokio::time::timeout(Duration::from_millis(timeout_ms), child.wait()).await;

        match status {
            Ok(Ok(status)) => {
                let stdout = stdout_task.await??.into_text();
                let stderr = stderr_task.await??.into_text();
                let exit_code = status.code().unwrap_or(-1);
                let is_error = !status.success();

                let payload = json!({
                    "stdout": stdout,
                    "stderr": stderr,
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
            Ok(Err(error)) => {
                let _ = stdout_task.await;
                let _ = stderr_task.await;
                Ok(ToolResult::error(
                    call.id,
                    call.name,
                    format!("命令执行失败: {error}"),
                ))
            }
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                let _ = stdout_task.await;
                let _ = stderr_task.await;
                Ok(ToolResult::error(
                    call.id,
                    call.name,
                    format!("命令超时（{timeout_ms}ms）"),
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 记录测试期间收到的增量，验证输出在命令结束前即可观察。
    #[derive(Default)]
    struct CapturingOutputSink {
        outputs: Mutex<Vec<ToolOutputDelta>>,
    }

    impl ToolOutputSink for CapturingOutputSink {
        fn emit(&self, output: ToolOutputDelta) {
            self.outputs.lock().expect("输出锁不应中毒").push(output);
        }
    }

    /// Shell 应在仍处于运行状态时发布 stdout，并在结束后保留完整结果。
    #[test]
    fn shell_streams_output_before_completion() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("应创建测试运行时")
            .block_on(async {
                let sink = Arc::new(CapturingOutputSink::default());
                let task_sink: Arc<dyn ToolOutputSink> = sink.clone();
                let task = tokio::spawn(async move {
                    ShellTool::default()
                        .call_with_output(
                            ToolCall::new(
                                "shell-live",
                                "shell",
                                json!({"command": "printf first; sleep 0.2; printf second"}),
                            ),
                            task_sink,
                        )
                        .await
                });

                tokio::time::timeout(Duration::from_secs(1), async {
                    loop {
                        if !sink.outputs.lock().expect("输出锁不应中毒").is_empty() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await
                .expect("首段输出应及时到达");
                assert!(!task.is_finished(), "命令此时应仍在等待第二段输出");
                assert_eq!(
                    sink.outputs.lock().expect("输出锁不应中毒")[0].delta,
                    "first"
                );

                let result = task.await.expect("Shell 任务不应崩溃").expect("调用应成功");
                assert_eq!(result.content["stdout"], "firstsecond");
                assert_eq!(result.content["exit_code"], 0);
            });
    }
}

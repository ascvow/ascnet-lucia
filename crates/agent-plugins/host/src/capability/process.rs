//! 插件子进程的受控派生、读写与终止。

use crate::capability::requests::{
    copy_safe_process_environment, parse_request, resolve_from, validate_process_spawn_request,
    ProcessHandleRequest, ProcessReadRequest, ProcessSpawnRequest, ProcessWriteRequest,
};
use crate::capability::{
    capability_denied, CapabilityState, MAX_PLUGIN_PROCESSES, MAX_PROCESS_LINE_BYTES,
    MAX_PROCESS_WRITE_BYTES, MAX_READ_TIMEOUT_MS,
};
use agent_tool::ExecutionProfile;
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::process::{ChildStdin, ChildStdout};
use tokio::time::timeout;
pub(crate) struct ManagedProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    /// 超时重试保留已接收前缀，超限后关闭句柄，避免剩余字节伪装为下一条完整消息。
    pending_line: Vec<u8>,
    /// Unix 下由子进程 PID 建立的独立进程组，用于回收命令启动的全部后代。
    #[cfg(unix)]
    process_group: u32,
}

impl ManagedProcess {
    /// 请求终止插件进程及其派生的整个进程树。
    pub(crate) fn start_kill_tree(&mut self) {
        #[cfg(unix)]
        {
            // 子进程启动时已成为同 PID 的进程组 leader，负 PID 表示向整组发送信号。
            if let Ok(group) = i32::try_from(self.process_group) {
                unsafe {
                    libc::kill(-group, libc::SIGKILL);
                }
            }
        }
        let _ = self.child.start_kill();
    }
}

impl Drop for ManagedProcess {
    /// 插件停用、Host 状态替换或 future 取消时同步回收整个独立进程组。
    fn drop(&mut self) {
        self.start_kill_tree();
    }
}

impl CapabilityState {
    /// 启动一个带管道 stdin/stdout 的长驻子进程。
    pub(crate) fn spawn_process(&mut self, request_json: &str) -> Result<Value> {
        self.require_process_exec()?;
        // 这里只统计当前插件实例持有的受管直接进程组句柄；命令组内后代不计入该值。
        let process_limit = self
            .execution_policy
            .limits
            .max_processes
            .unwrap_or(MAX_PLUGIN_PROCESSES)
            .min(MAX_PLUGIN_PROCESSES);
        if self.processes.len() >= process_limit {
            return Err(anyhow!(
                "当前策略允许单个插件同时运行的进程数上限为 {process_limit}"
            ));
        }

        let request: ProcessSpawnRequest = parse_request(request_json)?;
        validate_process_spawn_request(&request)?;

        let cwd = self.resolve_process_cwd(request.cwd.as_deref())?;
        let mut command = Command::new(&request.command);
        #[cfg(unix)]
        command.process_group(0);
        command
            .args(&request.args)
            .current_dir(cwd)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(if request.inherit_stderr {
                Stdio::inherit()
            } else {
                Stdio::null()
            })
            .kill_on_drop(true);
        copy_safe_process_environment(&mut command);
        command.envs(request.env);

        let mut child = command
            .spawn()
            .with_context(|| format!("启动插件进程失败：{}", request.command))?;
        #[cfg(unix)]
        let process_group = child.id().context("插件子进程缺少 PID")?;
        let stdin = child.stdin.take().context("插件进程缺少 stdin 管道")?;
        let stdout = child.stdout.take().context("插件进程缺少 stdout 管道")?;
        let handle = self.next_process_handle;
        self.next_process_handle = self.next_process_handle.saturating_add(1);
        self.processes.insert(
            handle,
            ManagedProcess {
                child,
                stdin,
                stdout: BufReader::new(stdout),
                pending_line: Vec::new(),
                #[cfg(unix)]
                process_group,
            },
        );
        Ok(json!(handle))
    }

    /// 向指定子进程原样写入字节字符串。
    pub(crate) async fn write_process(&mut self, request_json: &str) -> Result<Value> {
        self.require_process_exec()?;
        let request: ProcessWriteRequest = parse_request(request_json)?;
        if request.data.len() > MAX_PROCESS_WRITE_BYTES {
            return Err(anyhow!(
                "插件进程单次 stdin 写入超过 {MAX_PROCESS_WRITE_BYTES} 字节限制"
            ));
        }
        let process = self
            .processes
            .get_mut(&request.handle)
            .ok_or_else(|| anyhow!("未知插件进程句柄：{}", request.handle))?;
        timeout(Duration::from_millis(MAX_READ_TIMEOUT_MS), async {
            process.stdin.write_all(request.data.as_bytes()).await?;
            process.stdin.flush().await
        })
        .await
        .map_err(|_| anyhow!("写入插件进程 stdin 超时"))?
        .context("写入插件进程 stdin 失败")?;
        Ok(Value::Null)
    }

    /// 从指定子进程读取一行，EOF 返回 `null`。
    pub(crate) async fn read_process_line(&mut self, request_json: &str) -> Result<Value> {
        self.require_process_exec()?;
        let request: ProcessReadRequest = parse_request(request_json)?;
        let process = self
            .processes
            .get_mut(&request.handle)
            .ok_or_else(|| anyhow!("未知插件进程句柄：{}", request.handle))?;
        let timeout_ms = request.timeout_ms.clamp(1, MAX_READ_TIMEOUT_MS);
        let line = timeout(Duration::from_millis(timeout_ms), async {
            loop {
                let buffer = process.stdout.fill_buf().await?;
                if buffer.is_empty() {
                    if process.pending_line.is_empty() {
                        return Ok(None);
                    }
                    break;
                }
                let newline = buffer.iter().position(|byte| *byte == b'\n');
                let take = newline.map_or(buffer.len(), |position| position + 1);
                if process.pending_line.len().saturating_add(take) > MAX_PROCESS_LINE_BYTES + 2 {
                    return Err(anyhow!(
                        "插件进程单行输出超过 {MAX_PROCESS_LINE_BYTES} 字节限制"
                    ));
                }
                process.pending_line.extend_from_slice(&buffer[..take]);
                process.stdout.consume(take);
                if newline.is_some() {
                    break;
                }
            }
            let mut line = std::mem::take(&mut process.pending_line);
            if line.last() == Some(&b'\n') {
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
            }
            if line.len() > MAX_PROCESS_LINE_BYTES {
                return Err(anyhow!(
                    "插件进程单行输出超过 {MAX_PROCESS_LINE_BYTES} 字节限制"
                ));
            }
            Ok(Some(
                String::from_utf8(line).context("插件进程 stdout 不是 UTF-8")?,
            ))
        })
        .await
        .map_err(|_| anyhow!("读取插件进程 stdout 超时：{timeout_ms}ms"))?;
        match line {
            Ok(Some(line)) => Ok(json!(line)),
            Ok(None) => Ok(Value::Null),
            Err(error) => {
                self.processes.remove(&request.handle);
                Err(error)
            }
        }
    }

    /// 终止并移除指定子进程。
    pub(crate) async fn kill_process(&mut self, request_json: &str) -> Result<Value> {
        self.require_process_exec()?;
        let request: ProcessHandleRequest = parse_request(request_json)?;
        let mut process = self
            .processes
            .remove(&request.handle)
            .ok_or_else(|| anyhow!("未知插件进程句柄：{}", request.handle))?;
        process.start_kill_tree();
        process.child.wait().await.context("等待插件进程终止失败")?;
        Ok(Value::Null)
    }

    pub(crate) fn require_process_exec(&self) -> Result<()> {
        if self.execution_policy.profile() != ExecutionProfile::Serve
            || !self.execution_policy.allow_process
        {
            return Err(capability_denied("Host ExecutionPolicy 禁止插件进程执行"));
        }
        if self.execution_policy.limits.max_processes == Some(0) {
            return Err(capability_denied("Host ExecutionPolicy 的进程数上限为 0"));
        }
        debug_assert!(self.execution_policy.permits_process_execution());
        if !self.permissions.process_exec {
            return Err(capability_denied("插件 manifest 未声明 process_exec 能力"));
        }
        Ok(())
    }

    fn resolve_process_cwd(&self, requested: Option<&str>) -> Result<PathBuf> {
        let path = requested
            .map(|path| resolve_from(&self.plugin_dir, path))
            .unwrap_or_else(|| self.plugin_dir.clone());
        let canonical = path
            .canonicalize()
            .with_context(|| format!("插件进程工作目录不存在：{}", path.display()))?;
        if !canonical.is_dir() {
            return Err(anyhow!("插件进程工作目录不是目录：{}", canonical.display()));
        }
        Ok(canonical)
    }
}

//! `lucia-evolve` 到独立 `lucia-eval` 的受限进程客户端。
//!
//! 客户端不链接 Evaluator 实现，也不传入 Dataset、Verifier、Commit Policy 或 Store 路径。
//! 每次调用只向固定可执行文件发送一份版本化 JSON，并严格校验脱敏回执与请求身份。

use agent_evolution_protocol::{
    ContextEvaluationReceiptV1, ContextEvaluationRequestV1, EvaluationReceiptV1,
    EvaluationRequestV1, HealthCheckReceiptV1, HealthCheckRequestV1, PromotionRequestV1,
    ReleaseReceiptV1, RollbackRequestV1, M6_CONTEXT_GATE_VERSION,
};
use async_trait::async_trait;
use serde::{de::DeserializeOwned, Serialize};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{io::AsyncWriteExt, process::Command};

/// Evaluator 单次请求允许的最大 stdout 字节数。
const MAX_RECEIPT_BYTES: usize = 64 * 1024;
/// Evaluator 稳定错误码允许的最大字节数。
const MAX_ERROR_CODE_BYTES: usize = 128;

/// 独立 Evaluator 的进程边界抽象，便于 Cycle 使用真实进程或离线测试替身。
#[async_trait]
pub trait EvaluatorClient: Send + Sync {
    /// 提交一个 Candidate 评测请求并返回正式 Report Seal 回执。
    ///
    /// # Errors
    ///
    /// 请求无效、Evaluator 进程失败、超时、输出越界或回执错绑时返回错误。
    async fn evaluate(
        &self,
        request: &EvaluationRequestV1,
    ) -> Result<EvaluationReceiptV1, EvaluatorProcessError>;

    /// 请求受信 Release Controller 晋升一份已 Seal 的正式报告。
    ///
    /// # Errors
    ///
    /// 请求无效、Evaluator 进程失败或回执未绑定请求的 Report/Release 时返回错误。
    async fn promote(
        &self,
        request: &PromotionRequestV1,
    ) -> Result<ReleaseReceiptV1, EvaluatorProcessError>;

    /// 请求受信 Evaluator 复核 Promotion 后的 Stable 与 Runtime 健康观察。
    ///
    /// # Errors
    ///
    /// 请求无效、Evaluator 进程失败或回执未绑定请求身份时返回错误。
    async fn health(
        &self,
        request: &HealthCheckRequestV1,
    ) -> Result<HealthCheckReceiptV1, EvaluatorProcessError>;

    /// 请求受信 Release Controller 原子回滚指定 Promotion。
    ///
    /// # Errors
    ///
    /// 请求无效、Evaluator 进程失败或回执未绑定请求的 Release 时返回错误。
    async fn rollback(
        &self,
        request: &RollbackRequestV1,
    ) -> Result<ReleaseReceiptV1, EvaluatorProcessError>;
}

/// Context Cycle 使用的独立 Evaluator 进程边界。
///
/// Context Gate 评测使用专用请求和回执；Promotion、Health 与 Rollback 复用相同的受信
/// Release 控制面，不在 Evolver 内复制 Gate 或发布规则。
#[async_trait]
pub trait ContextEvaluatorClient: Send + Sync {
    /// 提交 Context Candidate 并返回八指标 Gate 与正式 Report Seal 回执。
    ///
    /// # Errors
    ///
    /// 请求、Evaluator 进程、Context Report 或身份绑定无效时返回错误。
    async fn evaluate_context(
        &self,
        request: &ContextEvaluationRequestV1,
    ) -> Result<ContextEvaluationReceiptV1, EvaluatorProcessError>;

    /// 请求受信 Release Controller 晋升已通过 Context Gate 的正式报告。
    ///
    /// # Errors
    ///
    /// 请求、Evaluator 进程或回执绑定无效时返回错误。
    async fn promote_context(
        &self,
        request: &PromotionRequestV1,
    ) -> Result<ReleaseReceiptV1, EvaluatorProcessError>;

    /// 请求受信 Evaluator 复核 Context Promotion 后的 Runtime 健康观察。
    ///
    /// # Errors
    ///
    /// 请求、Evaluator 进程或回执绑定无效时返回错误。
    async fn health_context(
        &self,
        request: &HealthCheckRequestV1,
    ) -> Result<HealthCheckReceiptV1, EvaluatorProcessError>;

    /// 请求受信 Release Controller 回滚不健康的 Context Promotion。
    ///
    /// # Errors
    ///
    /// 请求、Evaluator 进程或回执绑定无效时返回错误。
    async fn rollback_context(
        &self,
        request: &RollbackRequestV1,
    ) -> Result<ReleaseReceiptV1, EvaluatorProcessError>;
}

/// 通过 stdin/stdout JSON 调用固定 `lucia-eval` 可执行文件的客户端。
#[derive(Debug, Clone)]
pub struct LuciaEvalProcessClient {
    executable: PathBuf,
    timeout: Duration,
}

impl LuciaEvalProcessClient {
    /// 创建进程客户端；可执行文件在每次调用前重新验证为绝对、非符号链接普通文件。
    pub fn new(executable: impl Into<PathBuf>, timeout: Duration) -> Self {
        Self {
            executable: executable.into(),
            timeout,
        }
    }

    /// 返回配置的 Evaluator 可执行文件路径。
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    /// 调用指定子命令并解析严格 JSON 回执。
    async fn invoke<I, O>(
        &self,
        action: &'static str,
        request: &I,
    ) -> Result<O, EvaluatorProcessError>
    where
        I: Serialize + Sync,
        O: DeserializeOwned,
    {
        validate_executable(&self.executable).await?;
        let bytes = serde_json::to_vec(request).map_err(EvaluatorProcessError::SerializeRequest)?;
        let mut child = Command::new(&self.executable)
            .arg(action)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| EvaluatorProcessError::Spawn {
                path: self.executable.clone(),
                source,
            })?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or(EvaluatorProcessError::MissingStdin)?;
        stdin
            .write_all(&bytes)
            .await
            .map_err(EvaluatorProcessError::WriteRequest)?;
        stdin
            .shutdown()
            .await
            .map_err(EvaluatorProcessError::WriteRequest)?;
        drop(stdin);

        let output = tokio::time::timeout(self.timeout, child.wait_with_output())
            .await
            .map_err(|_| EvaluatorProcessError::Timeout(self.timeout))?
            .map_err(EvaluatorProcessError::Wait)?;
        if output.stdout.len() > MAX_RECEIPT_BYTES || output.stderr.len() > MAX_RECEIPT_BYTES {
            return Err(EvaluatorProcessError::OutputTooLarge);
        }
        if !output.status.success() {
            return Err(EvaluatorProcessError::EvaluatorRejected {
                code: stable_error_code(&output.stderr),
                status: output.status.code(),
            });
        }
        if !output.stderr.is_empty() {
            return Err(EvaluatorProcessError::UnexpectedStderr);
        }
        serde_json::from_slice(&output.stdout).map_err(EvaluatorProcessError::InvalidReceipt)
    }
}

#[async_trait]
impl EvaluatorClient for LuciaEvalProcessClient {
    async fn evaluate(
        &self,
        request: &EvaluationRequestV1,
    ) -> Result<EvaluationReceiptV1, EvaluatorProcessError> {
        request
            .validate()
            .map_err(|error| EvaluatorProcessError::InvalidRequest(error.to_string()))?;
        let receipt: EvaluationReceiptV1 = self.invoke("evaluate", request).await?;
        receipt
            .validate()
            .map_err(|error| EvaluatorProcessError::InvalidReceiptBinding(error.to_string()))?;
        if receipt.request_id != request.request_id
            || receipt.parent_revision_id != request.parent_revision_id
            || receipt.candidate_revision_id != request.candidate_revision_id
        {
            return Err(EvaluatorProcessError::InvalidReceiptBinding(
                "Evaluation Receipt 与请求身份不匹配".to_string(),
            ));
        }
        Ok(receipt)
    }

    async fn promote(
        &self,
        request: &PromotionRequestV1,
    ) -> Result<ReleaseReceiptV1, EvaluatorProcessError> {
        request
            .validate()
            .map_err(|error| EvaluatorProcessError::InvalidRequest(error.to_string()))?;
        let receipt: ReleaseReceiptV1 = self.invoke("promote", request).await?;
        receipt
            .validate()
            .map_err(|error| EvaluatorProcessError::InvalidReceiptBinding(error.to_string()))?;
        if receipt.release_id != request.release_id || receipt.report_id != request.report_id {
            return Err(EvaluatorProcessError::InvalidReceiptBinding(
                "Promotion Receipt 与请求身份不匹配".to_string(),
            ));
        }
        if receipt.rollback_of.is_some() {
            return Err(EvaluatorProcessError::InvalidReceiptBinding(
                "Promotion Receipt 不得声明 rollback_of".to_string(),
            ));
        }
        Ok(receipt)
    }

    async fn health(
        &self,
        request: &HealthCheckRequestV1,
    ) -> Result<HealthCheckReceiptV1, EvaluatorProcessError> {
        request
            .validate()
            .map_err(|error| EvaluatorProcessError::InvalidRequest(error.to_string()))?;
        let receipt: HealthCheckReceiptV1 = self.invoke("health", request).await?;
        receipt
            .validate()
            .map_err(|error| EvaluatorProcessError::InvalidReceiptBinding(error.to_string()))?;
        if receipt.request_id != request.request_id
            || receipt.release_id != request.release_id
            || receipt.lineage != request.lineage
            || receipt.expected_revision_id != request.expected_revision_id
            || receipt.expected_generation != request.expected_generation
        {
            return Err(EvaluatorProcessError::InvalidReceiptBinding(
                "Health Receipt 与请求身份不匹配".to_string(),
            ));
        }
        Ok(receipt)
    }

    async fn rollback(
        &self,
        request: &RollbackRequestV1,
    ) -> Result<ReleaseReceiptV1, EvaluatorProcessError> {
        request
            .validate()
            .map_err(|error| EvaluatorProcessError::InvalidRequest(error.to_string()))?;
        let receipt: ReleaseReceiptV1 = self.invoke("rollback", request).await?;
        receipt
            .validate()
            .map_err(|error| EvaluatorProcessError::InvalidReceiptBinding(error.to_string()))?;
        if receipt.release_id != request.rollback_release_id
            || receipt.rollback_of.as_ref() != Some(&request.release_id)
        {
            return Err(EvaluatorProcessError::InvalidReceiptBinding(
                "Rollback Receipt 与请求身份不匹配".to_string(),
            ));
        }
        Ok(receipt)
    }
}

#[async_trait]
impl ContextEvaluatorClient for LuciaEvalProcessClient {
    async fn evaluate_context(
        &self,
        request: &ContextEvaluationRequestV1,
    ) -> Result<ContextEvaluationReceiptV1, EvaluatorProcessError> {
        request
            .validate()
            .map_err(|error| EvaluatorProcessError::InvalidRequest(error.to_string()))?;
        let receipt: ContextEvaluationReceiptV1 = self.invoke("context-evaluate", request).await?;
        receipt
            .validate(M6_CONTEXT_GATE_VERSION)
            .map_err(|error| EvaluatorProcessError::InvalidReceiptBinding(error.to_string()))?;
        if receipt.request_id != request.request_id
            || receipt.context_report.parent_revision_id != request.parent_revision_id
            || receipt.context_report.candidate_revision_id != request.candidate_revision_id
            || receipt.fixture_version != request.expected_fixture_version
        {
            return Err(EvaluatorProcessError::InvalidReceiptBinding(
                "Context Evaluation Receipt 与请求身份不匹配".to_string(),
            ));
        }
        Ok(receipt)
    }

    async fn promote_context(
        &self,
        request: &PromotionRequestV1,
    ) -> Result<ReleaseReceiptV1, EvaluatorProcessError> {
        EvaluatorClient::promote(self, request).await
    }

    async fn health_context(
        &self,
        request: &HealthCheckRequestV1,
    ) -> Result<HealthCheckReceiptV1, EvaluatorProcessError> {
        EvaluatorClient::health(self, request).await
    }

    async fn rollback_context(
        &self,
        request: &RollbackRequestV1,
    ) -> Result<ReleaseReceiptV1, EvaluatorProcessError> {
        EvaluatorClient::rollback(self, request).await
    }
}

/// 独立 Evaluator 调用错误。
#[derive(Debug, thiserror::Error)]
pub enum EvaluatorProcessError {
    /// 请求未通过共享 IPC 结构校验。
    #[error("Evaluator 请求无效：{0}")]
    InvalidRequest(String),
    /// 可执行文件必须是绝对、非符号链接普通文件。
    #[error("Evaluator 可执行文件路径不安全：{0}")]
    UnsafeExecutable(PathBuf),
    /// 请求 JSON 编码失败。
    #[error("序列化 Evaluator 请求失败：{0}")]
    SerializeRequest(serde_json::Error),
    /// 无法启动 Evaluator。
    #[error("启动 Evaluator 失败：{path}: {source}")]
    Spawn {
        /// 可执行文件路径。
        path: PathBuf,
        /// 原始错误。
        source: std::io::Error,
    },
    /// 子进程没有可写 stdin。
    #[error("Evaluator 子进程缺少 stdin")]
    MissingStdin,
    /// 写入请求失败。
    #[error("写入 Evaluator 请求失败：{0}")]
    WriteRequest(std::io::Error),
    /// 等待 Evaluator 失败。
    #[error("等待 Evaluator 失败：{0}")]
    Wait(std::io::Error),
    /// Evaluator 超过受控时间上限。
    #[error("Evaluator 请求超时：{0:?}")]
    Timeout(Duration),
    /// stdout 或 stderr 超过回执上限。
    #[error("Evaluator 输出超过上限")]
    OutputTooLarge,
    /// Evaluator 非零退出；只保留稳定错误码，不回显潜在敏感 stderr。
    #[error("Evaluator 拒绝请求：code={code}, status={status:?}")]
    EvaluatorRejected {
        /// 经过字符集和长度限制的稳定错误码。
        code: String,
        /// 子进程退出码。
        status: Option<i32>,
    },
    /// 成功请求不得携带 stderr，避免隐藏数据旁路泄漏。
    #[error("Evaluator 成功响应包含非空 stderr")]
    UnexpectedStderr,
    /// stdout 不是严格回执 JSON。
    #[error("Evaluator 回执 JSON 无效：{0}")]
    InvalidReceipt(serde_json::Error),
    /// 回执结构或身份未绑定原请求。
    #[error("Evaluator 回执绑定无效：{0}")]
    InvalidReceiptBinding(String),
}

/// 验证固定可执行文件不会经由相对路径或符号链接替换。
async fn validate_executable(path: &Path) -> Result<(), EvaluatorProcessError> {
    if !path.is_absolute() {
        return Err(EvaluatorProcessError::UnsafeExecutable(path.to_path_buf()));
    }
    let metadata = tokio::fs::symlink_metadata(path)
        .await
        .map_err(|_| EvaluatorProcessError::UnsafeExecutable(path.to_path_buf()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(EvaluatorProcessError::UnsafeExecutable(path.to_path_buf()));
    }
    Ok(())
}

/// 从 stderr 提取稳定 ASCII 错误码；任意自由文本都折叠为固定值。
fn stable_error_code(stderr: &[u8]) -> String {
    let Ok(text) = std::str::from_utf8(stderr) else {
        return "evaluator_failed".to_string();
    };
    let candidate = text.trim();
    if candidate.is_empty()
        || candidate.len() > MAX_ERROR_CODE_BYTES
        || !candidate
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        "evaluator_failed".to_string()
    } else {
        candidate.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// stderr 只有稳定短码可以跨越进程边界。
    #[test]
    fn sanitizes_evaluator_error_output() {
        assert_eq!(stable_error_code(b"dataset_invalid\n"), "dataset_invalid");
        assert_eq!(
            stable_error_code(b"failed: /private/hidden/answers.json"),
            "evaluator_failed"
        );
        assert_eq!(stable_error_code(&[0xff]), "evaluator_failed");
    }

    /// 相对路径不能成为受信 Evaluator 可执行文件。
    #[tokio::test]
    async fn rejects_relative_executable() {
        let client = LuciaEvalProcessClient::new("lucia-eval", Duration::from_secs(1));
        let error = validate_executable(client.executable())
            .await
            .expect_err("相对路径应拒绝");
        assert!(matches!(error, EvaluatorProcessError::UnsafeExecutable(_)));
    }
}

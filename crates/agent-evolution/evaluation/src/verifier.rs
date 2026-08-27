//! 受信最终 Verifier 的固定规则实现。
//!
//! Verifier 规则文件与 TaskCase 一起保存在受信 Dataset 中，不会进入 Candidate 上下文。
//! 评测结果只导出通过状态和计数，不导出期望答案或逐项隐藏反馈。

use crate::{dataset::validate_relative_path, FixtureCallRecord};
use agent_tool::{ToolErrorKind, ToolResult};
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, fmt::Debug, fs, path::Path, sync::Arc};

/// 当前支持的 Verifier 规则 schema 版本。
pub const VERIFIER_RULE_SCHEMA_VERSION: u32 = 1;
/// 单个 TaskCase 允许的最大 Verifier 检查数。
const MAX_VERIFIER_CHECKS: usize = 128;

/// 受信 Verifier 实现的稳定注册契约。
///
/// 实现只接收 Evaluator 已经收集的运行结果和 Fixture Workspace。Candidate 输出始终作为
/// 数据传入，Verifier 不得执行其中的命令或把失败详情返回给 Candidate。
pub trait TrustedVerifier: Debug + Send + Sync {
    /// 返回实现的稳定版本；该值必须与 [`VerifierRule::verifier_version`] 精确匹配。
    fn version(&self) -> &'static str;

    /// 校验一份规则是否属于当前实现支持的 schema 和检查集合。
    ///
    /// # Errors
    ///
    /// 规则版本、检查数量、拒绝类别或文件路径不合法时返回错误。
    fn validate_rule(&self, rule: &VerifierRule) -> Result<()>;

    /// 对一次真实运行执行最终验证。
    ///
    /// # Errors
    ///
    /// 规则无效，或读取受信 Workspace 时发生 I/O 错误时返回错误。答案不匹配必须返回
    /// `passed = false`，不能伪装为基础设施故障。
    fn verify(
        &self,
        rule: &VerifierRule,
        final_text: &str,
        workspace: &Path,
        fixture_records: &[FixtureCallRecord],
        tool_results: &[ToolResult],
    ) -> Result<VerificationResult>;
}

/// 内置确定性 Verifier v1。
#[derive(Debug, Default)]
pub struct BuiltinVerifierV1;

impl TrustedVerifier for BuiltinVerifierV1 {
    fn version(&self) -> &'static str {
        "builtin-v1"
    }

    fn validate_rule(&self, rule: &VerifierRule) -> Result<()> {
        rule.validate()
    }

    fn verify(
        &self,
        rule: &VerifierRule,
        final_text: &str,
        workspace: &Path,
        fixture_records: &[FixtureCallRecord],
        tool_results: &[ToolResult],
    ) -> Result<VerificationResult> {
        rule.verify(final_text, workspace, fixture_records, tool_results)
    }
}

/// 受信 Verifier Registry。
///
/// Registry 由 `lucia-eval` 进程构造，Dataset 只能引用已经注册的固定实现版本。未知版本
/// 会在执行 Candidate 之前失败关闭，不能回退到 Candidate 提供的规则解释器。
#[derive(Debug, Default)]
pub struct VerifierRegistry {
    implementations: BTreeMap<String, Arc<dyn TrustedVerifier>>,
}

impl VerifierRegistry {
    /// 创建不含任何实现的 Registry。
    pub fn new() -> Self {
        Self::default()
    }

    /// 创建只包含 Lucia 内置确定性 Verifier 的生产 Registry。
    pub fn with_builtin() -> Self {
        let mut registry = Self::new();
        registry
            .register(BuiltinVerifierV1)
            .expect("内置 Verifier 版本必须合法且唯一");
        registry
    }

    /// 注册一个受信 Verifier 实现。
    ///
    /// # Errors
    ///
    /// 版本名不符合稳定标识规则，或相同版本已经注册时返回错误。注册不会替换已有实现。
    pub fn register<V>(&mut self, implementation: V) -> Result<()>
    where
        V: TrustedVerifier + 'static,
    {
        let version = implementation.version();
        validate_stable_version(version)?;
        if self.implementations.contains_key(version) {
            return Err(anyhow!("Verifier 版本已注册，禁止覆盖：{version}"));
        }
        self.implementations
            .insert(version.to_string(), Arc::new(implementation));
        Ok(())
    }

    /// 校验规则并确认其实现版本已由受信进程注册。
    ///
    /// # Errors
    ///
    /// 实现版本未知或规则不合法时返回错误。
    pub fn validate_rule(&self, rule: &VerifierRule) -> Result<()> {
        self.resolve(rule)?.validate_rule(rule)
    }

    /// 使用规则绑定的受信实现执行最终验证。
    ///
    /// # Errors
    ///
    /// 实现版本未知、规则无效或 Verifier 读取 Workspace 失败时返回错误。
    pub fn verify(
        &self,
        rule: &VerifierRule,
        final_text: &str,
        workspace: &Path,
        fixture_records: &[FixtureCallRecord],
        tool_results: &[ToolResult],
    ) -> Result<VerificationResult> {
        let implementation = self.resolve(rule)?;
        implementation.validate_rule(rule)?;
        implementation.verify(rule, final_text, workspace, fixture_records, tool_results)
    }

    /// 解析规则绑定的实现；未知版本必须失败关闭。
    fn resolve(&self, rule: &VerifierRule) -> Result<&dyn TrustedVerifier> {
        self.implementations
            .get(&rule.verifier_version)
            .map(AsRef::as_ref)
            .ok_or_else(|| anyhow!("Verifier 实现未注册：{}", rule.verifier_version))
    }
}

/// 校验 Verifier 版本是可审计、可移植的稳定名称。
fn validate_stable_version(version: &str) -> Result<()> {
    if version.is_empty()
        || version.len() > 128
        || !version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(anyhow!("Verifier 版本必须是 1-128 位稳定 ASCII 名称"));
    }
    Ok(())
}

/// 受信 Evaluator 支持的确定性检查。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum VerifierCheck {
    /// 最终文本必须与期望值完全相等。
    ExactText {
        /// 受信期望文本，不得写入评测报告。
        expected: String,
    },
    /// 最终文本必须包含指定片段。
    ContainsText {
        /// 受信文本片段，不得写入评测报告。
        expected: String,
    },
    /// 最终文本必须是与期望值完全相等的 JSON。
    JsonEquals {
        /// 受信 JSON 答案，不得写入评测报告。
        expected: Value,
    },
    /// Fixture Runtime 成功匹配的工具名序列必须完全相等。
    ToolSequence {
        /// 受信期望工具名序列。
        expected: Vec<String>,
    },
    /// 指定工具必须被可信执行策略以允许的类别拒绝。
    ToolDenied {
        /// 安全探针使用的工具名。
        tool: String,
        /// 可接受的稳定拒绝类别；空列表不合法。
        accepted_kinds: Vec<ToolErrorKind>,
    },
    /// 所有实际工具结果都必须成功。
    NoToolErrors,
    /// Fixture Workspace 中的文本文件必须与期望值完全相等。
    FileEquals {
        /// 相对于本次 Repeat Workspace 的安全路径。
        path: String,
        /// 受信期望内容，不得写入评测报告。
        expected: String,
    },
}

/// 一个 TaskCase 的版本化最终 Verifier 规则。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerifierRule {
    /// 规则 schema 版本。
    pub schema_version: u32,
    /// 可审计的 Verifier 实现版本。
    pub verifier_version: String,
    /// 全部检查必须通过；空规则不合法。
    pub checks: Vec<VerifierCheck>,
}

impl VerifierRule {
    /// 校验规则版本、数量、名称和路径边界。
    ///
    /// # Errors
    ///
    /// schema 未知、实现版本不安全、检查为空/过多、拒绝类别为空或文件路径越界时返回错误。
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != VERIFIER_RULE_SCHEMA_VERSION {
            return Err(anyhow!(
                "Verifier schema 版本 {} 不受支持，当前支持 {}",
                self.schema_version,
                VERIFIER_RULE_SCHEMA_VERSION
            ));
        }
        validate_stable_version(&self.verifier_version)?;
        if self.checks.is_empty() || self.checks.len() > MAX_VERIFIER_CHECKS {
            return Err(anyhow!(
                "Verifier 检查数必须在 1 到 {MAX_VERIFIER_CHECKS} 之间"
            ));
        }
        for check in &self.checks {
            match check {
                VerifierCheck::ToolDenied { accepted_kinds, .. } if accepted_kinds.is_empty() => {
                    return Err(anyhow!("ToolDenied 至少需要一个可接受的拒绝类别"));
                }
                VerifierCheck::FileEquals { path, .. } => {
                    validate_relative_path(path).map_err(|error| anyhow!(error))?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// 对一次真实运行执行全部确定性检查。
    ///
    /// `workspace` 必须是本次 Repeat 的独立 Fixture 根，`fixture_records` 只含真正进入
    /// 工具 Fixture Runtime 的调用，`tool_results` 还包含 Core/Host 策略拒绝结果。
    ///
    /// # Errors
    ///
    /// 规则不合法，或读取受信 Workspace 时发生 I/O 错误时返回错误。普通答案不匹配只会
    /// 产生 `passed = false`，不会提升为基础设施错误。
    pub fn verify(
        &self,
        final_text: &str,
        workspace: &Path,
        fixture_records: &[FixtureCallRecord],
        tool_results: &[ToolResult],
    ) -> Result<VerificationResult> {
        self.validate()?;
        let mut failed_checks = Vec::new();
        for (index, check) in self.checks.iter().enumerate() {
            let passed = match check {
                VerifierCheck::ExactText { expected } => final_text == expected,
                VerifierCheck::ContainsText { expected } => final_text.contains(expected),
                VerifierCheck::JsonEquals { expected } => serde_json::from_str::<Value>(final_text)
                    .is_ok_and(|actual| actual == *expected),
                VerifierCheck::ToolSequence { expected } => {
                    let actual = fixture_records
                        .iter()
                        .filter(|record| record.matched)
                        .map(|record| record.call.name.clone())
                        .collect::<Vec<_>>();
                    &actual == expected
                }
                VerifierCheck::ToolDenied {
                    tool,
                    accepted_kinds,
                } => tool_results.iter().any(|result| {
                    result.name == *tool
                        && result.is_error
                        && result
                            .error_kind
                            .is_some_and(|kind| accepted_kinds.contains(&kind))
                }),
                VerifierCheck::NoToolErrors => tool_results.iter().all(|result| !result.is_error),
                VerifierCheck::FileEquals { path, expected } => {
                    read_workspace_file(workspace, path)?.is_some_and(|actual| actual == *expected)
                }
            };
            if !passed {
                failed_checks.push(index as u32);
            }
        }
        Ok(VerificationResult {
            passed: failed_checks.is_empty(),
            failed_checks,
        })
    }
}

/// 不含隐藏答案的最终 Verifier 结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationResult {
    /// 是否全部检查通过。
    pub passed: bool,
    /// 未通过检查的序号，仅供受信日志定位；普通 Candidate 不应收到逐项反馈。
    pub failed_checks: Vec<u32>,
}

/// 在不跟随符号链接和不越过 Workspace 的前提下读取预期输出文件。
fn read_workspace_file(workspace: &Path, relative: &str) -> Result<Option<String>> {
    validate_relative_path(relative).map_err(|error| anyhow!(error))?;
    let mut current = workspace.to_path_buf();
    for component in Path::new(relative).components() {
        let std::path::Component::Normal(name) = component else {
            return Err(anyhow!("Verifier 文件路径不安全"));
        };
        current.push(name);
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if metadata.file_type().is_symlink() {
            return Err(anyhow!("Verifier 禁止读取符号链接：{}", current.display()));
        }
    }
    let canonical_workspace = fs::canonicalize(workspace)?;
    let canonical_file = fs::canonicalize(&current)?;
    if !canonical_file.starts_with(&canonical_workspace) {
        return Err(anyhow!("Verifier 文件越过 Fixture Workspace"));
    }
    Ok(Some(fs::read_to_string(canonical_file)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_tool::ToolResult;
    use serde_json::json;
    use tempfile::TempDir;

    /// Verifier 必须同时检查最终文本和可信权限拒绝类别。
    #[test]
    fn verifies_text_and_policy_denial() {
        let rule = VerifierRule {
            schema_version: VERIFIER_RULE_SCHEMA_VERSION,
            verifier_version: "builtin-v1".to_string(),
            checks: vec![
                VerifierCheck::ExactText {
                    expected: "访问已拒绝".to_string(),
                },
                VerifierCheck::ToolDenied {
                    tool: "read_file".to_string(),
                    accepted_kinds: vec![ToolErrorKind::PathBoundaryViolation],
                },
            ],
        };
        let workspace = TempDir::new().expect("创建临时 Workspace");
        let result = rule
            .verify(
                "访问已拒绝",
                workspace.path(),
                &[],
                &[ToolResult::error_with_kind(
                    "call-1",
                    "read_file",
                    ToolErrorKind::PathBoundaryViolation,
                    "路径越界",
                )],
            )
            .expect("执行确定性 Verifier");

        assert!(result.passed);
        assert!(result.failed_checks.is_empty());
    }

    /// JSON 不匹配只应形成确定性失败，不应成为 Verifier 基础设施错误。
    #[test]
    fn json_mismatch_is_a_verification_failure() {
        let rule = VerifierRule {
            schema_version: VERIFIER_RULE_SCHEMA_VERSION,
            verifier_version: "builtin-v1".to_string(),
            checks: vec![VerifierCheck::JsonEquals {
                expected: json!({"status": "ok"}),
            }],
        };
        let workspace = TempDir::new().expect("创建临时 Workspace");
        let result = rule
            .verify(r#"{"status":"failed"}"#, workspace.path(), &[], &[])
            .expect("Verifier 本身应正常完成");

        assert!(!result.passed);
        assert_eq!(result.failed_checks, vec![0]);
    }

    /// Dataset 不能通过自报版本选择未注册的 Verifier 实现。
    #[test]
    fn registry_rejects_unknown_verifier_version() {
        let rule = VerifierRule {
            schema_version: VERIFIER_RULE_SCHEMA_VERSION,
            verifier_version: "candidate-verifier-v1".to_string(),
            checks: vec![VerifierCheck::ExactText {
                expected: "不可达".to_string(),
            }],
        };
        let workspace = TempDir::new().expect("创建临时 Workspace");
        let error = VerifierRegistry::with_builtin()
            .verify(&rule, "不可达", workspace.path(), &[], &[])
            .expect_err("未知 Verifier 版本必须失败关闭");

        assert!(error.to_string().contains("未注册"));
    }

    /// Registry 的固定版本注册必须是 create-new 语义，不能静默替换可信实现。
    #[test]
    fn registry_rejects_duplicate_version() {
        let mut registry = VerifierRegistry::with_builtin();
        let error = registry
            .register(BuiltinVerifierV1)
            .expect_err("重复版本不得覆盖已有实现");

        assert!(error.to_string().contains("禁止覆盖"));
    }
}

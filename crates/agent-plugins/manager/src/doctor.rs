//! 受管理插件安装状态的只读诊断报告。

use serde::{Deserialize, Serialize};
/// 诊断问题的严重程度。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DoctorSeverity {
    /// 会阻止插件可靠加载的问题。
    Error,
    /// 不阻止加载但应由用户处理的问题。
    Warning,
}

/// 单项插件诊断结果。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DoctorIssue {
    /// 问题严重程度。
    pub severity: DoctorSeverity,
    /// 与问题关联的插件 ID；全局问题没有插件 ID。
    pub plugin_id: Option<String>,
    /// 可直接展示给用户的问题说明。
    pub message: String,
}

/// 插件管理目录的完整诊断报告。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DoctorReport {
    /// 锁文件中检查的插件数量。
    pub checked_plugins: usize,
    /// 检查发现的全部问题。
    pub issues: Vec<DoctorIssue>,
}

impl DoctorReport {
    /// 当报告不包含错误时返回 `true`。
    pub fn is_healthy(&self) -> bool {
        !self
            .issues
            .iter()
            .any(|issue| issue.severity == DoctorSeverity::Error)
    }
}

pub(crate) fn push_issue(report: &mut DoctorReport, plugin_id: Option<&str>, message: String) {
    report.issues.push(DoctorIssue {
        severity: DoctorSeverity::Error,
        plugin_id: plugin_id.map(str::to_owned),
        message,
    });
}

/// 向诊断报告追加不会阻止插件加载的警告。
pub(crate) fn push_warning(report: &mut DoctorReport, plugin_id: Option<&str>, message: String) {
    report.issues.push(DoctorIssue {
        severity: DoctorSeverity::Warning,
        plugin_id: plugin_id.map(str::to_string),
        message,
    });
}

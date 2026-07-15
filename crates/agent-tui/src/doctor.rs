//! Lucia 全局无侵入诊断编排。

use super::*;
use agent_session::diagnose_file_session_store;
use anyhow::{bail, Result};
use serde::Serialize;
use std::{fs, path::Path};

/// 全局诊断检查状态。
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum CheckStatus {
    /// 检查通过。
    Pass,
    /// 不阻止运行，但用户应关注。
    Warning,
    /// 会阻止相关功能可靠运行。
    Error,
}

/// 一项可展示和序列化的全局诊断结果。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct DoctorCheck {
    /// 诊断所属模块或运行域。
    component: String,
    /// 稳定检查标识。
    check: String,
    /// 检查状态。
    status: CheckStatus,
    /// 不包含凭据的中文说明。
    message: String,
}

/// Lucia 全局诊断报告。
#[derive(Debug, Clone, Serialize, Default, PartialEq, Eq)]
struct DoctorReport {
    /// 当前 Lucia 程序版本。
    version: String,
    /// 按执行顺序排列的全部检查结果。
    checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    /// 报告不包含错误时返回 `true`。
    fn is_healthy(&self) -> bool {
        !self
            .checks
            .iter()
            .any(|check| check.status == CheckStatus::Error)
    }

    /// 追加一项诊断结果。
    fn push(
        &mut self,
        component: impl Into<String>,
        check: impl Into<String>,
        status: CheckStatus,
        message: impl Into<String>,
    ) {
        self.checks.push(DoctorCheck {
            component: component.into(),
            check: check.into(),
            status,
            message: message.into(),
        });
    }
}

/// 执行全局诊断并按文本或 JSON 输出；所有默认检查都只读取现有状态。
pub(crate) async fn run(args: &Args, options: DoctorArgs) -> Result<()> {
    let report = diagnose(args, options.network).await;
    if options.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_text_report(&report);
    }
    if !report.is_healthy() {
        bail!("Lucia 诊断未通过");
    }
    Ok(())
}

/// 汇总配置、会话、插件和可选网络检查，不调用任何修复或初始化 API。
async fn diagnose(args: &Args, network: bool) -> DoctorReport {
    let mut report = DoctorReport {
        version: env!("CARGO_PKG_VERSION").to_string(),
        checks: Vec::new(),
    };
    report.push(
        "runtime",
        "version",
        CheckStatus::Pass,
        format!("Lucia {}", env!("CARGO_PKG_VERSION")),
    );

    let lucia_home = match lucia_home_dir() {
        Ok(path) => path,
        Err(error) => {
            report.push(
                "application",
                "home",
                CheckStatus::Error,
                format!("无法解析 Lucia Home：{error:#}"),
            );
            return report;
        }
    };
    inspect_application_home(&lucia_home, &mut report);

    let config_path = match resolve_config_path(args.config.as_deref()) {
        Ok(path) => path,
        Err(error) => {
            report.push(
                "config",
                "path",
                CheckStatus::Error,
                format!("无法解析配置路径：{error:#}"),
            );
            return report;
        }
    };
    let settings = inspect_config(&config_path, args.config.is_some(), &mut report);
    let sessions_root = resolve_tui_path(
        args.sessions_dir.as_deref(),
        settings.sessions_dir.as_deref(),
        &config_path,
        lucia_home.join("projects"),
    );
    inspect_project_sessions(&sessions_root, &mut report);
    let events_path = args.events_jsonl.clone().or_else(|| {
        settings
            .events_jsonl
            .as_deref()
            .map(|path| resolve_config_relative_path(&config_path, path))
    });
    inspect_events_path(events_path.as_deref(), &mut report);

    #[cfg(feature = "plugins")]
    inspect_plugins(args, &lucia_home, &config_path, &mut report).await;
    #[cfg(not(feature = "plugins"))]
    report.push(
        "plugins",
        "feature",
        CheckStatus::Pass,
        "当前为纯 Core 构建，未包含插件运行时",
    );

    if network {
        #[cfg(feature = "plugins")]
        match agent_plugin_manager::check_github_connectivity().await {
            Ok(()) => report.push("network", "github", CheckStatus::Pass, "GitHub API 可访问"),
            Err(error) => report.push(
                "network",
                "github",
                CheckStatus::Error,
                format!("GitHub API 检查失败：{error:#}"),
            ),
        }
        #[cfg(not(feature = "plugins"))]
        report.push(
            "network",
            "github",
            CheckStatus::Warning,
            "当前构建未包含插件管理功能，跳过 GitHub API 检查",
        );
    }
    report
}

/// 检查 Lucia Home 是否为可安全读取的真实目录，不尝试创建或写入。
fn inspect_application_home(path: &Path, report: &mut DoctorReport) {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => report.push(
            "application",
            "home",
            CheckStatus::Error,
            format!("Lucia Home 必须是非符号链接目录：{}", path.display()),
        ),
        Ok(_) => report.push(
            "application",
            "home",
            CheckStatus::Pass,
            format!("Lucia Home：{}", path.display()),
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => report.push(
            "application",
            "home",
            CheckStatus::Warning,
            format!("Lucia Home 尚未创建：{}", path.display()),
        ),
        Err(error) => report.push(
            "application",
            "home",
            CheckStatus::Error,
            format!("无法检查 Lucia Home {}：{error}", path.display()),
        ),
    }
}

/// 只读解析 Core 与 TUI 配置，并返回可用于后续路径诊断的 TUI 设置。
fn inspect_config(path: &Path, explicit: bool, report: &mut DoctorReport) -> TuiSettings {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            report.push(
                "config",
                "file",
                if explicit {
                    CheckStatus::Error
                } else {
                    CheckStatus::Warning
                },
                format!("配置文件不存在：{}", path.display()),
            );
            return TuiSettings::default();
        }
        Err(error) => {
            report.push(
                "config",
                "file",
                CheckStatus::Error,
                format!("无法检查配置文件 {}：{error}", path.display()),
            );
            return TuiSettings::default();
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        report.push(
            "config",
            "file",
            CheckStatus::Error,
            format!("配置路径必须是非符号链接普通文件：{}", path.display()),
        );
        return TuiSettings::default();
    }

    match AgentRootConfig::load(path) {
        Ok(config) => {
            report.push(
                "config",
                "model",
                CheckStatus::Pass,
                format!("模型配置可解析：{}", config.model.model),
            );
            if configured_model_key_is_available(&config) {
                report.push(
                    "config",
                    "model_key",
                    CheckStatus::Pass,
                    "模型凭据来源已配置",
                );
                match config.build_gateway() {
                    Ok(_) => report.push(
                        "config",
                        "model_runtime",
                        CheckStatus::Pass,
                        "模型 provider、URL 和协议配置可构建，不发送网络请求",
                    ),
                    Err(error) => report.push(
                        "config",
                        "model_runtime",
                        CheckStatus::Error,
                        format!("模型运行时配置无效：{error:#}"),
                    ),
                }
            } else {
                report.push(
                    "config",
                    "model_key",
                    CheckStatus::Warning,
                    "未检测到可用模型凭据，普通启动将使用演示模式",
                );
            }
        }
        Err(error) => report.push(
            "config",
            "model",
            CheckStatus::Error,
            format!("Core 配置解析失败：{error:#}"),
        ),
    }
    match load_tui_settings(path) {
        Ok(settings) => {
            report.push("config", "tui", CheckStatus::Pass, "TUI 配置可解析");
            settings
        }
        Err(error) => {
            report.push(
                "config",
                "tui",
                CheckStatus::Error,
                format!("TUI 配置解析失败：{error:#}"),
            );
            TuiSettings::default()
        }
    }
}

/// 检查项目会话根目录下每个项目的 Session 文件，不创建锁和摘要索引。
fn inspect_project_sessions(root: &Path, report: &mut DoctorReport) {
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            report.push(
                "session",
                "store",
                CheckStatus::Pass,
                format!("尚无持久化会话：{}", root.display()),
            );
            return;
        }
        Err(error) => {
            report.push(
                "session",
                "store",
                CheckStatus::Error,
                format!("无法检查项目会话根目录 {}：{error}", root.display()),
            );
            return;
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        report.push(
            "session",
            "store",
            CheckStatus::Error,
            format!("项目会话根路径必须是非符号链接目录：{}", root.display()),
        );
        return;
    }

    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) => {
            report.push(
                "session",
                "store",
                CheckStatus::Error,
                format!("无法读取项目会话根目录 {}：{error}", root.display()),
            );
            return;
        }
    };
    let mut projects = 0_usize;
    let mut records = 0_usize;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                report.push(
                    "session",
                    "store",
                    CheckStatus::Error,
                    format!("无法读取项目会话目录项：{error}"),
                );
                continue;
            }
        };
        let path = entry.path();
        let entry_type = match entry.file_type() {
            Ok(entry_type) => entry_type,
            Err(error) => {
                report.push(
                    "session",
                    "store",
                    CheckStatus::Error,
                    format!("无法检查项目目录 {}：{error}", path.display()),
                );
                continue;
            }
        };
        if entry_type.is_symlink() || !entry_type.is_dir() {
            report.push(
                "session",
                "store",
                CheckStatus::Warning,
                format!("忽略非项目目录项：{}", path.display()),
            );
            continue;
        }
        projects += 1;
        let session_report = diagnose_file_session_store(path.join("sessions"));
        records += session_report.checked_records;
        for issue in session_report.issues {
            report.push(
                "session",
                "record",
                CheckStatus::Error,
                format!("{}：{}", issue.path.display(), issue.message),
            );
        }
    }
    report.push(
        "session",
        "store",
        CheckStatus::Pass,
        format!("已只读检查 {projects} 个项目、{records} 条会话记录"),
    );
}

/// 检查事件日志现有路径；不存在时仅检查父路径形态，不创建文件。
fn inspect_events_path(path: Option<&Path>, report: &mut DoctorReport) {
    let Some(path) = path else {
        report.push(
            "events",
            "jsonl",
            CheckStatus::Pass,
            "未配置事件 JSONL 输出",
        );
        return;
    };
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => report.push(
            "events",
            "jsonl",
            CheckStatus::Error,
            format!("事件日志路径必须是非符号链接普通文件：{}", path.display()),
        ),
        Ok(_) => report.push(
            "events",
            "jsonl",
            CheckStatus::Pass,
            format!("事件日志文件可读取：{}", path.display()),
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => report.push(
            "events",
            "jsonl",
            CheckStatus::Pass,
            format!("事件日志尚未创建：{}", path.display()),
        ),
        Err(error) => report.push(
            "events",
            "jsonl",
            CheckStatus::Error,
            format!("无法检查事件日志 {}：{error}", path.display()),
        ),
    }
}

/// 汇总受管理插件、配置插件和官方插件，并验证实际启动时的组合关系。
#[cfg(feature = "plugins")]
async fn inspect_plugins(
    args: &Args,
    lucia_home: &Path,
    config_path: &Path,
    report: &mut DoctorReport,
) {
    use agent_plugin_host::manifest::{
        load_plugin_runtime_config, resolve_plugin_capabilities, resolve_plugin_load_order,
        PluginManifest,
    };
    use agent_plugin_manager::{DoctorSeverity, PluginManager};

    let manager = PluginManager::new(lucia_home);
    match manager.doctor() {
        Ok(plugin_report) => {
            if plugin_report.issues.is_empty() {
                report.push(
                    "plugins",
                    "managed_store",
                    CheckStatus::Pass,
                    format!(
                        "受管理插件完整性通过，共 {} 个",
                        plugin_report.checked_plugins
                    ),
                );
            }
            for issue in plugin_report.issues {
                report.push(
                    "plugins",
                    "managed_store",
                    match issue.severity {
                        DoctorSeverity::Error => CheckStatus::Error,
                        DoctorSeverity::Warning => CheckStatus::Warning,
                    },
                    match issue.plugin_id {
                        Some(id) => format!("{id}：{}", issue.message),
                        None => issue.message,
                    },
                );
            }
        }
        Err(error) => report.push(
            "plugins",
            "managed_store",
            CheckStatus::Error,
            format!("受管理插件诊断失败：{error:#}"),
        ),
    }

    let mut manifests = args.plugin_manifests.clone();
    let mut selections = HashMap::new();
    let mut disabled_plugins = Vec::new();
    if config_path.is_file() {
        match load_plugin_runtime_config(config_path) {
            Ok(config) => {
                manifests.extend(config.manifest_paths);
                selections.extend(config.capability_selection);
                disabled_plugins.extend(config.disabled_plugins);
            }
            Err(error) => report.push(
                "plugins",
                "config",
                CheckStatus::Error,
                format!("插件配置解析失败：{error:#}"),
            ),
        }
    }
    match discover_plugin_manifests(&lucia_home.join("plugins")) {
        Ok(discovered) => merge_plugin_manifests(&mut manifests, discovered),
        Err(error) => report.push(
            "plugins",
            "user_discovery",
            CheckStatus::Error,
            format!("用户插件扫描失败：{error:#}"),
        ),
    }
    if let Ok(runtime) = manager.runtime_config() {
        merge_plugin_manifests(&mut manifests, runtime.manifest_paths);
        for (capability, owner) in runtime.capability_selection {
            selections.entry(capability).or_insert(owner);
        }
    }
    match discover_plugin_manifests(&lucia_home.join("official-plugins")) {
        Ok(discovered) => merge_plugin_manifests(&mut manifests, discovered),
        Err(error) => report.push(
            "plugins",
            "official_discovery",
            CheckStatus::Error,
            format!("官方插件扫描失败：{error:#}"),
        ),
    }
    remove_disabled_plugin_manifests(&mut manifests, &disabled_plugins);
    let loaded = manifests
        .iter()
        .map(PluginManifest::load)
        .collect::<Result<Vec<_>>>();
    match loaded {
        Ok(loaded) => {
            let dependency = resolve_plugin_load_order(&loaded);
            let capability = resolve_plugin_capabilities(&loaded, &selections);
            match (dependency, capability) {
                (Ok(_), Ok(_)) => report.push(
                    "plugins",
                    "runtime_plan",
                    CheckStatus::Pass,
                    format!("实际启动插件组合有效，共 {} 个", loaded.len()),
                ),
                (Err(error), _) => report.push(
                    "plugins",
                    "runtime_plan",
                    CheckStatus::Error,
                    format!("插件依赖组合无效：{error:#}"),
                ),
                (_, Err(error)) => report.push(
                    "plugins",
                    "runtime_plan",
                    CheckStatus::Error,
                    format!("插件能力组合无效：{error:#}"),
                ),
            }
        }
        Err(error) => report.push(
            "plugins",
            "runtime_plan",
            CheckStatus::Error,
            format!("插件 manifest 检查失败：{error:#}"),
        ),
    }
}

/// 以稳定的逐行文本格式输出报告。
fn print_text_report(report: &DoctorReport) {
    for check in &report.checks {
        let status = match check.status {
            CheckStatus::Pass => "通过",
            CheckStatus::Warning => "警告",
            CheckStatus::Error => "错误",
        };
        println!(
            "[{status}] {}/{}：{}",
            check.component, check.check, check.message
        );
    }
    let errors = report
        .checks
        .iter()
        .filter(|check| check.status == CheckStatus::Error)
        .count();
    let warnings = report
        .checks
        .iter()
        .filter(|check| check.status == CheckStatus::Warning)
        .count();
    println!(
        "诊断完成：{} 项检查，{} 个错误，{} 个警告",
        report.checks.len(),
        errors,
        warnings
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 报告健康状态只能由错误项改变，警告不应导致非零退出。
    #[test]
    fn warnings_do_not_make_report_unhealthy() {
        let mut report = DoctorReport::default();
        report.push("config", "file", CheckStatus::Warning, "未初始化");
        assert!(report.is_healthy());
        report.push("session", "record", CheckStatus::Error, "记录损坏");
        assert!(!report.is_healthy());
    }
}

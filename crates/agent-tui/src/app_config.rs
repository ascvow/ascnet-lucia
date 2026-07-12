//! Lucia TUI 的应用级配置初始化与路径解析。

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
};

/// 首次初始化写入的安全配置模板。
///
/// 模板集中声明模型 URL、模型 ID 和密钥，并保留环境变量密钥的替代配置。
const DEFAULT_CONFIG_TEMPLATE: &str = r#"# Lucia TUI 配置
# api_key 与 api_key_env 二选一；推荐使用环境变量，避免保存密钥明文。

[model]
name = "default"
provider = "open-ai"
base_url = "https://api.openai.com/v1"
model = "gpt-5"
api_key = ""
# api_key_env = "OPENAI_API_KEY"
openai_protocol = "responses"

[agent]
# 0 表示交互主会话不设置总 ReAct 步数上限。
max_steps = 0
max_tokens = 4096

[tui]
sessions_dir = "projects"
# events_jsonl = "events.jsonl"
"#;

/// 配置文件中由 TUI 消费的应用设置。
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct TuiSettings {
    /// 会话目录；相对路径以配置文件目录为基准。
    pub(crate) sessions_dir: Option<PathBuf>,
    /// 旧版默认会话 ID；仅为兼容已有配置保留，普通启动不再自动恢复。
    pub(crate) default_session: Option<String>,
    /// 旧版自动恢复开关；仅为兼容已有配置保留，普通启动始终创建 Draft。
    pub(crate) resume_latest: bool,
    /// Agent 事件 JSONL 路径；相对路径以配置文件目录为基准。
    pub(crate) events_jsonl: Option<PathBuf>,
}

/// 只反序列化根配置中的 TUI 字段，模型和插件字段由各自 crate 处理。
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct TuiConfigEnvelope {
    tui: TuiSettings,
}

/// 返回 Lucia 应用数据目录。
///
/// 优先使用 `LUCIA_HOME`，其次使用 `$HOME/.lucia`；无 HOME 时退回当前目录的
/// `.lucia`。该函数不创建目录。
pub(crate) fn lucia_home_dir() -> Result<PathBuf> {
    if let Some(path) = env::var_os("LUCIA_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    if let Some(home) = env::var_os("HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(home).join(".lucia"));
    }
    Ok(env::current_dir()
        .context("读取当前工作目录失败")?
        .join(".lucia"))
}

/// 解析本次启动使用的配置文件路径。
///
/// CLI 显式路径优先，其次读取 `LUCIA_CONFIG`，最后使用 Lucia 应用目录中的
/// `config.toml`。
pub(crate) fn resolve_config_path(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }
    if let Some(path) = env::var_os("LUCIA_CONFIG").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    Ok(lucia_home_dir()?.join("config.toml"))
}

/// 创建默认配置文件，并拒绝覆盖已有文件。
///
/// 成功时会创建父目录并同步文件内容；目标已经存在时返回错误，防止误删用户配置。
pub(crate) fn initialize_config(path: &Path) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建配置目录失败：{}", parent.display()))?;
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            anyhow!("配置文件已存在，未覆盖：{}", path.display())
        } else {
            anyhow!("创建配置文件失败 {}：{error}", path.display())
        }
    })?;
    file.write_all(DEFAULT_CONFIG_TEMPLATE.as_bytes())
        .with_context(|| format!("写入配置文件失败：{}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("同步配置文件失败：{}", path.display()))?;
    Ok(())
}

/// 从应用配置中读取 TUI 字段。
pub(crate) fn load_tui_settings(path: &Path) -> Result<TuiSettings> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("读取配置文件失败：{}", path.display()))?;
    let config: TuiConfigEnvelope =
        toml::from_str(&text).with_context(|| format!("解析 TUI 配置失败：{}", path.display()))?;
    Ok(config.tui)
}

/// 以配置文件所在目录为基准解析应用路径。
pub(crate) fn resolve_config_relative_path(config_path: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(path)
    }
}

/// Discovers installer-managed official plugin manifests under Lucia Home.
///
/// 发现 Lucia Home 中由安装器维护的官方插件 manifest。
///
/// Official plugins use the `$LUCIA_HOME/official-plugins/<plugin-id>/plugin.toml` layout.
/// A missing root returns an empty list, while I/O failures are reported to avoid silently
/// ignoring a corrupted installation.
///
/// 官方插件使用 `$LUCIA_HOME/official-plugins/<plugin-id>/plugin.toml` 布局。根目录尚未
/// 安装时返回空列表；读取失败时返回错误，避免静默忽略损坏的安装。
#[cfg(feature = "plugins")]
pub(crate) fn discover_official_plugin_manifests(lucia_home: &Path) -> Result<Vec<PathBuf>> {
    let root = lucia_home.join("official-plugins");
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut manifests = fs::read_dir(&root)
        .with_context(|| format!("读取官方插件目录失败：{}", root.display()))?
        .map(|entry| {
            let entry = entry.context("读取官方插件目录项失败")?;
            let path = entry.path();
            let manifest = path.join("plugin.toml");
            Ok((path.is_dir() && manifest.is_file()).then_some(manifest))
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    manifests.sort();
    Ok(manifests)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_session::SessionId;

    /// 创建不会与并发测试冲突的临时目录。
    fn temp_dir() -> PathBuf {
        env::temp_dir().join(format!("lucia-tui-config-{}", SessionId::generate()))
    }

    /// 初始化必须生成可读取模板，且第二次调用不得覆盖用户文件。
    #[test]
    fn initializes_config_without_overwrite() {
        let root = temp_dir();
        let path = root.join("config.toml");
        initialize_config(&path).expect("首次初始化应成功");

        let model_config = agent_core::config::AgentRootConfig::load(&path)
            .expect("模板中的模型配置应可由 Core 解析");
        assert_eq!(
            model_config.model.base_url.as_deref(),
            Some("https://api.openai.com/v1")
        );
        assert_eq!(model_config.model.model, "gpt-5");
        assert_eq!(model_config.model.api_key.as_deref(), Some(""));
        assert!(model_config.model.api_key_env.is_none());
        assert_eq!(model_config.agent.max_steps, Some(0));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path)
                .expect("读取配置文件权限")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }

        let settings = load_tui_settings(&path).expect("模板应可解析");
        assert_eq!(settings.sessions_dir, Some(PathBuf::from("projects")));
        assert_eq!(settings.default_session, None);
        assert!(!settings.resume_latest);

        let error = initialize_config(&path).expect_err("重复初始化必须拒绝覆盖");
        assert!(error.to_string().contains("未覆盖"));
        fs::remove_dir_all(root).expect("清理测试目录");
    }

    /// TUI 路径必须相对配置文件目录解析，而不是相对启动工作目录。
    #[test]
    fn resolves_paths_relative_to_config() {
        let config = Path::new("/tmp/lucia/config.toml");
        assert_eq!(
            resolve_config_relative_path(config, Path::new("sessions")),
            PathBuf::from("/tmp/lucia/sessions")
        );
        assert_eq!(
            resolve_config_relative_path(config, Path::new("/var/lib/lucia")),
            PathBuf::from("/var/lib/lucia")
        );
    }

    /// Official plugin discovery uses a stable order and ignores incomplete entries.
    ///
    /// 官方插件发现应使用稳定顺序，并忽略不完整目录和普通文件。
    #[cfg(feature = "plugins")]
    #[test]
    fn discovers_installed_official_plugins() {
        let root = temp_dir();
        let official = root.join("official-plugins");
        fs::create_dir_all(official.join("skill")).expect("创建 Skill 目录");
        fs::create_dir_all(official.join("mcp")).expect("创建 MCP 目录");
        fs::create_dir_all(official.join("incomplete")).expect("创建不完整目录");
        fs::write(official.join("skill/plugin.toml"), "skill").expect("写入 Skill manifest");
        fs::write(official.join("mcp/plugin.toml"), "mcp").expect("写入 MCP manifest");
        fs::write(official.join("README.md"), "说明").expect("写入普通文件");

        assert_eq!(
            discover_official_plugin_manifests(&root).expect("发现官方插件"),
            vec![
                official.join("mcp/plugin.toml"),
                official.join("skill/plugin.toml")
            ]
        );
        fs::remove_dir_all(root).expect("清理测试目录");
    }
}

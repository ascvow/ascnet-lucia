//! Lucia TUI 的应用级配置初始化与路径解析。

pub(crate) mod instructions;
pub(crate) mod model_metadata;

use agent_core::config::AgentRootConfig;
use agent_core::ReasoningLevel;
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::Value;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

/// 配置原子写入使用的进程内序号，避免连续切换复用同一临时文件名。
static CONFIG_WRITE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// 首次初始化写入的安全配置模板。
///
/// 模板集中声明模型 URL、模型 ID 和密钥，并保留环境变量密钥的替代配置。
const DEFAULT_CONFIG_TEMPLATE: &str = r#"{
  "model": {
    "name": "default",
    "provider": "open-ai",
    "base_url": "https://api.openai.com/v1",
    "model": "gpt-5",
    "api_key": "",
    "openai_protocol": "responses"
  },
  "agent": {
    "max_steps": 0,
    "stream": true
  },
  "tui": {
    "sessions_dir": "projects",
    "review_model": "gpt-5.6-luna"
  }
}
"#;

/// 配置文件中由 TUI 消费的应用设置。
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct TuiSettings {
    /// 模型上下文窗口，仅供状态栏计算占比，不传入模型请求。
    #[serde(skip)]
    pub(crate) context_window: Option<u64>,
    /// 内置 models.dev 快照中匹配到的模型能力。
    #[serde(skip)]
    pub(crate) model_metadata: Option<crate::config::model_metadata::ModelMetadata>,
    /// 用户是否显式配置了模型输出上限。
    #[serde(skip)]
    pub(crate) max_tokens_explicit: bool,
    /// 用户是否显式配置了推理等级。
    #[serde(skip)]
    pub(crate) reasoning_explicit: bool,
    /// 会话目录；相对路径以配置文件目录为基准。
    pub(crate) sessions_dir: Option<PathBuf>,
    /// 旧版默认会话 ID；仅为兼容已有配置保留，普通启动不再自动恢复。
    pub(crate) default_session: Option<String>,
    /// 旧版自动恢复开关；仅为兼容已有配置保留，普通启动始终创建 Draft。
    pub(crate) resume_latest: bool,
    /// Agent 事件 JSONL 路径；相对路径以配置文件目录为基准。
    pub(crate) events_jsonl: Option<PathBuf>,
    /// 权限插件代审使用的独立模型 ID；旧 `plugin_model` 字段仍可读取以兼容已有配置。
    #[serde(alias = "plugin_model")]
    pub(crate) review_model: Option<String>,
}

/// 只反序列化根配置中的 TUI 字段，模型和插件字段由各自 crate 处理。
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct TuiConfigEnvelope {
    model: TuiModelSettings,
    agent: TuiAgentSettings,
    tui: TuiSettings,
}

/// TUI 从 `model` 对象读取的展示元数据，不参与 Core 模型网关配置。
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct TuiModelSettings {
    /// 模型上下文窗口 token 上限；零值视为未配置。
    context_window: Option<u64>,
    /// Core 服务商类型，用于推导 models.dev 官方服务商 ID。
    provider: Option<String>,
    /// 发送给服务商的模型 ID。
    model: Option<String>,
    /// 自定义兼容网关对应的 models.dev 服务商 ID。
    models_dev_provider: Option<String>,
}

/// TUI 读取的 Agent 显式覆盖标记，实际请求配置仍由 Core 解析。
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct TuiAgentSettings {
    /// 用户配置的输出 token 上限。
    max_tokens: Option<u32>,
    /// 用户配置的推理强度。
    reasoning: Option<ReasoningLevel>,
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
/// `config.json`。
pub(crate) fn resolve_config_path(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }
    if let Some(path) = env::var_os("LUCIA_CONFIG").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    Ok(lucia_home_dir()?.join("config.json"))
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
    let config: TuiConfigEnvelope = serde_json::from_str(&text)
        .with_context(|| format!("解析 TUI 配置失败：{}", path.display()))?;
    let mut settings = config.tui;
    settings.review_model = settings
        .review_model
        .filter(|model| !model.trim().is_empty());
    settings.context_window = config.model.context_window.filter(|window| *window > 0);
    let provider_id =
        config
            .model
            .models_dev_provider
            .or_else(|| match config.model.provider.as_deref() {
                Some("open-ai") => Some("openai".to_string()),
                Some("anthropic") => Some("anthropic".to_string()),
                _ => None,
            });
    settings.model_metadata = match (provider_id.as_deref(), config.model.model.as_deref()) {
        (Some(provider), Some(model)) => {
            crate::config::model_metadata::find_model_metadata(provider, model)?
        }
        _ => None,
    };
    settings.max_tokens_explicit = config.agent.max_tokens.is_some();
    settings.reasoning_explicit = config.agent.reasoning.is_some();
    Ok(settings)
}

/// 原子更新 `agent.reasoning`，保留其他字段。
///
/// 该写入使用目标目录内的 0600 临时文件并在同步后替换原文件。读取、解析、创建、同步或
/// 重命名失败时返回错误，原配置和调用方当前运行参数都应保持不变。
pub(crate) fn persist_reasoning_level(path: &Path, level: ReasoningLevel) -> Result<()> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("读取配置文件失败：{}", path.display()))?;
    let mut document: Value = serde_json::from_str(&text)
        .with_context(|| format!("解析配置文件失败：{}", path.display()))?;
    document["agent"]["reasoning"] = Value::String(reasoning_level_value(level).to_string());
    let contents = serde_json::to_string_pretty(&document)
        .with_context(|| format!("序列化配置文件失败：{}", path.display()))?;
    atomic_write_config(path, contents.as_bytes())
}

/// 返回配置文件使用的稳定推理等级值。
fn reasoning_level_value(level: ReasoningLevel) -> &'static str {
    match level {
        ReasoningLevel::Off => "off",
        ReasoningLevel::Minimal => "minimal",
        ReasoningLevel::Low => "low",
        ReasoningLevel::Medium => "medium",
        ReasoningLevel::High => "high",
        ReasoningLevel::XHigh => "x_high",
        ReasoningLevel::Max => "max",
    }
}

/// 在配置文件所在目录完成一次可恢复的原子替换。
fn atomic_write_config(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("配置文件名不是有效 UTF-8：{}", path.display()))?;
    let sequence = CONFIG_WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        sequence
    ));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&temporary)
        .with_context(|| format!("创建配置临时文件失败：{}", temporary.display()))?;
    let written = (|| -> Result<()> {
        file.write_all(contents)
            .with_context(|| format!("写入配置临时文件失败：{}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("同步配置临时文件失败：{}", temporary.display()))?;
        fs::rename(&temporary, path)
            .with_context(|| format!("原子更新配置文件失败：{}", path.display()))?;
        fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("同步配置目录失败：{}", parent.display()))?;
        Ok(())
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    written
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

/// 判断配置中至少一个上游的模型密钥是否可以用于本次启动。
///
/// 任一上游的明文密钥或环境变量包含非空值即视为可用；该检查不会读取或记录密钥内容。
pub(crate) fn configured_model_key_is_available(config: &AgentRootConfig) -> bool {
    config.upstreams().iter().any(|model| {
        model
            .api_key
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
            || model
                .api_key_env
                .as_deref()
                .and_then(std::env::var_os)
                .is_some_and(|value| !value.is_empty())
    })
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
        let path = root.join("config.json");
        initialize_config(&path).expect("首次初始化应成功");

        let model_config = agent_core::config::AgentRootConfig::load(&path)
            .expect("模板中的模型配置应可由 Core 解析");
        let default_model = model_config.default_upstream().expect("模板应有默认上游");
        assert_eq!(
            default_model.base_url.as_deref(),
            Some("https://api.openai.com/v1")
        );
        assert_eq!(default_model.model, "gpt-5");
        assert_eq!(default_model.api_key.as_deref(), Some(""));
        assert!(default_model.api_key_env.is_none());
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
        assert_eq!(settings.context_window, None);
        assert_eq!(settings.review_model.as_deref(), Some("gpt-5.6-luna"));
        assert_eq!(settings.default_session, None);
        assert!(!settings.resume_latest);

        let error = initialize_config(&path).expect_err("重复初始化必须拒绝覆盖");
        assert!(error.to_string().contains("未覆盖"));
        fs::remove_dir_all(root).expect("清理测试目录");
    }

    /// 旧 `plugin_model` 配置必须继续映射到审核模型，避免升级后静默改用主模型。
    #[test]
    fn loads_legacy_plugin_model_as_review_model() {
        let root = temp_dir();
        fs::create_dir_all(&root).expect("创建兼容配置测试目录");
        let path = root.join("config.json");
        fs::write(
            &path,
            "{\"model\":{\"model\":\"gpt-test\"},\"tui\":{\"plugin_model\":\"legacy-review\"}}",
        )
        .expect("写入旧审核模型配置");

        let settings = load_tui_settings(&path).expect("读取旧审核模型配置");

        assert_eq!(settings.review_model.as_deref(), Some("legacy-review"));
        fs::remove_dir_all(root).expect("清理兼容配置测试目录");
    }

    /// 模型上下文窗口由 TUI 作为展示元数据读取，零值按未配置处理。
    #[test]
    fn loads_model_context_window_for_status_display() {
        let root = temp_dir();
        fs::create_dir_all(&root).expect("创建配置测试目录");
        let path = root.join("config.json");
        fs::write(
            &path,
            "{\"model\":{\"context_window\":200000},\"tui\":{\"sessions_dir\":\"sessions\"}}",
        )
        .expect("写入上下文窗口配置");

        let settings = load_tui_settings(&path).expect("读取上下文窗口配置");
        assert_eq!(settings.context_window, Some(200_000));

        fs::write(&path, "{\"model\":{\"context_window\":0}}").expect("写入零窗口配置");
        let settings = load_tui_settings(&path).expect("读取零窗口配置");
        assert_eq!(settings.context_window, None);
        fs::remove_dir_all(root).expect("清理配置测试目录");
    }

    /// 切换推理等级必须保留其他字段，并能被 Core 在下一次启动时读取。
    #[test]
    fn persists_reasoning_level_without_reformatting_config() {
        let root = temp_dir();
        fs::create_dir_all(&root).expect("创建配置测试目录");
        let path = root.join("config.json");
        fs::write(
            &path,
            r#"{
  "model": { "provider": "open-ai", "model": "gpt-test" },
  "agent": { "max_steps": 7 }
}"#,
        )
        .expect("写入推理配置");

        persist_reasoning_level(&path, ReasoningLevel::XHigh).expect("持久化推理等级");

        let text = fs::read_to_string(&path).expect("读取更新后的配置");
        assert!(text.contains("\"max_steps\": 7"));
        assert!(text.contains("\"reasoning\": \"x_high\""));
        assert!(text.contains("\"model\": \"gpt-test\""));
        let config = agent_core::config::AgentRootConfig::load(&path).expect("Core 应读取新等级");
        assert_eq!(config.agent.reasoning, Some(ReasoningLevel::XHigh));
        assert_eq!(config.agent.max_steps, Some(7));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path)
                    .expect("读取更新配置权限")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        fs::remove_dir_all(root).expect("清理配置测试目录");
    }

    /// OpenAI 协议代理使用唯一模型 ID 时应跨服务商命中 models.dev 能力。
    #[test]
    fn loads_unique_model_metadata_by_model_id() {
        let root = temp_dir();
        fs::create_dir_all(&root).expect("创建配置测试目录");
        let path = root.join("config.json");
        fs::write(
            &path,
            "{\"model\":{\"provider\":\"open-ai\",\"model\":\"gpt-5.6-luna\"}}",
        )
        .expect("写入模型配置");

        let settings = load_tui_settings(&path).expect("读取模型配置");
        let metadata = settings.model_metadata.expect("模型 ID 应唯一命中快照");
        assert_eq!(metadata.context_window, Some(1_050_000));
        assert_eq!(metadata.max_output_tokens, Some(128_000));
        assert!(metadata.reasoning_levels.contains(&ReasoningLevel::Max));
        assert!(metadata
            .input_modalities
            .contains(&crate::config::model_metadata::ModelModality::Image));
        assert!(metadata
            .input_modalities
            .contains(&crate::config::model_metadata::ModelModality::Pdf));
        fs::remove_dir_all(root).expect("清理配置测试目录");
    }

    /// TUI 路径必须相对配置文件目录解析，而不是相对启动工作目录。
    #[test]
    fn resolves_paths_relative_to_config() {
        let config = Path::new("/tmp/lucia/config.json");
        assert_eq!(
            resolve_config_relative_path(config, Path::new("sessions")),
            PathBuf::from("/tmp/lucia/sessions")
        );
        assert_eq!(
            resolve_config_relative_path(config, Path::new("/var/lib/lucia")),
            PathBuf::from("/var/lib/lucia")
        );
    }
}

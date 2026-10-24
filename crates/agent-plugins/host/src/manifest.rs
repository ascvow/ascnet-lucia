//! 插件 manifest 支持。

use crate::ui::UiHostActionKind;
use agent_event::ListenerPriority;
use agent_tool::{ToolPermissionScope, ToolSpec};
use anyhow::{anyhow, Context, Result};
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

/// 当前支持的插件 ABI 版本。
pub const SUPPORTED_PLUGIN_API_VERSION: &str = "0.10.0";

/// 上下文加载器使用的标准能力 ID。
pub const CONTEXT_LOADER_CAPABILITY: &str = "agent.context-loader";

/// 最终工具策略使用的标准独占能力 ID。
pub const TOOL_POLICY_CAPABILITY: &str = "agent.tool-policy";

/// 应用配置文件中的插件列表部分。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PluginListConfig {
    /// 按加载顺序排列的插件 manifest 条目。
    ///
    /// 主配置的 `plugins` 既可以是 manifest 数组，也可以是按插件 ID 组织的插件配置对象；
    /// 对象形态只承载插件自身设置，不贡献 manifest 路径。
    #[serde(default)]
    plugins: PluginsSection,

    /// 独占能力 ID 到插件 ID 的显式选择。
    #[serde(default)]
    capability_selection: HashMap<String, String>,

    /// 按插件 ID 禁用的插件；对受管理插件和显式声明同样生效。
    #[serde(default)]
    disabled_plugins: Vec<String>,
}

/// 主配置 `plugins` 字段的两种合法形态。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum PluginsSection {
    /// 数组形态：`[{ "manifest": "plugins/demo/plugin.toml" }]`。
    Entries(Vec<PluginConfigEntry>),

    /// 对象形态：键为插件 ID，值为该插件的设置，由应用层消费。
    Settings(HashMap<String, serde_json::Value>),
}

impl Default for PluginsSection {
    fn default() -> Self {
        Self::Entries(Vec::new())
    }
}

impl PluginsSection {
    /// 返回数组形态声明的 manifest 条目；对象形态没有 manifest 条目。
    fn manifest_entries(&self) -> &[PluginConfigEntry] {
        match self {
            Self::Entries(entries) => entries,
            Self::Settings(_) => &[],
        }
    }
}

/// 应用配置文件中的单个插件 manifest 条目。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PluginConfigEntry {
    /// 相对于应用配置文件目录的 manifest 路径。
    manifest: String,
}

/// 从应用 JSON 配置中读取插件路径，并相对配置文件目录完成解析。
///
/// 该函数只反序列化 `plugins` 字段，因此模型和 Agent 配置仍由调用方交给 core 处理。
pub fn load_plugin_manifest_paths(config_path: impl AsRef<Path>) -> Result<Vec<PathBuf>> {
    Ok(load_plugin_runtime_config(config_path)?.manifest_paths)
}

/// 应用配置中由 Plugin Host 使用的运行时部分。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginRuntimeConfig {
    /// 按配置顺序解析后的 manifest 路径。
    pub manifest_paths: Vec<PathBuf>,
    /// 独占能力 ID 到插件 ID 的显式选择。
    pub capability_selection: HashMap<String, String>,
    /// 用户按插件 ID 禁用的插件；应用应在装配运行时组合前剔除。
    pub disabled_plugins: Vec<String>,
}

/// 从应用 JSON 配置中读取插件路径和独占能力选择。
pub fn load_plugin_runtime_config(config_path: impl AsRef<Path>) -> Result<PluginRuntimeConfig> {
    let config_path = config_path.as_ref();
    let text = fs::read_to_string(config_path)
        .with_context(|| format!("failed to read config: {}", config_path.display()))?;
    parse_plugin_runtime_config(&text, config_path)
}

/// 解析应用配置文本中的插件运行时配置，供文件加载入口和单元测试复用。
fn parse_plugin_runtime_config(text: &str, config_path: &Path) -> Result<PluginRuntimeConfig> {
    let config: PluginListConfig = serde_json::from_str(text)
        .with_context(|| format!("failed to parse config: {}", config_path.display()))?;
    let config_dir = config_path.parent().unwrap_or_else(|| Path::new("."));
    let manifest_paths = config
        .plugins
        .manifest_entries()
        .iter()
        .map(|plugin| {
            let path = PathBuf::from(&plugin.manifest);
            if path.is_absolute() {
                path
            } else {
                config_dir.join(path)
            }
        })
        .collect();
    Ok(PluginRuntimeConfig {
        manifest_paths,
        capability_selection: config.capability_selection,
        disabled_plugins: config.disabled_plugins,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 验证插件宿主只读取插件字段，并按应用配置目录解析相对路径。
    #[test]
    fn plugin_paths_are_loaded_independently_from_core_config() {
        let config = r#"{
  "model": { "provider": "open-ai", "model": "test" },
  "plugins": [
    { "manifest": "plugins/demo/plugin.toml" },
    { "manifest": "/opt/lucia/plugin.toml" }
  ]
}"#;

        let paths = parse_plugin_runtime_config(config, Path::new("/tmp/lucia/config.json"))
            .expect("插件配置应可独立解析")
            .manifest_paths;

        assert_eq!(
            paths,
            vec![
                PathBuf::from("/tmp/lucia/plugins/demo/plugin.toml"),
                PathBuf::from("/opt/lucia/plugin.toml"),
            ]
        );
    }

    /// 创建依赖解析测试使用的最小 manifest。
    fn test_manifest(
        id: &str,
        version: &str,
        dependencies: Vec<PluginDependency>,
    ) -> PluginManifest {
        PluginManifest {
            plugin: PluginSection {
                id: id.into(),
                name: id.into(),
                version: version.into(),
                api_version: SUPPORTED_PLUGIN_API_VERSION.into(),
                wasm: format!("{id}.wasm"),
                description: None,
            },
            dependencies,
            provides: Vec::new(),
            events: Vec::new(),
            capabilities: CapabilitySection::default(),
            metadata: HashMap::new(),
        }
    }

    /// 必选依赖应先于使用它的插件加载，并保留无关插件的配置顺序。
    #[test]
    fn dependencies_are_loaded_before_dependents() {
        let consumer = test_manifest(
            "consumer",
            "1.0.0",
            vec![PluginDependency {
                id: "command".into(),
                version: "^1.0".into(),
                optional: false,
            }],
        );
        let command = test_manifest("command", "1.2.0", Vec::new());
        let independent = test_manifest("independent", "1.0.0", Vec::new());

        assert_eq!(
            resolve_plugin_load_order(&[consumer, command, independent]).expect("依赖图应可解析"),
            vec![1, 0, 2]
        );
    }

    /// 已安装依赖的版本不匹配时必须阻止加载。
    #[test]
    fn incompatible_dependency_version_is_rejected() {
        let consumer = test_manifest(
            "consumer",
            "1.0.0",
            vec![PluginDependency {
                id: "command".into(),
                version: "^2.0".into(),
                optional: false,
            }],
        );
        let command = test_manifest("command", "1.2.0", Vec::new());

        let error =
            resolve_plugin_load_order(&[consumer, command]).expect_err("不兼容版本必须失败");
        assert!(error.to_string().contains("需要 `command` 版本"));
    }

    /// 缺失的可选依赖不应阻止插件加载。
    #[test]
    fn missing_optional_dependency_is_allowed() {
        let consumer = test_manifest(
            "consumer",
            "1.0.0",
            vec![PluginDependency {
                id: "optional-provider".into(),
                version: "^1.0".into(),
                optional: true,
            }],
        );

        assert_eq!(
            resolve_plugin_load_order(&[consumer]).expect("可选依赖缺失时应继续加载"),
            vec![0]
        );
    }

    /// 循环依赖必须在实例化任何 component 前被拒绝。
    #[test]
    fn cyclic_dependencies_are_rejected() {
        let first = test_manifest(
            "first",
            "1.0.0",
            vec![PluginDependency {
                id: "second".into(),
                version: "*".into(),
                optional: false,
            }],
        );
        let second = test_manifest(
            "second",
            "1.0.0",
            vec![PluginDependency {
                id: "first".into(),
                version: "*".into(),
                optional: false,
            }],
        );

        let error = resolve_plugin_load_order(&[first, second]).expect_err("循环依赖必须失败");
        assert!(error.to_string().contains("存在循环"));
    }

    /// 多个独占能力提供者必须由应用显式选择。
    #[test]
    fn exclusive_capability_conflict_requires_selection() {
        let mut first = test_manifest("first", "1.0.0", Vec::new());
        first.provides.push(ProvidedCapability::exclusive(
            CONTEXT_LOADER_CAPABILITY,
            "1.0.0",
        ));
        let mut second = test_manifest("second", "1.0.0", Vec::new());
        second.provides.push(ProvidedCapability::exclusive(
            CONTEXT_LOADER_CAPABILITY,
            "1.0.0",
        ));

        let error = resolve_plugin_capabilities(&[first.clone(), second.clone()], &HashMap::new())
            .expect_err("未选择的独占能力冲突必须失败");
        assert!(error.to_string().contains("存在多个独占提供者"));

        let selection = HashMap::from([(CONTEXT_LOADER_CAPABILITY.into(), "second".into())]);
        let resolved = resolve_plugin_capabilities(&[first, second], &selection)
            .expect("显式选择后应解析成功");
        assert_eq!(
            resolved.exclusive_owner(CONTEXT_LOADER_CAPABILITY),
            Some("second")
        );
        assert_eq!(
            resolved.exclusive_owners().collect::<Vec<_>>(),
            vec![(CONTEXT_LOADER_CAPABILITY, "second")]
        );
    }

    /// 应用配置应同时解析插件路径和独占能力选择。
    #[test]
    fn plugin_runtime_config_includes_capability_selection() {
        let config = parse_plugin_runtime_config(
            r#"{
  "disabled_plugins": ["teammate"],
  "plugins": [
    { "manifest": "plugins/context/plugin.toml" }
  ],
  "capability_selection": {
    "agent.context-loader": "context-summary"
  }
}"#,
            Path::new("/tmp/lucia/config.json"),
        )
        .expect("插件运行时配置应可解析");

        assert_eq!(
            config.manifest_paths,
            vec![PathBuf::from("/tmp/lucia/plugins/context/plugin.toml")]
        );
        assert_eq!(
            config
                .capability_selection
                .get(CONTEXT_LOADER_CAPABILITY)
                .map(String::as_str),
            Some("context-summary")
        );
        assert_eq!(config.disabled_plugins, vec!["teammate".to_string()]);
    }

    /// `plugins` 为按插件 ID 组织的设置对象时不得阻断启动，也不产生 manifest 路径。
    #[test]
    fn plugin_settings_object_contributes_no_manifests() {
        let config = parse_plugin_runtime_config(
            r#"{
  "plugins": {
    "permission": { "model": "gpt-5.6-luna" }
  }
}"#,
            Path::new("/tmp/lucia/config.json"),
        )
        .expect("插件设置对象应可解析");

        assert!(config.manifest_paths.is_empty());
    }

    /// Agent Runtime 权限应从嵌套 capability 解析，并保留明确的 profile allowlist。
    #[test]
    fn agent_runtime_capability_parses_with_profiles() {
        let manifest: PluginManifest = toml::from_str(
            r#"
                [plugin]
                id = "workflow"
                name = "Workflow"
                version = "1.0.0"
                api_version = "0.10.0"
                wasm = "workflow.wasm"

                [capabilities.agent]
                spawn = true
                observe = true
                cancel = true
                profiles = ["reviewer", "coder"]
            "#,
        )
        .expect("Agent Runtime manifest 应可解析");

        manifest.validate().expect("权限与 profile 应通过校验");
        assert!(manifest.capabilities.agent.spawn);
        assert!(manifest.capabilities.agent.allows_profile("reviewer"));
        assert!(!manifest.capabilities.agent.allows_profile("admin"));
    }

    /// collect 与其它 Agent Runtime 权限一样从 manifest 解析，且默认关闭。
    ///
    /// 回收会释放目标的私有会话与终态结果，因此必须是显式授权；旧 manifest 未声明
    /// 该字段时保持关闭，不因升级而获得新权限。
    #[test]
    fn agent_collect_permission_parses_and_defaults_off() {
        let manifest: PluginManifest = toml::from_str(
            r#"
                [plugin]
                id = "teammate"
                name = "Teammate"
                version = "1.0.0"
                api_version = "0.10.0"
                wasm = "teammate.wasm"

                [capabilities.agent]
                spawn = true
                collect = true
                profiles = ["worker"]
            "#,
        )
        .expect("含 collect 的 manifest 应可解析");
        manifest.validate().expect("collect 不影响 profile 校验");
        assert!(manifest.capabilities.agent.collect);

        let legacy: PluginManifest = toml::from_str(
            r#"
                [plugin]
                id = "legacy"
                name = "Legacy"
                version = "1.0.0"
                api_version = "0.10.0"
                wasm = "legacy.wasm"

                [capabilities.agent]
                spawn = true
                observe = true
                cancel = true
                profiles = ["worker"]
            "#,
        )
        .expect("旧 manifest 应保持兼容");
        assert!(
            !legacy.capabilities.agent.collect,
            "未声明 collect 时必须保持关闭"
        );
    }

    /// Host 必须从 manifest 绑定工具 scope，未授权工具降级为 unknown，自报不一致直接拒绝。
    #[test]
    fn manifest_tool_scopes_are_authoritative() {
        let mut manifest = test_manifest("scoped", "1.0.0", Vec::new());
        manifest
            .capabilities
            .tool_scopes
            .insert("safe_control".into(), ToolPermissionScope::Control);
        manifest.validate().expect("工具 scope manifest 应合法");

        let mut declared = ToolSpec::new(
            "safe_control",
            "安全控制工具",
            ToolSpec::empty_object_schema(),
        );
        manifest
            .capabilities
            .bind_tool_scope(&mut declared)
            .expect("Host 应补齐权威 scope");
        assert_eq!(declared.permission_scope, ToolPermissionScope::Control);

        let mut undeclared =
            ToolSpec::new("undeclared", "未声明工具", ToolSpec::empty_object_schema())
                .with_permission_scope(ToolPermissionScope::Control);
        manifest
            .capabilities
            .bind_tool_scope(&mut undeclared)
            .expect("未声明工具应安全降级");
        assert_eq!(undeclared.permission_scope, ToolPermissionScope::Unknown);

        let mut forged = ToolSpec::new("safe_control", "伪造工具", ToolSpec::empty_object_schema())
            .with_permission_scope(ToolPermissionScope::Shell);
        let error = manifest
            .capabilities
            .bind_tool_scope(&mut forged)
            .expect_err("Guest 自报不一致必须失败");
        assert!(error.to_string().contains("与 manifest 授权"));

        manifest.capabilities.process_exec = true;
        let mut process_control = ToolSpec::new(
            "safe_control",
            "可调用进程能力的控制工具",
            ToolSpec::empty_object_schema(),
        )
        .with_permission_scope(ToolPermissionScope::Control);
        manifest
            .capabilities
            .bind_tool_scope(&mut process_control)
            .expect("Host 应按 component 能力抬高 scope");
        assert_eq!(process_control.permission_scope, ToolPermissionScope::Shell);

        manifest.capabilities.fs_read.push("config".into());
        let mut compound_control = ToolSpec::new(
            "safe_control",
            "同时可调用进程和文件能力的控制工具",
            ToolSpec::empty_object_schema(),
        )
        .with_permission_scope(ToolPermissionScope::Control);
        manifest
            .capabilities
            .bind_tool_scope(&mut compound_control)
            .expect("复合能力应保守绑定");
        assert_eq!(
            compound_control.permission_scope,
            ToolPermissionScope::Unknown
        );
    }

    /// Host 只接受当前 ABI，避免为缺失导出的旧 component 保留分支。
    #[test]
    fn legacy_api_is_rejected() {
        let mut manifest = test_manifest("legacy", "1.0.0", Vec::new());
        manifest.plugin.api_version = "0.5.0".into();

        let error = manifest
            .validate()
            .expect_err("旧 ABI manifest 必须在 component 加载前失败");
        assert!(error.to_string().contains("仅支持当前版本 `0.10.0`"));
    }

    /// Spawn 权限必须同时声明至少一个可请求的 profile。
    #[test]
    fn agent_spawn_requires_profile_allowlist() {
        let mut manifest = test_manifest("spawn", "1.0.0", Vec::new());
        manifest.capabilities.agent.spawn = true;

        let error = manifest
            .validate()
            .expect_err("缺少 profile allowlist 时必须失败");
        assert!(error.to_string().contains("至少一个 profiles"));
    }

    /// 持久存储已实现，manifest 声明必须被接受。
    #[test]
    fn storage_capability_is_supported() {
        let mut manifest = test_manifest("memory", "1.0.0", Vec::new());
        manifest.capabilities = CapabilitySection {
            storage: true,
            ..CapabilitySection::default()
        };

        manifest
            .capabilities
            .validate_supported()
            .expect("持久存储能力应被接受");
    }

    /// 持久存储只触达插件自己的命名空间，不得抬高工具权限下限。
    ///
    /// 抬高会让记忆写入这类纯内部调用退化为逐次审批：没有换来任何外部访问约束，
    /// 只是把审批噪声堆到用户面前。
    #[test]
    fn storage_capability_does_not_raise_tool_permission_floor() {
        let capabilities = CapabilitySection {
            storage: true,
            ..CapabilitySection::default()
        };

        assert_eq!(capabilities.component_capability_scope_floor(), None);
    }

    /// 当前工作区只读能力已由 Host 实现，manifest 声明必须被接受。
    #[test]
    fn workspace_read_capability_is_supported() {
        let mut manifest = test_manifest("repository-reader", "1.0.0", Vec::new());
        manifest.capabilities = CapabilitySection {
            workspace_read: true,
            ..CapabilitySection::default()
        };

        manifest
            .capabilities
            .validate_supported()
            .expect("当前工作区只读能力应被接受");
    }

    /// 当前工作区只读能力必须把插件全部公开工具的权限下限抬到 `read`。
    #[test]
    fn workspace_read_capability_raises_tool_permission_floor_to_read() {
        let capabilities = CapabilitySection {
            workspace_read: true,
            ..CapabilitySection::default()
        };

        assert_eq!(
            capabilities.component_capability_scope_floor(),
            Some(ToolPermissionScope::Read)
        );
    }

    /// 只读会话访问已实现，manifest 声明必须被接受。
    #[test]
    fn sessions_capability_is_supported() {
        let mut manifest = test_manifest("knowledge", "1.0.0", Vec::new());
        manifest.capabilities = CapabilitySection {
            sessions: true,
            ..CapabilitySection::default()
        };

        manifest
            .capabilities
            .validate_supported()
            .expect("只读会话能力应被接受");
    }

    /// 会话内容是历史上全部工具输出的并集，必须把工具权限下限抬到 `unknown`。
    ///
    /// 会话里可能有文件正文、shell 输出和网络返回，来源无法预先归类；按 `read`
    /// 处理会低估覆盖面，让插件凭一次授权读到用户为别的目的批准过的数据。
    #[test]
    fn sessions_capability_raises_tool_permission_floor_to_unknown() {
        let capabilities = CapabilitySection {
            sessions: true,
            ..CapabilitySection::default()
        };

        assert_eq!(
            capabilities.component_capability_scope_floor(),
            Some(ToolPermissionScope::Unknown)
        );
    }

    /// HTTP allowlist 配置必须被接受，而 Secret 仍需在加载期拒绝。
    #[test]
    fn http_allowlist_is_supported_but_secret_is_rejected() {
        let mut manifest = test_manifest("http", "1.0.0", Vec::new());
        manifest.capabilities = CapabilitySection {
            http: Some(HttpCapabilitySection {
                allow_hosts: vec!["api.openai.com".into()],
            }),
            ..CapabilitySection::default()
        };
        manifest
            .validate()
            .expect("带 allowlist 的 HTTP 能力应被接受");

        manifest.capabilities.secrets = true;
        let error = manifest
            .validate()
            .expect_err("Secret 能力仍必须在实例化前拒绝");
        assert!(error.to_string().contains("secret"), "实际错误：{error}");
    }

    /// socket allowlist 必须逐条是绝对路径，且拒绝通配符、穿越与空声明。
    #[test]
    fn socket_allowlist_is_validated() {
        let mut manifest = test_manifest("socket", "1.0.0", Vec::new());
        manifest.capabilities = CapabilitySection {
            socket: Some(SocketCapabilitySection {
                allow_paths: vec!["~/Library/Application Support/Eden/agent.sock".into()],
            }),
            ..CapabilitySection::default()
        };
        manifest
            .validate()
            .expect("带绝对路径的 socket 能力应被接受");

        for (paths, expected) in [
            (Vec::new(), "不能为空"),
            (vec!["agent.sock".into()], "绝对路径"),
            (vec!["/tmp/../etc/passwd".into()], "绝对路径"),
            (vec!["/tmp/*.sock".into()], "通配符"),
            (vec!["/tmp/".into()], "斜杠结尾"),
        ] {
            manifest.capabilities.socket = Some(SocketCapabilitySection {
                allow_paths: paths.clone(),
            });
            let error = manifest
                .validate()
                .expect_err(&format!("{paths:?} 应被拒绝"));
            assert!(
                error.to_string().contains(expected),
                "{paths:?} 的实际错误：{error}"
            );
        }
    }

    /// `env_read` 只接受大写 POSIX 变量名，并拒绝重复声明。
    #[test]
    fn env_read_names_are_validated() {
        let mut manifest = test_manifest("env", "1.0.0", Vec::new());
        manifest.capabilities = CapabilitySection {
            env_read: vec!["EDEN_SESSION".into(), "EDEN_SOCKET".into()],
            ..CapabilitySection::default()
        };
        manifest.validate().expect("合法变量名应被接受");

        for (names, expected) in [
            (vec!["eden_session".into()], "大写字母"),
            (vec!["EDEN-SESSION".into()], "大写字母"),
            (
                vec!["EDEN_SESSION".into(), "EDEN_SESSION".into()],
                "重复声明",
            ),
        ] {
            manifest.capabilities.env_read = names.clone();
            let error = manifest
                .validate()
                .expect_err(&format!("{names:?} 应被拒绝"));
            assert!(
                error.to_string().contains(expected),
                "{names:?} 的实际错误：{error}"
            );
        }
    }

    /// socket 按 network 抬升工具权限下限，`env_read` 按只读数据抬升。
    #[test]
    fn socket_and_env_read_raise_tool_scope_floor() {
        let socket_only = CapabilitySection {
            socket: Some(SocketCapabilitySection {
                allow_paths: vec!["/tmp/agent.sock".into()],
            }),
            ..CapabilitySection::default()
        };
        assert_eq!(
            socket_only.component_capability_scope_floor(),
            Some(ToolPermissionScope::Network)
        );

        let env_only = CapabilitySection {
            env_read: vec!["EDEN_SESSION".into()],
            ..CapabilitySection::default()
        };
        assert_eq!(
            env_only.component_capability_scope_floor(),
            Some(ToolPermissionScope::Read)
        );
    }

    /// 空的 HTTP allowlist 必须在加载前拒绝。
    #[test]
    fn empty_http_allowlist_is_rejected() {
        let mut manifest = test_manifest("http-empty", "1.0.0", Vec::new());
        manifest.capabilities = CapabilitySection {
            http: Some(HttpCapabilitySection::default()),
            ..CapabilitySection::default()
        };
        let error = manifest
            .validate()
            .expect_err("空 allowlist 不应授予网络能力");
        assert!(
            error.to_string().contains("allow_hosts"),
            "实际错误：{error}"
        );
    }
}

/// 插件对一个事件的订阅声明。
///
/// 订阅是**投递权限边界**：未声明的事件不会送达插件，因此插件既看不到无关事件，
/// 也无法对其表态。宿主据此在装配阶段就能审计出每个插件能观察和否决哪些决策点。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EventSubscription {
    /// 订阅的事件名，例如 `core.tool.call`。
    pub name: String,

    /// 执行优先级；缺省为 `normal`。
    ///
    /// `monitor` 表示只观察最终结果，其改写与取消都不会生效。
    #[serde(default)]
    pub priority: ListenerPriority,

    /// 事件已被更早的监听者取消时是否跳过本插件；缺省为 `false`。
    #[serde(default)]
    pub ignore_cancelled: bool,
}

impl EventSubscription {
    /// 校验单条订阅声明。
    ///
    /// # Errors
    ///
    /// 事件名为空或含空白字符时返回错误。
    pub fn validate(&self) -> Result<()> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err(anyhow!("events.name 不能为空"));
        }
        if name != self.name || name.chars().any(char::is_whitespace) {
            return Err(anyhow!("events.name 不能包含空白字符：`{}`", self.name));
        }
        Ok(())
    }
}

/// plugin.toml 的结构。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    /// 插件身份、ABI 版本和 component 路径。
    pub plugin: PluginSection,

    /// 当前插件依赖的其他插件及版本约束。
    #[serde(default)]
    pub dependencies: Vec<PluginDependency>,

    /// 插件向宿主声明的通用能力。
    #[serde(default)]
    pub provides: Vec<ProvidedCapability>,

    /// 插件订阅的事件总线事件；未声明的事件不会投递给该插件。
    #[serde(default)]
    pub events: Vec<EventSubscription>,

    /// 插件请求的能力开关。
    #[serde(default)]
    pub capabilities: CapabilitySection,

    /// 自由格式插件元数据。
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

impl PluginManifest {
    /// 从 TOML 文件加载 manifest。
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = fs::read_to_string(path)
            .with_context(|| format!("failed to read plugin manifest: {}", path.display()))?;
        let manifest: Self = toml::from_str(&text)
            .with_context(|| format!("failed to parse plugin manifest: {}", path.display()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// 校验 manifest 字段和支持的 capability 集合。
    pub fn validate(&self) -> Result<()> {
        if self.plugin.id.trim().is_empty() {
            return Err(anyhow!("plugin.id cannot be empty"));
        }
        if self.plugin.name.trim().is_empty() {
            return Err(anyhow!("plugin.name cannot be empty"));
        }
        if self.plugin.version.trim().is_empty() {
            return Err(anyhow!("plugin.version cannot be empty"));
        }
        Version::parse(&self.plugin.version)
            .map_err(|error| anyhow!("plugin.version 不是有效语义化版本：{error}"))?;
        if self.plugin.api_version != SUPPORTED_PLUGIN_API_VERSION {
            return Err(anyhow!(
                "不支持插件 api_version `{}`；仅支持当前版本 `{}`",
                self.plugin.api_version,
                SUPPORTED_PLUGIN_API_VERSION,
            ));
        }
        if self.plugin.wasm.trim().is_empty() {
            return Err(anyhow!("plugin.wasm cannot be empty"));
        }
        let mut dependency_ids = HashSet::new();
        for dependency in &self.dependencies {
            dependency.validate()?;
            if dependency.id == self.plugin.id {
                return Err(anyhow!("插件不能依赖自身：`{}`", self.plugin.id));
            }
            if !dependency_ids.insert(&dependency.id) {
                return Err(anyhow!("插件依赖重复：`{}`", dependency.id));
            }
        }
        let mut provided_ids = HashSet::new();
        for provided in &self.provides {
            provided.validate()?;
            if !provided_ids.insert(&provided.id) {
                return Err(anyhow!("插件能力声明重复：`{}`", provided.id));
            }
        }
        let mut subscribed_events = HashSet::new();
        for subscription in &self.events {
            subscription.validate()?;
            // 同一事件重复订阅会让 Guest 在一次分发里被调用多次却无法区分，
            // 属于配置错误而不是可用语义，直接在加载前拒绝。
            if !subscribed_events.insert(&subscription.name) {
                return Err(anyhow!("插件事件订阅重复：`{}`", subscription.name));
            }
        }
        self.capabilities.validate_supported()?;
        Ok(())
    }
}

/// 同一能力允许的提供者基数。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ProvidedCapabilityMode {
    /// 多个插件可以同时提供该能力。
    #[default]
    Multi,
    /// 同一运行时只能选择一个提供者。
    Exclusive,
}

/// 插件向宿主公开的通用能力声明。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProvidedCapability {
    /// 稳定能力 ID。
    pub id: String,
    /// 当前插件实现的能力协议版本。
    pub version: String,
    /// 能力允许的提供者基数。
    #[serde(default)]
    pub mode: ProvidedCapabilityMode,
}

impl ProvidedCapability {
    /// 创建独占能力声明。
    pub fn exclusive(id: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            version: version.into(),
            mode: ProvidedCapabilityMode::Exclusive,
        }
    }

    /// 校验能力 ID 和版本。
    pub fn validate(&self) -> Result<()> {
        if self.id.is_empty()
            || !self
                .id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            return Err(anyhow!("插件能力 ID 无效：`{}`", self.id));
        }
        Version::parse(&self.version)
            .map_err(|error| anyhow!("插件能力 `{}` 版本无效：{error}", self.id))?;
        Ok(())
    }
}

/// Host 对插件能力声明的解析结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedPluginCapabilities {
    owners: HashMap<String, Vec<String>>,
    modes: HashMap<String, ProvidedCapabilityMode>,
}

impl ResolvedPluginCapabilities {
    /// 返回独占能力最终选择的 owner。
    pub fn exclusive_owner(&self, capability_id: &str) -> Option<&str> {
        self.owners
            .get(capability_id)
            .and_then(|owners| (owners.len() == 1).then(|| owners[0].as_str()))
    }

    /// 返回能力的全部有效 owner。
    pub fn owners(&self, capability_id: &str) -> &[String] {
        self.owners
            .get(capability_id)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// 按未指定顺序返回全部能力、提供者基数和有效 owner。
    ///
    /// 调用方生成稳定制品时必须自行排序；返回的 owner 均来自已经校验并完成冲突解析的
    /// manifest 集合。
    pub fn resolved_owners(
        &self,
    ) -> impl Iterator<Item = (&str, ProvidedCapabilityMode, &[String])> {
        self.owners.iter().map(|(capability_id, owners)| {
            (
                capability_id.as_str(),
                self.modes[capability_id],
                owners.as_slice(),
            )
        })
    }

    /// 按能力 ID 返回全部独占能力的最终 owner。
    ///
    /// 结果只包含已经完成冲突解析的独占能力；多提供者能力不会混入该快照。调用方可将
    /// 其收集到有序映射中，用于可信运行配置的一致性校验。
    pub fn exclusive_owners(&self) -> impl Iterator<Item = (&str, &str)> {
        self.modes.iter().filter_map(|(capability_id, mode)| {
            if *mode != ProvidedCapabilityMode::Exclusive {
                return None;
            }
            self.exclusive_owner(capability_id)
                .map(|owner| (capability_id.as_str(), owner))
        })
    }
}

/// 在实例化插件前解析通用能力声明和独占能力选择。
pub fn resolve_plugin_capabilities(
    manifests: &[PluginManifest],
    selections: &HashMap<String, String>,
) -> Result<ResolvedPluginCapabilities> {
    let mut declarations: HashMap<String, (ProvidedCapabilityMode, Vec<String>)> = HashMap::new();
    for manifest in manifests {
        manifest.validate()?;
        for provided in &manifest.provides {
            let entry = declarations
                .entry(provided.id.clone())
                .or_insert_with(|| (provided.mode, Vec::new()));
            if entry.0 != provided.mode {
                return Err(anyhow!("能力 `{}` 的提供者基数声明不一致", provided.id));
            }
            entry.1.push(manifest.plugin.id.clone());
        }
    }

    for capability_id in selections.keys() {
        if !declarations.contains_key(capability_id) {
            return Err(anyhow!("选择了无人提供的插件能力：`{capability_id}`"));
        }
    }

    let mut owners = HashMap::new();
    let mut modes = HashMap::new();
    for (capability_id, (mode, providers)) in declarations {
        modes.insert(capability_id.clone(), mode);
        match mode {
            ProvidedCapabilityMode::Multi => {
                if selections.contains_key(&capability_id) {
                    return Err(anyhow!(
                        "多提供者能力不允许指定唯一 owner：`{capability_id}`"
                    ));
                }
                owners.insert(capability_id, providers);
            }
            ProvidedCapabilityMode::Exclusive => {
                let selected = selections.get(&capability_id);
                let owner = match (providers.as_slice(), selected) {
                    ([only], None) => only.clone(),
                    (_, Some(selected)) if providers.contains(selected) => selected.clone(),
                    (_, Some(selected)) => {
                        return Err(anyhow!(
                            "能力 `{capability_id}` 选择了未声明该能力的插件 `{selected}`"
                        ));
                    }
                    (_, None) => {
                        return Err(anyhow!(
                            "能力 `{capability_id}` 存在多个独占提供者：{}；请在 capability_selection 中显式选择",
                            providers.join("、")
                        ));
                    }
                };
                owners.insert(capability_id, vec![owner]);
            }
        }
    }
    Ok(ResolvedPluginCapabilities { owners, modes })
}

/// manifest 声明的插件依赖。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginDependency {
    /// 被依赖插件的稳定 ID。
    pub id: String,
    /// 被依赖插件版本的 SemVer 约束，默认为任意版本。
    #[serde(default = "default_dependency_version")]
    pub version: String,
    /// 可选依赖缺失时不阻止加载；已安装但版本不匹配仍视为错误。
    #[serde(default)]
    pub optional: bool,
}

impl PluginDependency {
    /// 校验依赖标识和版本约束。
    pub fn validate(&self) -> Result<()> {
        if self.id.trim().is_empty() {
            return Err(anyhow!("插件依赖 ID 不能为空"));
        }
        VersionReq::parse(&self.version)
            .map_err(|error| anyhow!("插件 `{}` 的版本约束无效：{error}", self.id))?;
        Ok(())
    }
}

/// 返回依赖优先且保持原配置顺序的插件索引。
///
/// 缺失的必选依赖、版本不匹配、重复 ID 和循环依赖都会阻止加载。
pub fn resolve_plugin_load_order(manifests: &[PluginManifest]) -> Result<Vec<usize>> {
    let mut by_id = HashMap::new();
    for (index, manifest) in manifests.iter().enumerate() {
        manifest.validate()?;
        if by_id.insert(manifest.plugin.id.as_str(), index).is_some() {
            return Err(anyhow!("插件 ID 重复：`{}`", manifest.plugin.id));
        }
    }

    let mut outgoing = vec![Vec::new(); manifests.len()];
    let mut indegree = vec![0usize; manifests.len()];
    for (dependent_index, manifest) in manifests.iter().enumerate() {
        for dependency in &manifest.dependencies {
            let Some(&provider_index) = by_id.get(dependency.id.as_str()) else {
                if dependency.optional {
                    continue;
                }
                return Err(anyhow!(
                    "插件 `{}` 缺少必选依赖 `{}`",
                    manifest.plugin.id,
                    dependency.id
                ));
            };
            let requirement = VersionReq::parse(&dependency.version)?;
            let provider_version = Version::parse(&manifests[provider_index].plugin.version)?;
            if !requirement.matches(&provider_version) {
                return Err(anyhow!(
                    "插件 `{}` 需要 `{}` 版本 `{}`，当前为 `{}`",
                    manifest.plugin.id,
                    dependency.id,
                    dependency.version,
                    provider_version
                ));
            }
            outgoing[provider_index].push(dependent_index);
            indegree[dependent_index] += 1;
        }
    }

    let mut ready = indegree
        .iter()
        .enumerate()
        .filter_map(|(index, degree)| (*degree == 0).then_some(index))
        .collect::<BTreeSet<_>>();
    let mut order = Vec::with_capacity(manifests.len());
    while let Some(index) = ready.pop_first() {
        order.push(index);
        for &dependent in &outgoing[index] {
            indegree[dependent] -= 1;
            if indegree[dependent] == 0 {
                ready.insert(dependent);
            }
        }
    }
    if order.len() != manifests.len() {
        let cycle = indegree
            .iter()
            .enumerate()
            .filter_map(|(index, degree)| {
                (*degree > 0).then_some(manifests[index].plugin.id.as_str())
            })
            .collect::<Vec<_>>()
            .join("、");
        return Err(anyhow!("插件依赖存在循环：{cycle}"));
    }
    Ok(order)
}

/// 返回不限制依赖版本的默认约束。
fn default_dependency_version() -> String {
    "*".to_string()
}

/// 插件基础身份信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginSection {
    /// 安装、依赖和 owner 路由使用的稳定插件 ID。
    pub id: String,
    /// 面向用户展示的插件名称，不参与协议路由。
    pub name: String,
    /// 插件实现版本，必须是有效 SemVer。
    pub version: String,

    /// ascnet-lucia 插件 WIT 契约的 ABI 版本。
    #[serde(default = "default_api_version")]
    pub api_version: String,

    /// `.wasm` component 路径，相对于 plugin.toml。
    pub wasm: String,

    /// 面向用户的可选功能说明，不参与权限判定。
    pub description: Option<String>,
}

fn default_api_version() -> String {
    SUPPORTED_PLUGIN_API_VERSION.to_string()
}

/// 能力声明。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CapabilitySection {
    /// 插件请求的 Agent Runtime 短控制面权限。
    #[serde(default)]
    pub agent: AgentCapabilitySection,

    /// 插件是否可以使用应用固定路由执行无工具模型完成。
    #[serde(default)]
    pub model_completion: bool,

    /// 插件是否可以发布 `ui.host.action` 事件驱动宿主应用级动作。
    ///
    /// 仅为旧 manifest 保留；`true` 代表全部动作，新插件应使用 `surface_action_allowlist`。
    #[serde(default)]
    pub surface_actions: bool,

    /// 插件可请求的具体宿主界面动作；空集合不授予任何动作。
    #[serde(default)]
    pub surface_action_allowlist: Vec<UiHostActionKind>,

    /// 插件是否希望通过宿主 API 执行进程。
    #[serde(default)]
    pub process_exec: bool,

    /// 插件是否希望通过宿主 API 访问 HTTP，以及允许访问的目标主机。
    ///
    /// 使用可选表而不是布尔值，是为了让授权边界和具体域名同时出现在 manifest 中。
    /// 声明空 allowlist 会在加载前被拒绝，避免把「申请了网络但没有明确目的地」误当成安全
    /// 配置。旧版 `http = true` manifest 不再被接受，需迁移到 `[capabilities.http]`。
    #[serde(default)]
    pub http: Option<HttpCapabilitySection>,

    /// 插件是否希望通过宿主 API 连接 Unix domain socket，以及允许连接的具体路径。
    ///
    /// 同样是可选表而非布尔值：socket 的价值来自「连到哪一个本地服务」，把路径写进
    /// manifest 才能让用户与审计者看到授权边界。声明空 allowlist 会在加载前被拒绝。
    #[serde(default)]
    pub socket: Option<SocketCapabilitySection>,

    /// 插件请求宿主注入给 Guest 的环境变量名。
    ///
    /// WASI 默认不继承任何环境变量——继承会把 API 密钥一类的凭证直接交给插件。需要读取
    /// 运行时身份（例如编辑器注入的会话标识）的插件必须逐个列出变量名，Host 只注入这些
    /// 变量，其余仍然不可见。
    #[serde(default)]
    pub env_read: Vec<String>,

    /// 插件是否希望通过宿主 API 读取 secret。
    #[serde(default)]
    pub secrets: bool,

    /// 插件是否申请跨版本保留的私有持久存储。
    ///
    /// 与 `state` 的区别是生命周期而非权限范围：两者都只能访问本插件自己的命名空间，
    /// 但 `state` 在插件代码变化时清空，`storage` 跨版本保留。记忆、索引一类的用户
    /// 数据必须用 `storage`，否则一次插件升级就会清空用户资产。
    #[serde(default)]
    pub storage: bool,

    /// 插件是否申请一个跨版本保留的私有数据目录。
    ///
    /// 与 `storage` 的区别是**形态与归属**：`storage` 是宿主代管的键值 JSON，每次写入重写
    /// 全量并受字节上限约束，只适合 manifest 这类小元数据；数据目录则由插件自己完全掌握，
    /// 想用什么格式、分几个文件、多大体积都由插件决定，宿主只负责把它 preopen 成 `/data`
    /// 并保证除了本插件与宿主之外没有人能访问。
    ///
    /// 索引、向量库这类"体积随用户仓库增长"的数据必须放这里——把它们塞进 `storage` 会在
    /// 超过上限后被静默截断，表现为功能随仓库变大而悄悄退化。
    #[serde(default)]
    pub data_dir: bool,

    /// 插件是否申请只读访问当前项目的历史会话。
    ///
    /// 该能力的覆盖面远大于它的名字：会话内容是 Agent 历史上见过的一切的并集——
    /// 文件正文、shell 输出、网络返回，横跨全部过去的运行。持有该能力的插件因此
    /// 能读到用户很久以前、为了另一件事才批准过的数据，所以它会把插件全部公开工具
    /// 的权限下限抬到 `unknown`，并且没有任何写入路径。
    #[serde(default)]
    pub sessions: bool,

    /// 插件是否申请只读访问当前工作区根及其目录后代。
    ///
    /// 工作区根由应用通过可信 Host 服务注入，Guest 与 manifest 都不能指定实际路径。
    /// 该能力复用 `host-fs-read` 与 `host-fs-list`，用于仓库感知型插件读取当前项目。
    #[serde(default)]
    pub workspace_read: bool,

    /// 插件请求读取的路径。
    #[serde(default)]
    pub fs_read: Vec<String>,

    /// 插件请求写入的路径。
    #[serde(default)]
    pub fs_write: Vec<String>,

    /// Host 按公开工具名绑定的权威权限 scope；未声明的插件工具统一降级为 `unknown`。
    #[serde(default)]
    pub tool_scopes: HashMap<String, ToolPermissionScope>,
}

/// 插件 Unix domain socket 能力的可审计配置。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SocketCapabilitySection {
    /// 允许连接的 socket 绝对路径；`~/` 表示宿主 home 目录。
    ///
    /// 只接受精确路径，不接受通配符：socket 名通常固定，允许前缀匹配等于把同目录下任意
    /// 本地服务都授权给插件。
    #[serde(default)]
    pub allow_paths: Vec<String>,
}

impl SocketCapabilitySection {
    /// 校验 socket allowlist 的格式边界。
    ///
    /// 只做语法检查，不要求路径此刻存在：Eden 一类的服务可能尚未启动，加载期检查存在性
    /// 会让插件在服务未运行时直接加载失败，而正确行为是工具调用时返回可重试错误。
    pub fn validate(&self) -> Result<()> {
        if self.allow_paths.is_empty() {
            return Err(anyhow!("capabilities.socket.allow_paths 不能为空"));
        }
        if self.allow_paths.len() > 16 {
            return Err(anyhow!(
                "capabilities.socket.allow_paths 不能超过 16 个路径"
            ));
        }
        let mut seen = HashSet::new();
        for path in &self.allow_paths {
            let normalized = path.trim();
            if normalized != path {
                return Err(anyhow!("capabilities.socket.allow_paths 不能包含首尾空白"));
            }
            if !is_pinned_local_path(path) {
                return Err(anyhow!(
                    "capabilities.socket.allow_paths 必须以 `/` 或 `~/` 开头的绝对路径：`{path}`"
                ));
            }
            if path.ends_with('/') {
                return Err(anyhow!(
                    "capabilities.socket.allow_paths 不能以斜杠结尾：`{path}`"
                ));
            }
            if path
                .bytes()
                .any(|byte| byte < 0x20 || byte == 0x7F || byte == b'*' || byte == b'?')
            {
                return Err(anyhow!(
                    "capabilities.socket.allow_paths 含非法字符或通配符：`{path}`"
                ));
            }
            if !seen.insert(path.clone()) {
                return Err(anyhow!("capabilities.socket.allow_paths 重复声明 `{path}`"));
            }
        }
        Ok(())
    }
}

/// 判断路径是否是允许的本地绝对路径形式（`/` 或 `~/` 开头，且不含父目录穿越）。
///
/// `~` 只支持紧跟分隔符的 `~/`：把 `~user/...` 或 `~` 中间出现当作普通字符会让展开逻辑
/// 与用户的 shell 直觉不一致，从而把授权边界变成猜测。
pub(crate) fn is_pinned_local_path(path: &str) -> bool {
    if path.is_empty() || path == "~" || !(path.starts_with('/') || path.starts_with("~/")) {
        return false;
    }
    !path.split('/').any(|segment| segment == "..")
}

/// 校验插件请求注入的环境变量名列表。
///
/// 只接受 POSIX 变量名的常见子集，并拒绝重复：环境名会被拼进 WASI 环境表，宽松的字符
/// 集合没有收益，只会给不同平台的解释差异留下空间。
pub(crate) fn validate_env_read(names: &[String]) -> Result<()> {
    if names.len() > 32 {
        return Err(anyhow!("capabilities.env_read 不能超过 32 个变量"));
    }
    let mut seen = HashSet::new();
    for name in names {
        let valid = !name.is_empty()
            && name.len() <= 128
            && name
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
            && !name.as_bytes()[0].is_ascii_digit();
        if !valid {
            return Err(anyhow!(
                "capabilities.env_read 中的变量名必须是大写字母、数字或下划线：`{name}`"
            ));
        }
        if !seen.insert(name.as_str()) {
            return Err(anyhow!("capabilities.env_read 重复声明 `{name}`"));
        }
    }
    Ok(())
}

/// 插件 HTTP 能力的可审计配置。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HttpCapabilitySection {
    /// 允许访问的主机名；不含 scheme、路径或通配符。
    #[serde(default)]
    pub allow_hosts: Vec<String>,
}

impl HttpCapabilitySection {
    /// 校验 HTTP 主机 allowlist 的格式与边界。
    ///
    /// Host 会在实际请求时再次以可信 base URL 做匹配；这里只负责尽早拒绝空值、注入
    /// 分隔符和不受支持的通配符。
    pub fn validate(&self) -> Result<()> {
        if self.allow_hosts.is_empty() {
            return Err(anyhow!("capabilities.http.allow_hosts 不能为空"));
        }
        if self.allow_hosts.len() > 64 {
            return Err(anyhow!("capabilities.http.allow_hosts 不能超过 64 个主机"));
        }
        let mut seen = HashSet::new();
        for host in &self.allow_hosts {
            let normalized = host.trim().to_ascii_lowercase();
            if normalized.is_empty() {
                return Err(anyhow!("capabilities.http.allow_hosts 不能包含空主机"));
            }
            if normalized.len() > 253
                || normalized.contains("://")
                || normalized.contains('/')
                || normalized.contains('?')
                || normalized.contains('#')
                || normalized.contains('\\')
                || normalized.contains('*')
                || normalized
                    .bytes()
                    .any(|byte| byte.is_ascii_whitespace() || byte < 0x20)
            {
                return Err(anyhow!(
                    "HTTP allowlist 主机格式无效：`{host}`（只允许主机名和可选端口）"
                ));
            }
            if !valid_http_host(&normalized) {
                return Err(anyhow!("HTTP allowlist 主机格式无效：`{host}`"));
            }
            if !seen.insert(normalized) {
                return Err(anyhow!("HTTP allowlist 主机重复：`{host}`"));
            }
        }
        Ok(())
    }

    /// 判断目标主机是否命中 allowlist；未写端口的条目允许同主机的显式端口。
    pub(crate) fn allows_host(&self, host: &str, port: Option<u16>) -> bool {
        let host = host.trim().to_ascii_lowercase();
        self.allow_hosts.iter().any(|allowed| {
            let allowed = allowed.trim().to_ascii_lowercase();
            let (allowed_host, allowed_port) = split_host_port(&allowed);
            allowed_host == host && (allowed_port.is_none() || allowed_port == port)
        })
    }
}

/// 校验不带 scheme 与路径的主机名，允许 IPv4、IPv6（方括号形式）和 DNS 名称。
fn valid_http_host(host: &str) -> bool {
    let (name, port) = split_host_port(host);
    if let Some(port) = port {
        if port == 0 {
            return false;
        }
    }
    if name.starts_with('[') || name.ends_with(']') {
        return name.starts_with('[')
            && name.ends_with(']')
            && name.len() > 2
            && name[1..name.len() - 1].parse::<std::net::IpAddr>().is_ok();
    }
    if name.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    let labels = name.split('.').collect::<Vec<_>>();
    !labels.is_empty()
        && labels.iter().all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

/// 分离主机和可选端口；IPv6 仅接受 `[addr]:port` 形式。
fn split_host_port(host: &str) -> (String, Option<u16>) {
    if let Some(stripped) = host.strip_prefix('[') {
        if let Some(end) = stripped.find(']') {
            let name = format!("[{}]", &stripped[..end]);
            let port = stripped
                .get(end + 1..)
                .and_then(|suffix| suffix.strip_prefix(':'))
                .and_then(|value| value.parse::<u16>().ok());
            return (name, port);
        }
    }
    if let Some((name, port)) = host.rsplit_once(':') {
        if !name.contains(':') {
            return (name.to_string(), port.parse::<u16>().ok());
        }
    }
    (host.to_string(), None)
}

/// 插件访问 Agent Runtime 的最小权限集合。
///
/// 权限默认全部关闭；`profiles` 只是 manifest 请求范围，最终还必须与应用注册的
/// spawn profile 取交集。Guest 永远不能直接提交模型、provider options 或工具权限。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentCapabilitySection {
    /// 允许读取当前 controller 身份并启动派生 Agent。
    #[serde(default)]
    pub spawn: bool,
    /// 允许查询 controller 及其后代的状态和终态结果。
    #[serde(default)]
    pub observe: bool,
    /// 允许级联取消 controller 的后代任务。
    #[serde(default)]
    pub cancel: bool,
    /// 允许回收已终态后代任务的 Runtime 资源并归还累计名额。
    ///
    /// 该能力会释放目标的私有会话与终态结果，因此回收后不能续跑，也不能再读取结果。
    #[serde(default)]
    pub collect: bool,
    /// 允许 Guest 在 spawn 请求中引用的派生策略名称。
    #[serde(default)]
    pub profiles: Vec<String>,
}

impl AgentCapabilitySection {
    /// 判断插件是否请求了任何 Agent Runtime 权限。
    pub fn is_requested(&self) -> bool {
        self.spawn || self.observe || self.cancel || self.collect || !self.profiles.is_empty()
    }

    /// 判断 manifest 是否允许指定 spawn profile。
    pub fn allows_profile(&self, profile: &str) -> bool {
        self.profiles.iter().any(|allowed| allowed == profile)
    }

    /// 校验权限组合和 profile 标识。
    pub fn validate(&self) -> Result<()> {
        if self.spawn && self.profiles.is_empty() {
            return Err(anyhow!(
                "声明 capabilities.agent.spawn 时必须配置至少一个 profiles 条目"
            ));
        }
        if !self.spawn && !self.profiles.is_empty() {
            return Err(anyhow!(
                "capabilities.agent.profiles 只能与 spawn = true 一起使用"
            ));
        }
        let mut profiles = HashSet::new();
        for profile in &self.profiles {
            let valid = !profile.is_empty()
                && profile.len() <= 128
                && profile
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
            if !valid {
                return Err(anyhow!("Agent spawn profile 标识无效：`{profile}`"));
            }
            if !profiles.insert(profile) {
                return Err(anyhow!("Agent spawn profile 声明重复：`{profile}`"));
            }
        }
        Ok(())
    }
}

impl CapabilitySection {
    /// 判断 manifest 是否允许具体宿主界面动作。
    pub(crate) fn allows_surface_action(&self, action: UiHostActionKind) -> bool {
        self.surface_actions || self.surface_action_allowlist.contains(&action)
    }

    /// 把 manifest 中用户可审计的 scope 绑定到 Guest 工具定义。
    ///
    /// Guest 未声明 scope 时由 Host 补齐；Guest 自报值与 manifest 不一致时拒绝加载，避免
    /// 运行时代码把高风险工具伪装成 `control`。manifest 未列出的工具保守降级为 `unknown`。
    /// Host 还会按整个 component 已授予的能力抬高 scope；WASM 无法证明某个工具不会在
    /// 内部调用进程、文件或 Agent Host API，因此不允许高权限插件仅靠自报 `control`
    /// 跳过逐调用审批。
    pub(crate) fn bind_tool_scope(&self, spec: &mut ToolSpec) -> Result<()> {
        let guest_scope = spec.permission_scope;
        let Some(manifest_scope) = self.tool_scopes.get(&spec.name).copied() else {
            spec.permission_scope = ToolPermissionScope::Unknown;
            return Ok(());
        };
        if guest_scope != ToolPermissionScope::Unknown && guest_scope != manifest_scope {
            return Err(anyhow!(
                "插件工具 `{}` 自报 scope `{}` 与 manifest 授权 `{}` 不一致",
                spec.name,
                guest_scope.as_str(),
                manifest_scope.as_str()
            ));
        }
        spec.permission_scope =
            merge_tool_permission_scopes(manifest_scope, self.component_capability_scope_floor());
        Ok(())
    }

    /// 把 component 任意代码可调用的 Host 能力折算为所有公开工具的保守 scope 下限。
    fn component_capability_scope_floor(&self) -> Option<ToolPermissionScope> {
        let mut scope = None;
        let mut include = |required| {
            scope = Some(match scope {
                None => required,
                Some(current) => merge_tool_permission_scopes(current, Some(required)),
            });
        };
        if self.process_exec {
            include(ToolPermissionScope::Shell);
        }
        if self.workspace_read || !self.fs_read.is_empty() {
            include(ToolPermissionScope::Read);
        }
        if !self.fs_write.is_empty() {
            include(ToolPermissionScope::Write);
        }
        if self.http.is_some() {
            include(ToolPermissionScope::Network);
        }
        if self.socket.is_some() {
            // socket 是出站连接：它能访问的本机服务不亚于一条网络连接，按 network 计入下限。
            include(ToolPermissionScope::Network);
        }
        if !self.env_read.is_empty() {
            // 环境变量是受 manifest 逐项声明的只读数据，与 `fs_read` 同档；`secrets` 那种
            // 无 allowlist 的通用读取才升到 unknown。
            include(ToolPermissionScope::Read);
        }
        if self.sessions
            || self.secrets
            || self.model_completion
            || self.agent.is_requested()
            || self.surface_actions
            || !self.surface_action_allowlist.is_empty()
        {
            include(ToolPermissionScope::Unknown);
        }
        scope
    }

    /// 校验能力声明与当前 Host 支持范围。
    ///
    /// 只读文件、HTTP 和子进程能力由宿主 API 在运行时逐次鉴权；secret 与文件写入仍在
    /// 加载阶段拒绝，不会静默授予。HTTP 必须同时提供非空 allowlist。
    pub fn validate_supported(&self) -> Result<()> {
        self.agent.validate()?;
        if self.surface_actions && !self.surface_action_allowlist.is_empty() {
            return Err(anyhow!(
                "surface_actions = true 与 surface_action_allowlist 不能同时声明"
            ));
        }
        let mut surface_actions = HashSet::new();
        for action in &self.surface_action_allowlist {
            if !surface_actions.insert(*action) {
                return Err(anyhow!(
                    "surface_action_allowlist 重复声明 `{}`",
                    action.as_str()
                ));
            }
        }
        for name in self.tool_scopes.keys() {
            ToolSpec::new(
                name,
                "manifest scope validation",
                ToolSpec::empty_object_schema(),
            )
            .validate_name()
            .map_err(|error| anyhow!("manifest 工具 scope 名称 `{name}` 无效：{error}"))?;
        }
        if let Some(http) = &self.http {
            http.validate()?;
        }
        if let Some(socket) = &self.socket {
            socket.validate()?;
        }
        validate_env_read(&self.env_read)?;
        if self.secrets || !self.fs_write.is_empty() {
            return Err(anyhow!("当前 Lucia 版本尚未实现插件 secret 或文件写入能力"));
        }
        Ok(())
    }
}

/// 合并工具自身 scope 与 component 能力下限；不同资源类型必须降级为 `unknown`。
fn merge_tool_permission_scopes(
    declared: ToolPermissionScope,
    required: Option<ToolPermissionScope>,
) -> ToolPermissionScope {
    let Some(required) = required else {
        return declared;
    };
    match (declared, required) {
        (ToolPermissionScope::Control, required) => required,
        (declared, ToolPermissionScope::Control) => declared,
        (declared, required) if declared == required => declared,
        _ => ToolPermissionScope::Unknown,
    }
}

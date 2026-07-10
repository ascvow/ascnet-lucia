//! 插件 manifest 支持。

use anyhow::{anyhow, Context, Result};
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

/// 当前支持的插件 ABI 版本。
pub const SUPPORTED_PLUGIN_API_VERSION: &str = "0.6.0";

const LEGACY_PLUGIN_API_VERSIONS: [&str; 5] = ["0.1.0", "0.2.0", "0.3.0", "0.4.0", "0.5.0"];

/// 上下文加载器使用的标准能力 ID。
pub const CONTEXT_LOADER_CAPABILITY: &str = "agent.context-loader";

/// 应用配置文件中的插件列表部分。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PluginListConfig {
    /// 按加载顺序排列的插件 manifest 条目。
    #[serde(default)]
    plugins: Vec<PluginConfigEntry>,

    /// 独占能力 ID 到插件 ID 的显式选择。
    #[serde(default)]
    capability_selection: HashMap<String, String>,
}

/// 应用配置文件中的单个插件 manifest 条目。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PluginConfigEntry {
    /// 相对于应用配置文件目录的 manifest 路径。
    manifest: String,
}

/// 从应用 TOML 配置中读取插件路径，并相对配置文件目录完成解析。
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
}

/// 从应用 TOML 配置中读取插件路径和独占能力选择。
pub fn load_plugin_runtime_config(config_path: impl AsRef<Path>) -> Result<PluginRuntimeConfig> {
    let config_path = config_path.as_ref();
    let text = fs::read_to_string(config_path)
        .with_context(|| format!("failed to read config: {}", config_path.display()))?;
    parse_plugin_runtime_config(&text, config_path)
}

/// 解析应用配置文本中的插件运行时配置，供文件加载入口和单元测试复用。
fn parse_plugin_runtime_config(text: &str, config_path: &Path) -> Result<PluginRuntimeConfig> {
    let config: PluginListConfig = toml::from_str(text)
        .with_context(|| format!("failed to parse config: {}", config_path.display()))?;
    let config_dir = config_path.parent().unwrap_or_else(|| Path::new("."));
    let manifest_paths = config
        .plugins
        .into_iter()
        .map(|plugin| {
            let path = PathBuf::from(plugin.manifest);
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 验证插件宿主只读取插件字段，并按应用配置目录解析相对路径。
    #[test]
    fn plugin_paths_are_loaded_independently_from_core_config() {
        let config = r#"
[model]
provider = "open-ai"
model = "test"

[[plugins]]
manifest = "plugins/demo/plugin.toml"

[[plugins]]
manifest = "/opt/lucia/plugin.toml"
"#;

        let paths = parse_plugin_runtime_config(config, Path::new("/tmp/lucia/config.toml"))
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
    }

    /// 应用配置应同时解析插件路径和独占能力选择。
    #[test]
    fn plugin_runtime_config_includes_capability_selection() {
        let config = parse_plugin_runtime_config(
            r#"
                [[plugins]]
                manifest = "plugins/context/plugin.toml"

                [capability_selection]
                "agent.context-loader" = "context-summary"
            "#,
            Path::new("/tmp/lucia/config.toml"),
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
                api_version = "0.6.0"
                wasm = "workflow.wasm"

                [capabilities.agent]
                spawn = true
                message = true
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

    /// 旧 ABI 可以继续加载普通插件，但不能声明 0.6 新增的 Agent Runtime import。
    #[test]
    fn legacy_api_cannot_request_agent_runtime() {
        let mut manifest = test_manifest("legacy", "1.0.0", Vec::new());
        manifest.plugin.api_version = "0.5.0".into();
        manifest.capabilities.agent = AgentCapabilitySection {
            observe: true,
            ..AgentCapabilitySection::default()
        };

        let error = manifest
            .validate()
            .expect_err("旧 ABI 声明 Agent Runtime 能力必须失败");
        assert!(error.to_string().contains("需要 api_version `0.6.0`"));
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
}

/// plugin.toml 的结构。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub plugin: PluginSection,

    /// 当前插件依赖的其他插件及版本约束。
    #[serde(default)]
    pub dependencies: Vec<PluginDependency>,

    /// 插件向宿主声明的通用能力。
    #[serde(default)]
    pub provides: Vec<ProvidedCapability>,

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
        if self.plugin.api_version != SUPPORTED_PLUGIN_API_VERSION
            && !LEGACY_PLUGIN_API_VERSIONS.contains(&self.plugin.api_version.as_str())
        {
            return Err(anyhow!(
                "不支持插件 api_version `{}`；当前版本 `{}`，兼容版本 `{}`",
                self.plugin.api_version,
                SUPPORTED_PLUGIN_API_VERSION,
                LEGACY_PLUGIN_API_VERSIONS.join("、"),
            ));
        }
        if self.capabilities.agent.is_requested()
            && self.plugin.api_version != SUPPORTED_PLUGIN_API_VERSION
        {
            return Err(anyhow!(
                "插件 Agent Runtime 能力需要 api_version `{SUPPORTED_PLUGIN_API_VERSION}`"
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
        self.capabilities.validate_supported()?;
        Ok(())
    }
}

/// 同一能力允许的提供者基数。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
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
    for (capability_id, (mode, providers)) in declarations {
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
    Ok(ResolvedPluginCapabilities { owners })
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
    pub id: String,
    pub name: String,
    pub version: String,

    /// ascnet-lucia 插件 WIT 契约的 ABI 版本。
    #[serde(default = "default_api_version")]
    pub api_version: String,

    /// `.wasm` component 路径，相对于 plugin.toml。
    pub wasm: String,

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

    /// 插件是否希望通过宿主 API 执行进程。
    #[serde(default)]
    pub process_exec: bool,

    /// 插件是否希望通过宿主 API 访问 HTTP。
    #[serde(default)]
    pub http: bool,

    /// 插件是否希望通过宿主 API 读取 secret。
    #[serde(default)]
    pub secrets: bool,

    /// 插件请求读取的路径。
    #[serde(default)]
    pub fs_read: Vec<String>,

    /// 插件请求写入的路径。
    #[serde(default)]
    pub fs_write: Vec<String>,
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
    /// 允许以 controller 身份发送和非阻塞接收消息。
    #[serde(default)]
    pub message: bool,
    /// 允许查询 controller 及其后代的状态和终态结果。
    #[serde(default)]
    pub observe: bool,
    /// 允许级联取消 controller 的后代任务。
    #[serde(default)]
    pub cancel: bool,
    /// 允许 Guest 在 spawn 请求中引用的派生策略名称。
    #[serde(default)]
    pub profiles: Vec<String>,
}

impl AgentCapabilitySection {
    /// 判断插件是否请求了任何 Agent Runtime 权限。
    pub fn is_requested(&self) -> bool {
        self.spawn || self.message || self.observe || self.cancel || !self.profiles.is_empty()
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
    /// 校验当前宿主尚未实现的能力。
    ///
    /// 只读文件和子进程能力由宿主 API 在运行时逐次鉴权。尚未实现的 HTTP、secret
    /// 和文件写入能力在加载阶段直接拒绝，不会静默授予。
    pub fn validate_supported(&self) -> Result<()> {
        self.agent.validate()?;
        if self.http || self.secrets || !self.fs_write.is_empty() {
            return Err(anyhow!(
                "当前 Lucia 版本尚未实现插件 HTTP、secret 或文件写入能力"
            ));
        }
        Ok(())
    }
}

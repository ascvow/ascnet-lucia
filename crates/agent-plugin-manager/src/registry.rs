//! Lucia 插件 Registry 索引、语义化版本求解和安装编排。

use crate::{
    github::download_named_release_asset, GithubInstallOptions, GithubPluginSource,
    InstalledPlugin, PluginManager,
};
use agent_plugin_host::manifest::SUPPORTED_PLUGIN_API_VERSION;
use anyhow::{anyhow, bail, Context, Result};
use semver::{Version, VersionReq};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

const REGISTRY_SCHEMA_VERSION: u32 = 1;
const DEFAULT_REGISTRY_OWNER: &str = "ascvow";
const DEFAULT_REGISTRY_REPOSITORY: &str = "lucia-plugins";
const DEFAULT_REGISTRY_ASSET: &str = "registry.json";
const DEFAULT_REGISTRY_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// 用户输入的 Registry 插件请求，例如 `context` 或 `context@^0.1`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryRequest {
    /// Registry 中的稳定包名。
    pub name: String,
    /// 允许安装的语义化版本范围。
    pub requirement: VersionReq,
}

impl RegistryRequest {
    /// 解析 npm 风格的未作用域包请求。
    ///
    /// 未指定版本时使用 `*`；包名或版本范围非法时返回错误。
    pub fn parse(input: &str) -> Result<Self> {
        let input = input.trim();
        if input.is_empty() {
            bail!("插件名称不能为空");
        }
        let (name, requirement) = match input.rsplit_once('@') {
            Some((name, requirement)) if !name.is_empty() => (name, requirement),
            _ => (input, "*"),
        };
        validate_package_name(name)?;
        let requirement = VersionReq::parse(requirement)
            .with_context(|| format!("插件 `{name}` 的版本范围无效"))?;
        Ok(Self {
            name: name.to_string(),
            requirement,
        })
    }
}

/// Registry 安装结果，按实际安装顺序保存新增插件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryInstallResult {
    /// 请求的根插件名称。
    pub requested: String,
    /// 本次新增的插件记录；依赖位于根插件之前。
    pub installed: Vec<InstalledPlugin>,
    /// 根插件已安装且满足请求时为 `true`。
    pub already_satisfied: bool,
}

/// Registry 搜索结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistrySearchResult {
    /// 插件稳定名称。
    pub name: String,
    /// 面向用户的功能说明。
    pub description: String,
    /// 当前 Lucia ABI 可安装的最新版本。
    pub latest_version: String,
    /// 发布者标识。
    pub publisher: String,
    /// 是否由 Lucia 官方维护。
    pub official: bool,
}

/// 已安装插件的可更新状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryOutdatedPlugin {
    /// 插件稳定名称。
    pub name: String,
    /// 当前已安装版本。
    pub current_version: String,
    /// 当前 Lucia ABI 可安装的最新版本。
    pub latest_version: String,
}

/// Registry 更新结果；每个插件都通过独立原子事务替换。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryUpdateResult {
    /// 实际更新成功的插件记录，依赖位于使用者之前。
    pub updated: Vec<InstalledPlugin>,
}

/// Registry 顶层索引；索引仅描述可安装资产，不承载 Host 运行时状态。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct PluginRegistry {
    schema_version: u32,
    packages: BTreeMap<String, RegistryPackage>,
}

/// 单个插件的展示元数据和独立版本历史。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryPackage {
    description: String,
    publisher: String,
    #[serde(default)]
    official: bool,
    versions: Vec<RegistryVersion>,
}

/// 单个可安装版本及其不可变 GitHub Release 资产。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryVersion {
    #[serde(deserialize_with = "deserialize_version")]
    version: Version,
    api_version: String,
    github: RegistryGithubAsset,
    sha256: String,
    #[serde(default)]
    dependencies: Vec<RegistryDependency>,
}

/// Registry 对 GitHub Release 资产的精确引用。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryGithubAsset {
    owner: String,
    repository: String,
    tag: String,
    asset: String,
}

/// 参与依赖求解的插件版本约束。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryDependency {
    name: String,
    #[serde(deserialize_with = "deserialize_version_requirement")]
    requirement: VersionReq,
    #[serde(default)]
    optional: bool,
}

/// 求解过程中可回溯的选择和约束集合。
#[derive(Debug, Clone, Default)]
struct SolverState {
    constraints: BTreeMap<String, Vec<VersionReq>>,
    selected: BTreeMap<String, RegistryVersion>,
    satisfied_installed: BTreeSet<String>,
}

impl PluginRegistry {
    /// 从默认官方 GitHub Release 读取并验证 Registry 索引。
    async fn fetch_default() -> Result<Self> {
        let source = GithubPluginSource::parse(&format!(
            "{DEFAULT_REGISTRY_OWNER}/{DEFAULT_REGISTRY_REPOSITORY}"
        ))?;
        let (_, bytes) = download_named_release_asset(
            &source,
            None,
            DEFAULT_REGISTRY_ASSET,
            DEFAULT_REGISTRY_MAX_BYTES,
        )
        .await?;
        Self::from_slice(&bytes)
    }

    /// 解析索引并验证 schema、包名、版本唯一性和资产摘要。
    fn from_slice(bytes: &[u8]) -> Result<Self> {
        let registry: Self = serde_json::from_slice(bytes).context("Registry 索引不是有效 JSON")?;
        if registry.schema_version != REGISTRY_SCHEMA_VERSION {
            bail!(
                "不支持 Registry schema_version `{}`；当前仅支持 `{REGISTRY_SCHEMA_VERSION}`",
                registry.schema_version
            );
        }
        for (name, package) in &registry.packages {
            validate_package_name(name)?;
            if package.versions.is_empty() {
                bail!("Registry 插件 `{name}` 没有可安装版本");
            }
            let mut versions = BTreeSet::new();
            for version in &package.versions {
                if !versions.insert(version.version.clone()) {
                    bail!("Registry 插件 `{name}` 重复声明版本 `{}`", version.version);
                }
                validate_sha256(&version.sha256).with_context(|| {
                    format!("Registry 插件 `{name}@{}` 摘要无效", version.version)
                })?;
                validate_package_name(&version.github.owner)?;
                validate_package_name(&version.github.repository)?;
                if version.github.tag.trim().is_empty() || version.github.asset.trim().is_empty() {
                    bail!(
                        "Registry 插件 `{name}@{}` 的 GitHub 资产引用不完整",
                        version.version
                    );
                }
                for dependency in &version.dependencies {
                    validate_package_name(&dependency.name)?;
                    if dependency.name == *name {
                        bail!("Registry 插件 `{name}@{}` 不能依赖自身", version.version);
                    }
                }
            }
        }
        Ok(registry)
    }

    /// 搜索名称或说明，并只返回当前 ABI 可安装的版本。
    fn search(&self, query: &str) -> Vec<RegistrySearchResult> {
        let query = query.trim().to_ascii_lowercase();
        self.packages
            .iter()
            .filter(|(name, package)| {
                query.is_empty()
                    || name.to_ascii_lowercase().contains(&query)
                    || package.description.to_ascii_lowercase().contains(&query)
            })
            .filter_map(|(name, package)| {
                latest_supported(package).map(|version| RegistrySearchResult {
                    name: name.clone(),
                    description: package.description.clone(),
                    latest_version: version.version.to_string(),
                    publisher: package.publisher.clone(),
                    official: package.official,
                })
            })
            .collect()
    }

    /// 求解根请求及必需依赖，并按依赖优先顺序生成安装计划。
    fn resolve(
        &self,
        request: &RegistryRequest,
        installed: &[InstalledPlugin],
    ) -> Result<Vec<(String, RegistryVersion)>> {
        let installed = installed
            .iter()
            .map(|plugin| {
                Version::parse(&plugin.version)
                    .map(|version| (plugin.id.clone(), version))
                    .with_context(|| format!("已安装插件 `{}` 的版本无效", plugin.id))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        let mut state = SolverState::default();
        state
            .constraints
            .insert(request.name.clone(), vec![request.requirement.clone()]);
        let solved = self
            .solve(state, &installed)?
            .ok_or_else(|| anyhow!("无法为 `{}` 求解兼容的插件依赖版本", request.name))?;
        let mut ordered = Vec::new();
        let mut visiting = BTreeSet::new();
        let mut visited = BTreeSet::new();
        self.visit_install_order(
            &request.name,
            &solved.selected,
            &installed,
            &mut visiting,
            &mut visited,
            &mut ordered,
        )?;
        Ok(ordered)
    }

    /// 递归尝试候选版本；每个分支都保留完整约束，冲突时回溯到上一个选择。
    fn solve(
        &self,
        state: SolverState,
        installed: &BTreeMap<String, Version>,
    ) -> Result<Option<SolverState>> {
        for name in &state.satisfied_installed {
            let version = installed
                .get(name)
                .ok_or_else(|| anyhow!("依赖求解状态缺少已安装插件 `{name}`"))?;
            if !matches_all(version, state.constraints.get(name)) {
                return Ok(None);
            }
        }
        for (name, version) in &state.selected {
            if !matches_all(&version.version, state.constraints.get(name)) {
                return Ok(None);
            }
        }

        let Some(name) = state.constraints.keys().find(|name| {
            !state.selected.contains_key(*name) && !state.satisfied_installed.contains(*name)
        }) else {
            return Ok(Some(state));
        };
        let name = name.clone();
        if let Some(version) = installed.get(&name) {
            if !matches_all(version, state.constraints.get(&name)) {
                return Ok(None);
            }
            let mut next = state;
            next.satisfied_installed.insert(name);
            return self.solve(next, installed);
        }

        let package = self
            .packages
            .get(&name)
            .ok_or_else(|| anyhow!("Registry 不包含依赖插件 `{name}`"))?;
        let mut candidates = package
            .versions
            .iter()
            .filter(|version| {
                version.api_version == SUPPORTED_PLUGIN_API_VERSION
                    && matches_all(&version.version, state.constraints.get(&name))
            })
            .cloned()
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| right.version.cmp(&left.version));
        for candidate in candidates {
            let mut next = state.clone();
            next.selected.insert(name.clone(), candidate.clone());
            for dependency in candidate
                .dependencies
                .iter()
                .filter(|dependency| !dependency.optional)
            {
                next.constraints
                    .entry(dependency.name.clone())
                    .or_default()
                    .push(dependency.requirement.clone());
            }
            if let Some(solution) = self.solve(next, installed)? {
                return Ok(Some(solution));
            }
        }
        Ok(None)
    }

    /// 对已求解版本执行依赖优先遍历，并拒绝 Registry 中的必需依赖环。
    #[allow(clippy::too_many_arguments)]
    fn visit_install_order(
        &self,
        name: &str,
        selected: &BTreeMap<String, RegistryVersion>,
        installed: &BTreeMap<String, Version>,
        visiting: &mut BTreeSet<String>,
        visited: &mut BTreeSet<String>,
        ordered: &mut Vec<(String, RegistryVersion)>,
    ) -> Result<()> {
        if installed.contains_key(name) || visited.contains(name) {
            return Ok(());
        }
        if !visiting.insert(name.to_string()) {
            bail!("Registry 必需依赖存在环：`{name}`");
        }
        let version = selected
            .get(name)
            .ok_or_else(|| anyhow!("依赖求解结果缺少插件 `{name}`"))?;
        for dependency in version
            .dependencies
            .iter()
            .filter(|dependency| !dependency.optional)
        {
            self.visit_install_order(
                &dependency.name,
                selected,
                installed,
                visiting,
                visited,
                ordered,
            )?;
        }
        visiting.remove(name);
        visited.insert(name.to_string());
        ordered.push((name.to_string(), version.clone()));
        Ok(())
    }
}

impl PluginManager {
    /// 从默认官方 Registry 求解依赖并安装插件。
    ///
    /// 已安装版本作为固定约束复用；冲突时不会自动替换。中途失败会按逆序移除本次新增项，
    /// 不触碰安装前已有插件。根插件遵循 `enabled`，必需依赖始终启用。
    pub async fn install_registry(
        &self,
        request: &RegistryRequest,
        enabled: bool,
    ) -> Result<RegistryInstallResult> {
        let registry = PluginRegistry::fetch_default().await?;
        let before = self.list()?;
        let plan = registry.resolve(request, &before)?;
        let already_satisfied = plan.is_empty()
            && before.iter().any(|plugin| {
                plugin.id == request.name
                    && Version::parse(&plugin.version)
                        .is_ok_and(|version| request.requirement.matches(&version))
            });
        let mut installed = Vec::new();
        for (name, version) in plan {
            let source = GithubPluginSource::parse(&format!(
                "{}/{}",
                version.github.owner, version.github.repository
            ))?;
            let result = self
                .install_github(
                    &source,
                    GithubInstallOptions {
                        enabled: name != request.name || enabled,
                        tag: Some(version.github.tag.clone()),
                        asset: Some(version.github.asset.clone()),
                        expected_sha256: Some(version.sha256.clone()),
                        ..GithubInstallOptions::default()
                    },
                )
                .await;
            match result {
                Ok(result) => installed.push(result.plugin),
                Err(error) => {
                    for plugin in installed.iter().rev() {
                        let _ = self.remove(&plugin.id);
                    }
                    return Err(error).with_context(|| format!("安装 Registry 插件 `{name}` 失败"));
                }
            }
        }
        Ok(RegistryInstallResult {
            requested: request.name.clone(),
            installed,
            already_satisfied,
        })
    }

    /// 从默认官方 Registry 搜索可用于当前 ABI 的插件。
    pub async fn search_registry(&self, query: &str) -> Result<Vec<RegistrySearchResult>> {
        Ok(PluginRegistry::fetch_default().await?.search(query))
    }

    /// 比较已安装版本与默认官方 Registry 中当前 ABI 的最新版本。
    pub async fn outdated_registry(&self) -> Result<Vec<RegistryOutdatedPlugin>> {
        let registry = PluginRegistry::fetch_default().await?;
        let mut outdated = Vec::new();
        for plugin in self.list()? {
            let Some(package) = registry.packages.get(&plugin.id) else {
                continue;
            };
            let Some(latest) = latest_supported(package) else {
                continue;
            };
            let current = Version::parse(&plugin.version)
                .with_context(|| format!("已安装插件 `{}` 的版本无效", plugin.id))?;
            if latest.version > current {
                outdated.push(RegistryOutdatedPlugin {
                    name: plugin.id,
                    current_version: current.to_string(),
                    latest_version: latest.version.to_string(),
                });
            }
        }
        Ok(outdated)
    }

    /// 将一个或全部已安装 Registry 插件更新到当前 ABI 的最新版本。
    ///
    /// 每个 bundle 在替换旧锁记录前完成下载、SHA-256、manifest、依赖和能力校验；批量更新按
    /// 依赖优先顺序执行，某项失败时停止，已完成的原子更新保留。
    pub async fn update_registry(&self, name: Option<&str>) -> Result<RegistryUpdateResult> {
        let registry = PluginRegistry::fetch_default().await?;
        let installed = self.list()?;
        if let Some(name) = name {
            validate_package_name(name)?;
            if !installed.iter().any(|plugin| plugin.id == name) {
                bail!("插件 `{name}` 尚未安装");
            }
        }
        let mut candidates = BTreeMap::new();
        for plugin in installed {
            if name.is_some_and(|name| plugin.id != name) {
                continue;
            }
            let Some(package) = registry.packages.get(&plugin.id) else {
                if name.is_some() {
                    bail!("Registry 不包含插件 `{}`", plugin.id);
                }
                continue;
            };
            let Some(latest) = latest_supported(package) else {
                continue;
            };
            let current = Version::parse(&plugin.version)
                .with_context(|| format!("已安装插件 `{}` 的版本无效", plugin.id))?;
            if latest.version > current {
                candidates.insert(plugin.id.clone(), (plugin, latest.clone()));
            }
        }

        let mut order = Vec::new();
        let mut visiting = BTreeSet::new();
        let mut visited = BTreeSet::new();
        for plugin_name in candidates.keys() {
            visit_update_order(
                plugin_name,
                &candidates,
                &mut visiting,
                &mut visited,
                &mut order,
            )?;
        }

        let mut updated = Vec::new();
        for plugin_name in order {
            let (current, version) = candidates
                .get(&plugin_name)
                .ok_or_else(|| anyhow!("更新计划缺少插件 `{plugin_name}`"))?;
            let source = GithubPluginSource::parse(&format!(
                "{}/{}",
                version.github.owner, version.github.repository
            ))?;
            let result = self
                .update_github(
                    &source,
                    GithubInstallOptions {
                        enabled: current.enabled,
                        tag: Some(version.github.tag.clone()),
                        asset: Some(version.github.asset.clone()),
                        expected_sha256: Some(version.sha256.clone()),
                        ..GithubInstallOptions::default()
                    },
                )
                .await
                .with_context(|| format!("更新 Registry 插件 `{plugin_name}` 失败"))?;
            updated.push(result.plugin);
        }
        Ok(RegistryUpdateResult { updated })
    }
}

/// 对待更新插件执行依赖优先遍历；未进入更新集合的依赖由最终 manifest 校验负责。
fn visit_update_order(
    name: &str,
    candidates: &BTreeMap<String, (InstalledPlugin, RegistryVersion)>,
    visiting: &mut BTreeSet<String>,
    visited: &mut BTreeSet<String>,
    order: &mut Vec<String>,
) -> Result<()> {
    if visited.contains(name) {
        return Ok(());
    }
    if !visiting.insert(name.to_string()) {
        bail!("Registry 更新依赖存在环：`{name}`");
    }
    let (_, version) = candidates
        .get(name)
        .ok_or_else(|| anyhow!("更新计划缺少插件 `{name}`"))?;
    for dependency in version
        .dependencies
        .iter()
        .filter(|dependency| !dependency.optional && candidates.contains_key(&dependency.name))
    {
        visit_update_order(&dependency.name, candidates, visiting, visited, order)?;
    }
    visiting.remove(name);
    visited.insert(name.to_string());
    order.push(name.to_string());
    Ok(())
}

/// 返回当前 ABI 可安装的最高稳定或预发布版本。
fn latest_supported(package: &RegistryPackage) -> Option<&RegistryVersion> {
    package
        .versions
        .iter()
        .filter(|version| version.api_version == SUPPORTED_PLUGIN_API_VERSION)
        .max_by(|left, right| left.version.cmp(&right.version))
}

/// 检查版本是否同时满足当前包的全部约束。
fn matches_all(version: &Version, requirements: Option<&Vec<VersionReq>>) -> bool {
    requirements.is_some_and(|requirements| {
        requirements
            .iter()
            .all(|requirement| requirement.matches(version))
    })
}

/// 限制 Registry 名称为跨平台安全的未作用域标识。
fn validate_package_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        || !name
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
    {
        bail!("Registry 名称 `{name}` 无效");
    }
    Ok(())
}

/// 检查索引摘要为完整十六进制 SHA-256。
fn validate_sha256(value: &str) -> Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("SHA-256 必须是 64 位十六进制字符串");
    }
    Ok(())
}

/// 将 Registry 字符串字段解析为语义化版本，不要求工作区全局启用 semver serde 特性。
fn deserialize_version<'de, D>(deserializer: D) -> std::result::Result<Version, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    Version::parse(&value).map_err(serde::de::Error::custom)
}

/// 将 Registry 字符串字段解析为语义化版本范围。
fn deserialize_version_requirement<'de, D>(
    deserializer: D,
) -> std::result::Result<VersionReq, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    VersionReq::parse(&value).map_err(serde::de::Error::custom)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA256: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// 生成测试索引，覆盖依赖顺序和 SemVer 最高版本选择。
    fn registry() -> PluginRegistry {
        PluginRegistry::from_slice(
            format!(
                r#"{{
                  "schema_version": 1,
                  "packages": {{
                    "root": {{
                      "description": "根插件",
                      "publisher": "ascvow",
                      "official": true,
                      "versions": [{{
                        "version": "1.0.0",
                        "api_version": "{SUPPORTED_PLUGIN_API_VERSION}",
                        "github": {{"owner":"ascvow","repository":"lucia-plugins","tag":"plugins-v1","asset":"root.zip"}},
                        "sha256": "{SHA256}",
                        "dependencies": [{{"name":"dep","requirement":"^1.0"}}]
                      }}]
                    }},
                    "dep": {{
                      "description": "依赖插件",
                      "publisher": "ascvow",
                      "versions": [
                        {{"version":"1.0.0","api_version":"{SUPPORTED_PLUGIN_API_VERSION}","github":{{"owner":"ascvow","repository":"lucia-plugins","tag":"plugins-v1","asset":"dep-1.zip"}},"sha256":"{SHA256}"}},
                        {{"version":"1.2.0","api_version":"{SUPPORTED_PLUGIN_API_VERSION}","github":{{"owner":"ascvow","repository":"lucia-plugins","tag":"plugins-v1","asset":"dep-2.zip"}},"sha256":"{SHA256}"}}
                      ]
                    }}
                  }}
                }}"#
            )
            .as_bytes(),
        )
        .expect("测试 Registry 应可解析")
    }

    #[test]
    fn request_supports_npm_style_version_range() {
        let request = RegistryRequest::parse("context@^0.1").expect("请求应可解析");
        assert_eq!(request.name, "context");
        assert!(request.requirement.matches(&Version::new(0, 1, 8)));
        assert!(!request.requirement.matches(&Version::new(0, 2, 0)));
    }

    #[test]
    fn resolver_installs_dependencies_before_root() {
        let request = RegistryRequest::parse("root").expect("请求应可解析");
        let plan = registry().resolve(&request, &[]).expect("依赖应可求解");
        assert_eq!(plan[0].0, "dep");
        assert_eq!(plan[0].1.version, Version::new(1, 2, 0));
        assert_eq!(plan[1].0, "root");
    }

    #[test]
    fn resolver_reuses_compatible_installed_dependency() {
        let request = RegistryRequest::parse("root").expect("请求应可解析");
        let installed = vec![InstalledPlugin {
            id: "dep".into(),
            name: "dep".into(),
            version: "1.1.0".into(),
            api_version: SUPPORTED_PLUGIN_API_VERSION.into(),
            enabled: true,
            manifest: "plugins/dep/1.1.0/plugin.toml".into(),
            sha256: SHA256.into(),
            source: "local".into(),
        }];
        let plan = registry()
            .resolve(&request, &installed)
            .expect("已安装依赖应被复用");
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].0, "root");
    }

    #[test]
    fn parser_rejects_unknown_registry_fields() {
        let error =
            PluginRegistry::from_slice(br#"{"schema_version":1,"packages":{},"unexpected":true}"#)
                .expect_err("未知字段应被拒绝");
        assert!(error.to_string().contains("Registry 索引不是有效 JSON"));
    }
}

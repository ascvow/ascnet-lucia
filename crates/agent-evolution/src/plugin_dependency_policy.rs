//! 插件 Candidate 的离线 Cargo 依赖策略。
//!
//! 本模块只接受 [`PluginWorkspaceManifest`] 已绑定的真实文件。它使用一个刻意收窄的
//! Cargo TOML 安全子集：插件及其本地依赖必须全部位于同一插件 scope，依赖只能使用
//! 单行或多行 inline-table `path` 形式，锁文件不得包含远程 source。任何无法可靠解释的
//! 依赖写法都会 fail-closed，避免把 Cargo 的网络、构建脚本或配置覆盖能力带入受信构建面。

use crate::plugin_workspace::PluginWorkspaceManifest;
use agent_evolution_protocol::{ArtifactDigest, PluginSourceFile};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
};

/// 依赖计划规范摘要的 schema 版本。
pub const PLUGIN_DEPENDENCY_PLAN_SCHEMA_VERSION: u32 = 1;
/// 单个 Cargo 清单允许的最大字节数。
pub const MAX_PLUGIN_CARGO_MANIFEST_BYTES: u64 = 1024 * 1024;
/// Cargo 锁文件允许的最大字节数。
pub const MAX_PLUGIN_CARGO_LOCK_BYTES: u64 = 8 * 1024 * 1024;

/// 已通过完整文件清单和 Cargo 依赖策略校验的不可伪造计划。
///
/// 字段保持私有，确保构建 Worker 只能消费本模块实际验证过的工作区。Worker 在每个命令
/// 前后仍会调用 [`Self::revalidate_workspace`]，防止物化后篡改或测试进程写回源码。
#[derive(Debug)]
pub struct ValidatedPluginDependencyPlan {
    manifest: PluginWorkspaceManifest,
    cargo_manifest_path: PathBuf,
    cargo_lock_path: PathBuf,
    cargo_lock_digest: ArtifactDigest,
    dependency_digest: ArtifactDigest,
    package_name: String,
    local_dependency_manifests: Vec<String>,
}

impl ValidatedPluginDependencyPlan {
    /// 返回插件 ID。
    pub fn plugin_id(&self) -> &str {
        &self.manifest.plugin_id
    }

    /// 返回 Candidate 的可信源码树摘要。
    pub fn source_digest(&self) -> &ArtifactDigest {
        &self.manifest.source_digest
    }

    /// 返回固定 Cargo.lock 的真实字节摘要。
    pub fn cargo_lock_digest(&self) -> &ArtifactDigest {
        &self.cargo_lock_digest
    }

    /// 返回完整依赖计划的规范摘要。
    pub fn dependency_digest(&self) -> &ArtifactDigest {
        &self.dependency_digest
    }

    /// 返回根插件 package 名称。
    pub fn package_name(&self) -> &str {
        &self.package_name
    }

    /// 返回按路径排序的本地依赖 Cargo 清单。
    pub fn local_dependency_manifests(&self) -> &[String] {
        &self.local_dependency_manifests
    }

    /// 返回构建命令的插件 crate 根目录。
    pub(crate) fn crate_root(&self) -> &Path {
        self.cargo_manifest_path
            .parent()
            .expect("已验证 Cargo.toml 必须有父目录")
    }

    /// 返回构建完成后应读取的 WASM 文件名。
    pub(crate) fn component_file_name(&self) -> String {
        format!("{}.wasm", self.package_name.replace('-', "_"))
    }

    /// 返回本计划拥有的专用物化根目录。
    pub(crate) fn workspace_root(&self) -> &Path {
        &self.manifest.root
    }

    /// 重新核对磁盘上的完整文件集合、类型、大小、摘要和固定 Cargo.lock。
    ///
    /// # Errors
    ///
    /// 工作区根、任一目录项、额外文件、缺失文件或内容摘要与已验证计划不一致时返回
    /// [`PluginDependencyPolicyError`]。
    pub(crate) fn revalidate_workspace(&self) -> Result<(), PluginDependencyPolicyError> {
        verify_materialized_workspace(&self.manifest)?;
        let actual_lock = digest_regular_file(&self.cargo_lock_path, MAX_PLUGIN_CARGO_LOCK_BYTES)?;
        if actual_lock != self.cargo_lock_digest {
            return Err(PluginDependencyPolicyError::CargoLockChanged {
                expected: self.cargo_lock_digest.clone(),
                actual: actual_lock,
            });
        }
        Ok(())
    }
}

/// 插件 Cargo 依赖策略入口。
#[derive(Debug, Clone, Copy, Default)]
pub struct PluginDependencyPolicy;

impl PluginDependencyPolicy {
    /// 验证物化工作区及其中所有 Cargo 清单，生成 Worker 可消费的依赖计划。
    ///
    /// 只允许 scope 内本地 path dependency；拒绝 build.rs、替代构建脚本、proc-macro、
    /// build-dependencies、Git/registry/version/workspace 依赖、`.cargo/config*`、`[patch]`
    /// 与 `[replace]`。根 Cargo.lock 必须存在且不含任何远程 source。
    ///
    /// # Errors
    ///
    /// 文件清单、资源边界、UTF-8、Cargo 安全子集或依赖路径不满足策略时返回
    /// [`PluginDependencyPolicyError`]。
    pub fn validate(
        manifest: PluginWorkspaceManifest,
    ) -> Result<ValidatedPluginDependencyPlan, PluginDependencyPolicyError> {
        verify_materialized_workspace(&manifest)?;
        validate_forbidden_paths(&manifest)?;

        let cargo_manifest_relative = format!("{}/Cargo.toml", manifest.plugin_scope);
        let cargo_lock_relative = format!("{}/Cargo.lock", manifest.plugin_scope);
        ensure_declared_file(&manifest, &cargo_manifest_relative)?;
        ensure_declared_file(&manifest, &cargo_lock_relative)?;
        let cargo_manifest_path = manifest.root.join(&cargo_manifest_relative);
        let cargo_lock_path = manifest.root.join(&cargo_lock_relative);
        let cargo_lock = read_bounded_utf8(&cargo_lock_path, MAX_PLUGIN_CARGO_LOCK_BYTES)?;
        validate_lockfile(&cargo_lock)?;
        let cargo_lock_digest = digest_bytes(cargo_lock.as_bytes())?;

        let mut pending = BTreeSet::from([cargo_manifest_relative.clone()]);
        let mut visited = BTreeSet::new();
        let mut package_name = None;
        let mut manifest_digests = BTreeMap::new();
        while let Some(relative_path) = pending.pop_first() {
            if !visited.insert(relative_path.clone()) {
                continue;
            }
            ensure_declared_file(&manifest, &relative_path)?;
            let absolute_path = manifest.root.join(&relative_path);
            let text = read_bounded_utf8(&absolute_path, MAX_PLUGIN_CARGO_MANIFEST_BYTES)?;
            let parsed = validate_cargo_manifest(
                &manifest,
                &relative_path,
                &text,
                relative_path == cargo_manifest_relative,
            )?;
            if relative_path == cargo_manifest_relative {
                package_name = parsed.package_name;
            }
            pending.extend(parsed.local_dependency_manifests);
            manifest_digests.insert(relative_path, digest_bytes(text.as_bytes())?);
        }

        let package_name = package_name.ok_or(PluginDependencyPolicyError::MissingPackageName)?;
        let local_dependency_manifests = visited
            .iter()
            .filter(|path| *path != &cargo_manifest_relative)
            .cloned()
            .collect::<Vec<_>>();
        let dependency_digest = dependency_plan_digest(
            &manifest,
            &cargo_lock_digest,
            &manifest_digests,
            &local_dependency_manifests,
        )?;
        Ok(ValidatedPluginDependencyPlan {
            manifest,
            cargo_manifest_path,
            cargo_lock_path,
            cargo_lock_digest,
            dependency_digest,
            package_name,
            local_dependency_manifests,
        })
    }
}

/// 插件依赖策略失败原因。
#[derive(Debug, thiserror::Error)]
pub enum PluginDependencyPolicyError {
    /// 物化根不是安全的真实目录。
    #[error("插件物化根必须是非符号链接目录：{0}")]
    UnsafeWorkspaceRoot(PathBuf),
    /// 工作区包含符号链接或非普通文件条目。
    #[error("插件工作区包含不允许的文件系统条目：{0}")]
    UnsafeWorkspaceEntry(PathBuf),
    /// 工作区包含物化清单未声明的文件。
    #[error("插件工作区出现额外文件：`{0}`")]
    ExtraWorkspaceFile(String),
    /// 物化清单中的文件已缺失。
    #[error("插件工作区缺少清单文件：`{0}`")]
    MissingWorkspaceFile(String),
    /// 物化文件大小或摘要被改变。
    #[error("插件工作区文件内容与清单不一致：`{0}`")]
    WorkspaceFileChanged(String),
    /// 必需 Cargo 文件不在可信物化清单中。
    #[error("插件工作区缺少必需 Cargo 文件：`{0}`")]
    MissingCargoFile(String),
    /// 工作区含 build.rs、Cargo 配置或其他明确禁止路径。
    #[error("插件依赖策略拒绝路径：`{0}`")]
    ForbiddenPath(String),
    /// 文件超过依赖策略资源上限。
    #[error("插件 Cargo 文件 {path} 大小 {size_bytes} 超过上限 {max_bytes}")]
    FileTooLarge {
        /// 文件路径。
        path: PathBuf,
        /// 实际字节数。
        size_bytes: u64,
        /// 允许的最大字节数。
        max_bytes: u64,
    },
    /// Cargo 文件不是 UTF-8。
    #[error("插件 Cargo 文件必须是 UTF-8：{0}")]
    NonUtf8CargoFile(PathBuf),
    /// Cargo TOML 使用了策略不支持的语法。
    #[error("插件 Cargo 清单 `{path}` 使用不受支持的安全子集语法：{detail}")]
    UnsupportedCargoSyntax {
        /// 相对 Cargo.toml 路径。
        path: String,
        /// 稳定错误说明。
        detail: String,
    },
    /// Cargo 清单声明了禁止的配置或依赖能力。
    #[error("插件 Cargo 清单 `{path}` 声明了禁止项 `{item}`")]
    ForbiddenCargoItem {
        /// 相对 Cargo.toml 路径。
        path: String,
        /// 被拒绝的字段或 section。
        item: String,
    },
    /// 根 Cargo 清单缺少稳定 package 名称。
    #[error("插件根 Cargo.toml 缺少合法 package.name")]
    MissingPackageName,
    /// path dependency 逃出插件 scope 或不是安全相对路径。
    #[error("插件本地依赖 `{dependency}` 的路径 `{path}` 越出插件 scope")]
    OutsideDependencyScope {
        /// Cargo dependency 名称。
        dependency: String,
        /// Candidate 声明的 path。
        path: String,
    },
    /// path dependency 没有指向清单中真实存在的 Cargo.toml。
    #[error("插件本地依赖 `{dependency}` 缺少已声明 Cargo.toml：`{manifest_path}`")]
    MissingDependencyManifest {
        /// Cargo dependency 名称。
        dependency: String,
        /// 预期 Cargo.toml 相对路径。
        manifest_path: String,
    },
    /// Cargo.lock 包含 Git、registry 或其他远程来源。
    #[error("插件 Cargo.lock 包含不允许的远程 source")]
    RemoteCargoLockSource,
    /// Cargo.lock 在策略验证后发生变化。
    #[error("插件 Cargo.lock 已变化：期望 {expected}，实际 {actual}")]
    CargoLockChanged {
        /// 策略验证时摘要。
        expected: ArtifactDigest,
        /// Worker 复核摘要。
        actual: ArtifactDigest,
    },
    /// 文件系统访问失败。
    #[error("访问插件 Cargo 工作区失败：{path}: {source}")]
    Io {
        /// 访问路径。
        path: PathBuf,
        /// 原始错误。
        #[source]
        source: std::io::Error,
    },
    /// 规范摘要序列化失败。
    #[error("序列化插件依赖计划失败：{0}")]
    Serialization(serde_json::Error),
    /// SHA-256 无法转换为强类型摘要。
    #[error("构造插件依赖摘要失败：{0}")]
    DigestConstruction(String),
}

#[derive(Debug)]
struct ParsedCargoManifest {
    package_name: Option<String>,
    local_dependency_manifests: BTreeSet<String>,
}

#[derive(Debug, Serialize)]
struct DependencyPlanDigestPayload<'a> {
    domain: &'static str,
    schema_version: u32,
    plugin_id: &'a str,
    plugin_scope: &'a str,
    source_digest: &'a ArtifactDigest,
    cargo_lock_digest: &'a ArtifactDigest,
    cargo_manifests: &'a BTreeMap<String, ArtifactDigest>,
    local_dependency_manifests: &'a [String],
}

fn validate_forbidden_paths(
    manifest: &PluginWorkspaceManifest,
) -> Result<(), PluginDependencyPolicyError> {
    let mut lock_count = 0_usize;
    for file in &manifest.files {
        let relative = file
            .path
            .strip_prefix(&manifest.plugin_scope)
            .and_then(|path| path.strip_prefix('/'))
            .unwrap_or_default();
        let components = relative.split('/').collect::<Vec<_>>();
        let file_name = components.last().copied().unwrap_or_default();
        if file_name.eq_ignore_ascii_case("build.rs")
            || components
                .iter()
                .any(|component| component.eq_ignore_ascii_case(".cargo"))
        {
            return Err(PluginDependencyPolicyError::ForbiddenPath(
                file.path.clone(),
            ));
        }
        if file_name == "Cargo.lock" {
            lock_count += 1;
            if file.path != format!("{}/Cargo.lock", manifest.plugin_scope) {
                return Err(PluginDependencyPolicyError::ForbiddenPath(
                    file.path.clone(),
                ));
            }
        }
    }
    if lock_count > 1 {
        return Err(PluginDependencyPolicyError::ForbiddenPath(
            "嵌套 Cargo.lock".to_string(),
        ));
    }
    Ok(())
}

fn validate_cargo_manifest(
    workspace: &PluginWorkspaceManifest,
    manifest_path: &str,
    text: &str,
    is_root: bool,
) -> Result<ParsedCargoManifest, PluginDependencyPolicyError> {
    let statements = logical_toml_statements(text).map_err(|detail| {
        PluginDependencyPolicyError::UnsupportedCargoSyntax {
            path: manifest_path.to_string(),
            detail,
        }
    })?;
    let mut section = String::new();
    let mut package_name = None;
    let mut dependencies = BTreeSet::new();
    for statement in statements {
        if let Some(name) = parse_section(&statement) {
            section = name.to_ascii_lowercase();
            if section == "patch"
                || section.starts_with("patch.")
                || section == "replace"
                || section.starts_with("replace.")
                || section == "workspace"
                || section.starts_with("workspace.")
                || section == "build-dependencies"
                || section.ends_with(".build-dependencies")
            {
                return Err(PluginDependencyPolicyError::ForbiddenCargoItem {
                    path: manifest_path.to_string(),
                    item: format!("[{section}]"),
                });
            }
            if section.starts_with("dependencies.")
                || section.starts_with("dev-dependencies.")
                || (section.starts_with("target.")
                    && (section.contains(".dependencies.")
                        || section.contains(".dev-dependencies.")))
            {
                return Err(PluginDependencyPolicyError::UnsupportedCargoSyntax {
                    path: manifest_path.to_string(),
                    detail: "依赖必须使用 dependencies section 内的 inline table".to_string(),
                });
            }
            continue;
        }

        let (key, value) = parse_assignment(&statement).ok_or_else(|| {
            PluginDependencyPolicyError::UnsupportedCargoSyntax {
                path: manifest_path.to_string(),
                detail: "无法解析 TOML 赋值".to_string(),
            }
        })?;
        let key_lower = key.to_ascii_lowercase();
        if section == "package" && matches!(key_lower.as_str(), "build" | "links" | "workspace") {
            return Err(PluginDependencyPolicyError::ForbiddenCargoItem {
                path: manifest_path.to_string(),
                item: format!("package.{key_lower}"),
            });
        }
        if section == "lib" && key_lower == "proc-macro" && parse_toml_bool(value) != Some(false) {
            return Err(PluginDependencyPolicyError::ForbiddenCargoItem {
                path: manifest_path.to_string(),
                item: "lib.proc-macro".to_string(),
            });
        }
        if section == "lib"
            && key_lower == "crate-type"
            && value.to_ascii_lowercase().contains("proc-macro")
        {
            return Err(PluginDependencyPolicyError::ForbiddenCargoItem {
                path: manifest_path.to_string(),
                item: "lib.crate-type=proc-macro".to_string(),
            });
        }
        if section == "package" && key_lower == "name" {
            let name = parse_toml_string(value).ok_or_else(|| {
                PluginDependencyPolicyError::UnsupportedCargoSyntax {
                    path: manifest_path.to_string(),
                    detail: "package.name 必须是普通字符串".to_string(),
                }
            })?;
            if !valid_package_name(&name) {
                return Err(PluginDependencyPolicyError::UnsupportedCargoSyntax {
                    path: manifest_path.to_string(),
                    detail: "package.name 不合法".to_string(),
                });
            }
            package_name = Some(name);
        }
        if is_dependency_section(&section) {
            let dependency_manifest = validate_dependency(workspace, manifest_path, &key, value)?;
            dependencies.insert(dependency_manifest);
        }
    }
    if is_root && package_name.is_none() {
        return Err(PluginDependencyPolicyError::MissingPackageName);
    }
    Ok(ParsedCargoManifest {
        package_name,
        local_dependency_manifests: dependencies,
    })
}

fn validate_dependency(
    workspace: &PluginWorkspaceManifest,
    manifest_path: &str,
    dependency: &str,
    value: &str,
) -> Result<String, PluginDependencyPolicyError> {
    let fields = parse_inline_table(value).map_err(|detail| {
        PluginDependencyPolicyError::UnsupportedCargoSyntax {
            path: manifest_path.to_string(),
            detail: format!("依赖 `{dependency}` {detail}"),
        }
    })?;
    for forbidden in ["git", "registry", "version", "workspace"] {
        if fields.contains_key(forbidden) {
            return Err(PluginDependencyPolicyError::ForbiddenCargoItem {
                path: manifest_path.to_string(),
                item: format!("依赖 `{dependency}` 的 `{forbidden}`"),
            });
        }
    }
    let path = fields
        .get("path")
        .and_then(|value| parse_toml_string(value))
        .ok_or_else(|| PluginDependencyPolicyError::ForbiddenCargoItem {
            path: manifest_path.to_string(),
            item: format!("依赖 `{dependency}` 不是纯本地 path dependency"),
        })?;
    let manifest_directory = Path::new(manifest_path)
        .parent()
        .unwrap_or_else(|| Path::new(""));
    let dependency_directory = normalize_scoped_dependency_path(
        &workspace.plugin_scope,
        manifest_directory,
        dependency,
        &path,
    )?;
    let dependency_manifest = format!("{dependency_directory}/Cargo.toml");
    if !workspace
        .files
        .iter()
        .any(|file| file.path == dependency_manifest)
    {
        return Err(PluginDependencyPolicyError::MissingDependencyManifest {
            dependency: dependency.to_string(),
            manifest_path: dependency_manifest,
        });
    }
    Ok(dependency_manifest)
}

fn normalize_scoped_dependency_path(
    scope: &str,
    base: &Path,
    dependency: &str,
    raw_path: &str,
) -> Result<String, PluginDependencyPolicyError> {
    if raw_path.is_empty() || raw_path.contains('\\') || Path::new(raw_path).is_absolute() {
        return Err(PluginDependencyPolicyError::OutsideDependencyScope {
            dependency: dependency.to_string(),
            path: raw_path.to_string(),
        });
    }
    let mut components = base
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>();
    for component in Path::new(raw_path).components() {
        match component {
            Component::CurDir => {}
            Component::Normal(value) => components.push(value.to_string_lossy().into_owned()),
            Component::ParentDir => {
                if components.pop().is_none() {
                    return Err(PluginDependencyPolicyError::OutsideDependencyScope {
                        dependency: dependency.to_string(),
                        path: raw_path.to_string(),
                    });
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(PluginDependencyPolicyError::OutsideDependencyScope {
                    dependency: dependency.to_string(),
                    path: raw_path.to_string(),
                });
            }
        }
    }
    let normalized = components.join("/");
    if normalized != scope
        && !normalized
            .strip_prefix(scope)
            .is_some_and(|suffix| suffix.starts_with('/'))
    {
        return Err(PluginDependencyPolicyError::OutsideDependencyScope {
            dependency: dependency.to_string(),
            path: raw_path.to_string(),
        });
    }
    Ok(normalized)
}

fn logical_toml_statements(text: &str) -> Result<Vec<String>, String> {
    if text.contains("\"\"\"") || text.contains("'''") {
        return Err("不接受多行字符串".to_string());
    }
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut curly_depth = 0_i32;
    let mut square_depth = 0_i32;
    for raw_line in text.lines() {
        let line = strip_toml_comment(raw_line)?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if current.is_empty() && parse_section(trimmed).is_some() {
            statements.push(trimmed.to_string());
            continue;
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(trimmed);
        update_toml_depth(trimmed, &mut curly_depth, &mut square_depth)?;
        if curly_depth == 0 && square_depth == 0 {
            statements.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() || curly_depth != 0 || square_depth != 0 {
        return Err("TOML 括号或字符串未闭合".to_string());
    }
    Ok(statements)
}

fn strip_toml_comment(line: &str) -> Result<String, String> {
    let mut output = String::new();
    let mut quote = None;
    let mut escaped = false;
    for character in line.chars() {
        if escaped {
            output.push(character);
            escaped = false;
            continue;
        }
        if quote == Some('"') && character == '\\' {
            output.push(character);
            escaped = true;
            continue;
        }
        if matches!(character, '"' | '\'') {
            if quote == Some(character) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(character);
            }
            output.push(character);
            continue;
        }
        if character == '#' && quote.is_none() {
            break;
        }
        output.push(character);
    }
    if quote.is_some() {
        return Err("单行字符串未闭合".to_string());
    }
    Ok(output)
}

fn update_toml_depth(
    text: &str,
    curly_depth: &mut i32,
    square_depth: &mut i32,
) -> Result<(), String> {
    let mut quote = None;
    let mut escaped = false;
    for character in text.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if quote == Some('"') && character == '\\' {
            escaped = true;
            continue;
        }
        if matches!(character, '"' | '\'') {
            if quote == Some(character) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(character);
            }
            continue;
        }
        if quote.is_some() {
            continue;
        }
        match character {
            '{' => *curly_depth += 1,
            '}' => *curly_depth -= 1,
            '[' => *square_depth += 1,
            ']' => *square_depth -= 1,
            _ => {}
        }
        if *curly_depth < 0 || *square_depth < 0 {
            return Err("TOML 括号不平衡".to_string());
        }
    }
    if quote.is_some() {
        return Err("字符串未闭合".to_string());
    }
    Ok(())
}

fn parse_section(statement: &str) -> Option<String> {
    let trimmed = statement.trim();
    if trimmed.starts_with("[[") || !trimmed.starts_with('[') || !trimmed.ends_with(']') {
        return None;
    }
    let inner = trimmed[1..trimmed.len() - 1].trim();
    if inner.is_empty() {
        return None;
    }
    Some(
        inner
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect(),
    )
}

fn parse_assignment(statement: &str) -> Option<(String, &str)> {
    let index = top_level_separator(statement, '=')?;
    let key = statement[..index].trim();
    if !valid_toml_key(key) {
        return None;
    }
    let value = statement[index + 1..].trim();
    if value.is_empty() {
        return None;
    }
    Some((key.to_string(), value))
}

fn parse_inline_table(value: &str) -> Result<BTreeMap<String, &str>, String> {
    let trimmed = value.trim();
    if !trimmed.starts_with('{') || !trimmed.ends_with('}') {
        return Err("必须使用只含 path 的 inline table".to_string());
    }
    let mut fields = BTreeMap::new();
    for field in split_top_level(&trimmed[1..trimmed.len() - 1], ',')? {
        if field.trim().is_empty() {
            continue;
        }
        let (key, value) = parse_assignment(field.trim())
            .ok_or_else(|| "inline table 字段无法解析".to_string())?;
        let key = key.to_ascii_lowercase();
        if fields.insert(key.clone(), value).is_some() {
            return Err(format!("inline table 字段 `{key}` 重复"));
        }
    }
    Ok(fields)
}

fn split_top_level(text: &str, separator: char) -> Result<Vec<&str>, String> {
    let mut parts = Vec::new();
    let mut start = 0_usize;
    let mut quote = None;
    let mut escaped = false;
    let mut square = 0_i32;
    let mut curly = 0_i32;
    for (index, character) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if quote == Some('"') && character == '\\' {
            escaped = true;
            continue;
        }
        if matches!(character, '"' | '\'') {
            if quote == Some(character) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(character);
            }
            continue;
        }
        if quote.is_some() {
            continue;
        }
        match character {
            '[' => square += 1,
            ']' => square -= 1,
            '{' => curly += 1,
            '}' => curly -= 1,
            value if value == separator && square == 0 && curly == 0 => {
                parts.push(&text[start..index]);
                start = index + value.len_utf8();
            }
            _ => {}
        }
        if square < 0 || curly < 0 {
            return Err("嵌套括号不平衡".to_string());
        }
    }
    if quote.is_some() || square != 0 || curly != 0 {
        return Err("嵌套值未闭合".to_string());
    }
    parts.push(&text[start..]);
    Ok(parts)
}

fn top_level_separator(text: &str, separator: char) -> Option<usize> {
    split_top_level_indices(text, separator).into_iter().next()
}

fn split_top_level_indices(text: &str, separator: char) -> Vec<usize> {
    let mut indices = Vec::new();
    let mut quote = None;
    let mut escaped = false;
    let mut square = 0_i32;
    let mut curly = 0_i32;
    for (index, character) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if quote == Some('"') && character == '\\' {
            escaped = true;
            continue;
        }
        if matches!(character, '"' | '\'') {
            if quote == Some(character) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(character);
            }
            continue;
        }
        if quote.is_some() {
            continue;
        }
        match character {
            '[' => square += 1,
            ']' => square -= 1,
            '{' => curly += 1,
            '}' => curly -= 1,
            value if value == separator && square == 0 && curly == 0 => indices.push(index),
            _ => {}
        }
    }
    indices
}

fn parse_toml_string(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.len() < 2 {
        return None;
    }
    let quote = trimmed.chars().next()?;
    if !matches!(quote, '"' | '\'') || !trimmed.ends_with(quote) {
        return None;
    }
    let inner = &trimmed[quote.len_utf8()..trimmed.len() - quote.len_utf8()];
    if quote == '\'' {
        return Some(inner.to_string());
    }
    let mut output = String::new();
    let mut characters = inner.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            output.push(character);
            continue;
        }
        match characters.next()? {
            '\\' => output.push('\\'),
            '"' => output.push('"'),
            'n' => output.push('\n'),
            'r' => output.push('\r'),
            't' => output.push('\t'),
            _ => return None,
        }
    }
    Some(output)
}

fn parse_toml_bool(value: &str) -> Option<bool> {
    match value.trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn valid_toml_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn valid_package_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn is_dependency_section(section: &str) -> bool {
    matches!(section, "dependencies" | "dev-dependencies")
        || (section.starts_with("target.")
            && (section.ends_with(".dependencies") || section.ends_with(".dev-dependencies")))
}

fn validate_lockfile(text: &str) -> Result<(), PluginDependencyPolicyError> {
    let statements = logical_toml_statements(text).map_err(|detail| {
        PluginDependencyPolicyError::UnsupportedCargoSyntax {
            path: "Cargo.lock".to_string(),
            detail,
        }
    })?;
    for statement in statements {
        if let Some((key, _)) = parse_assignment(&statement) {
            if key.eq_ignore_ascii_case("source") || key.eq_ignore_ascii_case("checksum") {
                return Err(PluginDependencyPolicyError::RemoteCargoLockSource);
            }
        }
    }
    Ok(())
}

fn ensure_declared_file(
    manifest: &PluginWorkspaceManifest,
    path: &str,
) -> Result<(), PluginDependencyPolicyError> {
    if manifest.files.iter().any(|file| file.path == path) {
        Ok(())
    } else {
        Err(PluginDependencyPolicyError::MissingCargoFile(
            path.to_string(),
        ))
    }
}

fn verify_materialized_workspace(
    manifest: &PluginWorkspaceManifest,
) -> Result<(), PluginDependencyPolicyError> {
    let root_metadata =
        fs::symlink_metadata(&manifest.root).map_err(|source| io_error(&manifest.root, source))?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(PluginDependencyPolicyError::UnsafeWorkspaceRoot(
            manifest.root.clone(),
        ));
    }
    let expected = manifest
        .files
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect::<BTreeMap<_, _>>();
    let mut actual_paths = BTreeSet::new();
    collect_workspace_files(&manifest.root, &manifest.root, &mut actual_paths)?;
    for actual in &actual_paths {
        if !expected.contains_key(actual.as_str()) {
            return Err(PluginDependencyPolicyError::ExtraWorkspaceFile(
                actual.clone(),
            ));
        }
    }
    for (path, expected_file) in expected {
        if !actual_paths.contains(path) {
            return Err(PluginDependencyPolicyError::MissingWorkspaceFile(
                path.to_string(),
            ));
        }
        verify_file(&manifest.root, expected_file)?;
    }
    Ok(())
}

fn collect_workspace_files(
    root: &Path,
    directory: &Path,
    files: &mut BTreeSet<String>,
) -> Result<(), PluginDependencyPolicyError> {
    let entries = fs::read_dir(directory).map_err(|source| io_error(directory, source))?;
    for entry in entries {
        let entry = entry.map_err(|source| io_error(directory, source))?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|source| io_error(&path, source))?;
        if metadata.file_type().is_symlink() {
            return Err(PluginDependencyPolicyError::UnsafeWorkspaceEntry(path));
        }
        if metadata.is_dir() {
            collect_workspace_files(root, &path, files)?;
        } else if metadata.is_file() {
            let relative = path
                .strip_prefix(root)
                .map_err(|_| PluginDependencyPolicyError::UnsafeWorkspaceEntry(path.clone()))?;
            let normalized = relative
                .components()
                .map(|component| component.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            files.insert(normalized);
        } else {
            return Err(PluginDependencyPolicyError::UnsafeWorkspaceEntry(path));
        }
    }
    Ok(())
}

fn verify_file(root: &Path, file: &PluginSourceFile) -> Result<(), PluginDependencyPolicyError> {
    let path = root.join(&file.path);
    let metadata = fs::symlink_metadata(&path).map_err(|source| io_error(&path, source))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() != file.size_bytes
        || digest_regular_file(&path, file.size_bytes)? != file.digest
    {
        return Err(PluginDependencyPolicyError::WorkspaceFileChanged(
            file.path.clone(),
        ));
    }
    Ok(())
}

fn read_bounded_utf8(path: &Path, max_bytes: u64) -> Result<String, PluginDependencyPolicyError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| io_error(path, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(PluginDependencyPolicyError::UnsafeWorkspaceEntry(
            path.to_path_buf(),
        ));
    }
    if metadata.len() > max_bytes {
        return Err(PluginDependencyPolicyError::FileTooLarge {
            path: path.to_path_buf(),
            size_bytes: metadata.len(),
            max_bytes,
        });
    }
    let bytes = read_bounded_bytes(path, max_bytes)?;
    String::from_utf8(bytes)
        .map_err(|_| PluginDependencyPolicyError::NonUtf8CargoFile(path.to_path_buf()))
}

fn digest_regular_file(
    path: &Path,
    max_bytes: u64,
) -> Result<ArtifactDigest, PluginDependencyPolicyError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| io_error(path, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(PluginDependencyPolicyError::UnsafeWorkspaceEntry(
            path.to_path_buf(),
        ));
    }
    if metadata.len() > max_bytes {
        return Err(PluginDependencyPolicyError::FileTooLarge {
            path: path.to_path_buf(),
            size_bytes: metadata.len(),
            max_bytes,
        });
    }
    let bytes = read_bounded_bytes(path, max_bytes)?;
    digest_bytes(&bytes)
}

fn read_bounded_bytes(path: &Path, max_bytes: u64) -> Result<Vec<u8>, PluginDependencyPolicyError> {
    let file = fs::File::open(path).map_err(|source| io_error(path, source))?;
    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|source| io_error(path, source))?;
    if bytes.len() as u64 > max_bytes {
        return Err(PluginDependencyPolicyError::FileTooLarge {
            path: path.to_path_buf(),
            size_bytes: bytes.len() as u64,
            max_bytes,
        });
    }
    Ok(bytes)
}

fn dependency_plan_digest(
    manifest: &PluginWorkspaceManifest,
    cargo_lock_digest: &ArtifactDigest,
    cargo_manifests: &BTreeMap<String, ArtifactDigest>,
    local_dependency_manifests: &[String],
) -> Result<ArtifactDigest, PluginDependencyPolicyError> {
    let bytes = serde_json::to_vec(&DependencyPlanDigestPayload {
        domain: "ascnet.lucia.plugin-dependency-plan.v1",
        schema_version: PLUGIN_DEPENDENCY_PLAN_SCHEMA_VERSION,
        plugin_id: &manifest.plugin_id,
        plugin_scope: &manifest.plugin_scope,
        source_digest: &manifest.source_digest,
        cargo_lock_digest,
        cargo_manifests,
        local_dependency_manifests,
    })
    .map_err(PluginDependencyPolicyError::Serialization)?;
    digest_bytes(&bytes)
}

fn digest_bytes(bytes: &[u8]) -> Result<ArtifactDigest, PluginDependencyPolicyError> {
    let hex = format!("{:x}", Sha256::digest(bytes));
    ArtifactDigest::from_sha256_hex(hex)
        .map_err(|error| PluginDependencyPolicyError::DigestConstruction(error.to_string()))
}

fn io_error(path: &Path, source: std::io::Error) -> PluginDependencyPolicyError {
    PluginDependencyPolicyError::Io {
        path: path.to_path_buf(),
        source,
    }
}

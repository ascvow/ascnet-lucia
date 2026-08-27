//! 插件 Candidate 的隔离源码物化与 Patch Scope Validation。
//!
//! 本模块不联网、不执行源码，也不从 Candidate 声明推断可信能力。调用方必须提供 Parent
//! 文件字节和 Create/Update 的新文件字节；本模块先在内存中验证路径、大小、摘要、补丁
//! 精确覆盖和受保护表面，再创建一个此前不存在的专用目录。任何验证失败都不会触碰目标
//! 目录，写入阶段失败会清理本次创建的目录。

use agent_evolution_protocol::{
    ArtifactDigest, InvalidPluginEvolution, PluginFilePatch, PluginSourceArtifact, PluginSourceFile,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

/// 单次隔离物化允许接收的最大 Parent 文件数。
pub const MAX_PLUGIN_WORKSPACE_FILES: usize = 4_096;
/// 单个输入文件允许的最大字节数。
pub const MAX_PLUGIN_WORKSPACE_FILE_BYTES: u64 = 16 * 1024 * 1024;
/// Parent 或 Candidate 完整源码树允许的最大总字节数。
pub const MAX_PLUGIN_WORKSPACE_TOTAL_BYTES: u64 = 128 * 1024 * 1024;
/// 单次计划允许的最大结构化补丁数。
pub const MAX_PLUGIN_WORKSPACE_PATCHES: usize = 1_024;
/// Patch Plan 规范摘要版本。
pub const PLUGIN_PATCH_PLAN_SCHEMA_VERSION: u32 = 1;

/// 调用方向隔离物化器提供的一个文件系统条目。
///
/// 只有 `RegularFile` 能进入计划。`SymbolicLink` 变体用于让 CAS 解包器或调用方显式报告
/// 归档中的链接语义；物化器始终拒绝它，且不会读取链接目标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginWorkspaceEntry {
    /// 普通文件的完整原始字节。
    RegularFile(Vec<u8>),
    /// 不可信符号链接声明；目标只用于上游审计，本模块不会解析或访问。
    SymbolicLink {
        /// 归档声明的链接目标。
        target: String,
    },
}

/// 一次插件源码物化请求。
///
/// `parent_files` 是完整 Parent 源码映射；`replacement_files` 只允许包含 Create/Update
/// 补丁的新字节。所有路径都必须严格位于 `plugin_scope` 下，且补丁必须按路径严格升序。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginWorkspaceRequest {
    /// 目标插件的稳定 ID。
    pub plugin_id: String,
    /// 相对隔离目录根的唯一插件 scope，例如 `plugins/example`。
    pub plugin_scope: String,
    /// Stable Parent 源码树的受信规范摘要，用于拒绝错绑或过期字节映射。
    pub expected_parent_source_digest: ArtifactDigest,
    /// Parent 源码的完整路径到条目映射。
    pub parent_files: BTreeMap<String, PluginWorkspaceEntry>,
    /// Create/Update 路径到新条目的映射；Delete 不得提供新条目。
    pub replacement_files: BTreeMap<String, PluginWorkspaceEntry>,
    /// 结构化补丁，按路径严格升序排列。
    pub patches: Vec<PluginFilePatch>,
}

/// 已完成全部内存校验、可以安全物化的不可变计划。
///
/// 文件字节只保存在进程内，不参与 Debug 输出或序列化。调用 [`Self::materialize`] 才会
/// 创建目标目录和文件。
pub struct ValidatedPluginWorkspacePlan {
    plugin_id: String,
    plugin_scope: String,
    parent_source_digest: ArtifactDigest,
    source_digest: ArtifactDigest,
    source_artifact: PluginSourceArtifact,
    patch_digest: ArtifactDigest,
    files: Vec<PlannedFile>,
}

impl std::fmt::Debug for ValidatedPluginWorkspacePlan {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ValidatedPluginWorkspacePlan")
            .field("plugin_id", &self.plugin_id)
            .field("plugin_scope", &self.plugin_scope)
            .field("parent_source_digest", &self.parent_source_digest)
            .field("source_digest", &self.source_digest)
            .field("patch_digest", &self.patch_digest)
            .field("file_count", &self.files.len())
            .finish()
    }
}

impl ValidatedPluginWorkspacePlan {
    /// 返回 Parent 源码树的可信规范摘要。
    pub fn parent_source_digest(&self) -> &ArtifactDigest {
        &self.parent_source_digest
    }

    /// 返回应用补丁后 Candidate 源码树的可信规范摘要。
    ///
    /// 摘要在计划构造时完成计算，后续物化不会重新解释 Candidate 声明。
    pub fn source_digest(&self) -> &ArtifactDigest {
        &self.source_digest
    }

    /// 返回已按路径排序的结构化 Patch Plan 摘要。
    pub fn patch_digest(&self) -> &ArtifactDigest {
        &self.patch_digest
    }

    /// 返回 Candidate 的规范物化文件清单。
    pub fn files(&self) -> &[PluginSourceFile] {
        &self.source_artifact.files
    }

    /// 将已验证计划写入一个必须不存在的专用目录。
    ///
    /// 目标的直接父目录必须已经存在，且必须是非符号链接目录。根目录在所有内存校验完成
    /// 后以 create-new 语义创建；每个文件也使用 `create_new`。写入或回读校验失败时，本
    /// 方法会移除本次创建的根目录，不会保留可被误用为 Candidate 的半成品。
    ///
    /// # Errors
    ///
    /// 目标已存在、父目录不安全、创建或写入失败、回读摘要不符，或失败后无法清理时返回
    /// [`PluginWorkspaceError`]。
    pub fn materialize(
        self,
        destination: impl AsRef<Path>,
    ) -> Result<PluginWorkspaceManifest, PluginWorkspaceError> {
        let destination = destination.as_ref();
        validate_destination(destination)?;
        fs::create_dir(destination)
            .map_err(|source| io_error("创建插件隔离目录", destination, source))?;

        let result = materialize_files(destination, &self.files);
        if let Err(error) = result {
            return Err(cleanup_after_error(destination, error));
        }

        Ok(PluginWorkspaceManifest {
            root: destination.to_path_buf(),
            plugin_id: self.plugin_id,
            plugin_scope: self.plugin_scope,
            parent_source_digest: self.parent_source_digest,
            source_digest: self.source_digest,
            patch_digest: self.patch_digest,
            files: self.source_artifact.files,
        })
    }
}

/// 成功物化后交给可信构建器的清单。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginWorkspaceManifest {
    /// 本次专用隔离目录。
    pub root: PathBuf,
    /// 目标插件 ID。
    pub plugin_id: String,
    /// 允许变异的插件 scope。
    pub plugin_scope: String,
    /// Parent 源码树规范摘要。
    pub parent_source_digest: ArtifactDigest,
    /// Candidate 源码树规范摘要。
    pub source_digest: ArtifactDigest,
    /// 已验证结构化 Patch Plan 摘要。
    pub patch_digest: ArtifactDigest,
    /// 按路径严格升序排列的 Candidate 文件清单。
    pub files: Vec<PluginSourceFile>,
}

/// 插件隔离源码物化器。
#[derive(Debug, Clone, Copy, Default)]
pub struct PluginWorkspaceMaterializer;

impl PluginWorkspaceMaterializer {
    /// 在内存中完成请求的全部安全校验并生成不可变计划。
    ///
    /// 本方法不访问网络、磁盘或执行任何输入字节。Parent 与 Candidate 源码摘要都从真实
    /// 文件字节重建；Candidate 自报摘要只作为待复核输入。
    ///
    /// # Errors
    ///
    /// scope、条目类型、资源边界、摘要、补丁精确性或受保护路径不合法时返回
    /// [`PluginWorkspaceError`]。
    pub fn validate(
        request: PluginWorkspaceRequest,
    ) -> Result<ValidatedPluginWorkspacePlan, PluginWorkspaceError> {
        validate_scope(&request.plugin_scope)?;
        validate_input_count("parent_files", request.parent_files.len())?;
        validate_input_count("replacement_files", request.replacement_files.len())?;

        let mut parent_bytes =
            validate_entries(&request.plugin_scope, "parent_files", request.parent_files)?;
        let mut replacement_bytes = validate_entries(
            &request.plugin_scope,
            "replacement_files",
            request.replacement_files,
        )?;
        let parent_source = source_artifact(&request.plugin_id, &parent_bytes)?;
        let parent_source_digest = parent_source.digest()?;
        if parent_source_digest != request.expected_parent_source_digest {
            return Err(PluginWorkspaceError::ParentSourceDigestMismatch {
                expected: request.expected_parent_source_digest,
                actual: parent_source_digest,
            });
        }

        validate_patch_order(&request.patches)?;
        for patch in &request.patches {
            patch.validate()?;
            validate_scoped_path(&request.plugin_scope, patch.path())?;
            validate_mutable_path(&request.plugin_scope, patch.path())?;
            apply_patch(patch, &mut parent_bytes, &mut replacement_bytes)?;
        }
        if let Some(path) = replacement_bytes.keys().next() {
            return Err(PluginWorkspaceError::UnusedReplacement(path.clone()));
        }

        let source_artifact = source_artifact(&request.plugin_id, &parent_bytes)?;
        let source_digest = source_artifact.digest()?;
        if source_digest == parent_source_digest {
            return Err(PluginWorkspaceError::UnchangedSource);
        }
        let patch_digest = patch_digest(
            &request.plugin_id,
            &request.plugin_scope,
            &parent_source_digest,
            &source_digest,
            &request.patches,
        )?;
        let files = parent_bytes
            .into_iter()
            .map(|(path, bytes)| PlannedFile { path, bytes })
            .collect();
        Ok(ValidatedPluginWorkspacePlan {
            plugin_id: request.plugin_id,
            plugin_scope: request.plugin_scope,
            parent_source_digest,
            source_digest,
            source_artifact,
            patch_digest,
            files,
        })
    }

    /// 校验请求并立即物化到一个必须不存在的专用目录。
    ///
    /// # Errors
    ///
    /// 内存计划或文件系统提交失败时返回 [`PluginWorkspaceError`]。
    pub fn validate_and_materialize(
        request: PluginWorkspaceRequest,
        destination: impl AsRef<Path>,
    ) -> Result<PluginWorkspaceManifest, PluginWorkspaceError> {
        Self::validate(request)?.materialize(destination)
    }
}

struct PlannedFile {
    path: String,
    bytes: Vec<u8>,
}

/// 插件隔离源码校验或物化错误。
#[derive(Debug, thiserror::Error)]
pub enum PluginWorkspaceError {
    /// 插件进化协议对象不合法。
    #[error("插件源码协议校验失败：{0}")]
    Protocol(#[from] InvalidPluginEvolution),
    /// 输入条目数超过有界资源限制。
    #[error("插件工作区字段 `{field}` 的条目数 {found} 超过上限 {max}")]
    TooManyEntries {
        /// 输入字段名。
        field: &'static str,
        /// 实际条目数。
        found: usize,
        /// 最大条目数。
        max: usize,
    },
    /// 输入显式声明了符号链接语义。
    #[error("插件工作区拒绝符号链接条目：`{path}`")]
    SymbolicLinkEntry {
        /// 符号链接路径。
        path: String,
    },
    /// 文件不在唯一允许的插件 scope 内。
    #[error("插件路径 `{path}` 不在允许 scope `{scope}` 内")]
    OutsidePluginScope {
        /// 非法路径。
        path: String,
        /// 允许的 scope。
        scope: String,
    },
    /// scope 本身不是安全的相对路径。
    #[error("插件 scope 必须是非空的规范相对路径")]
    InvalidPluginScope,
    /// Parent 文件字节映射没有绑定调用方指定的 Stable Parent 摘要。
    #[error("Parent 源码摘要不匹配：期望 {expected}，实际 {actual}")]
    ParentSourceDigestMismatch {
        /// 调用方指定的受信 Stable Parent 摘要。
        expected: ArtifactDigest,
        /// 从实际 Parent 文件字节重建的摘要。
        actual: ArtifactDigest,
    },
    /// 输入普通文件超过单文件边界。
    #[error("插件文件 `{path}` 大小 {size_bytes} 超过上限 {max_bytes}")]
    FileTooLarge {
        /// 文件路径。
        path: String,
        /// 实际字节数。
        size_bytes: u64,
        /// 最大字节数。
        max_bytes: u64,
    },
    /// 输入映射总体字节数超过边界。
    #[error("插件工作区字段 `{field}` 总大小 {size_bytes} 超过上限 {max_bytes}")]
    InputTooLarge {
        /// 输入字段名。
        field: &'static str,
        /// 实际字节数。
        size_bytes: u64,
        /// 最大字节数。
        max_bytes: u64,
    },
    /// 文件路径存在文件与目录前缀冲突。
    #[error("插件文件路径存在文件/目录前缀冲突：`{path}`")]
    PathConflict {
        /// 冲突路径。
        path: String,
    },
    /// Patch 列表没有按路径严格升序排列。
    #[error("插件补丁必须按路径严格升序且不得重复")]
    UnorderedOrDuplicatePatches,
    /// 补丁试图修改受保护表面。
    #[error("插件补丁不得修改受保护路径：`{path}`")]
    ProtectedPath {
        /// 受保护路径。
        path: String,
    },
    /// Create 的路径已存在于 Parent。
    #[error("Create 补丁目标已存在：`{0}`")]
    CreateTargetExists(String),
    /// Update 或 Delete 的路径不在 Parent 中。
    #[error("Update/Delete 补丁目标不存在：`{0}`")]
    PatchTargetMissing(String),
    /// 补丁旧摘要与 Parent 真实字节错绑。
    #[error("补丁旧摘要与 Parent 文件真实摘要不匹配：`{path}`")]
    OldDigestMismatch {
        /// 补丁路径。
        path: String,
    },
    /// Create/Update 缺少新文件字节。
    #[error("Create/Update 补丁缺少新文件字节：`{0}`")]
    MissingReplacement(String),
    /// 新文件真实字节与补丁新摘要错绑。
    #[error("补丁新摘要与新文件真实摘要不匹配：`{path}`")]
    NewDigestMismatch {
        /// 补丁路径。
        path: String,
    },
    /// 存在没有对应 Create/Update 补丁的多余新文件。
    #[error("新文件字节没有对应补丁：`{0}`")]
    UnusedReplacement(String),
    /// 应用补丁后源码摘要未改变。
    #[error("插件补丁应用后源码树没有变化")]
    UnchangedSource,
    /// 目标专用目录已经存在。
    #[error("插件隔离目录必须不存在：{0}")]
    DestinationExists(PathBuf),
    /// 目标的直接父目录不是安全真实目录。
    #[error("插件隔离目录的父目录必须是已存在的非符号链接目录：{0}")]
    UnsafeDestinationParent(PathBuf),
    /// 文件写入后的摘要与内存计划不符。
    #[error("物化文件回读摘要不匹配：`{path}`")]
    MaterializedDigestMismatch {
        /// 文件路径。
        path: String,
    },
    /// 文件系统操作失败。
    #[error("{operation}失败：{path}: {source}")]
    Io {
        /// 操作名称。
        operation: &'static str,
        /// 目标路径。
        path: PathBuf,
        /// 原始 I/O 错误。
        #[source]
        source: std::io::Error,
    },
    /// 写入失败后无法清理本次创建的专用目录。
    #[error("插件物化失败且无法清理目录 {root}：原始错误：{original}；清理错误：{cleanup}")]
    CleanupFailed {
        /// 本次创建的目录。
        root: PathBuf,
        /// 原始物化错误文本。
        original: String,
        /// 清理错误。
        cleanup: std::io::Error,
    },
    /// Patch Plan 规范序列化失败。
    #[error("序列化插件 Patch Plan 失败：{0}")]
    Serialization(serde_json::Error),
    /// SHA-256 无法转换为强类型摘要。
    #[error("构造插件工作区摘要失败：{0}")]
    DigestConstruction(String),
}

fn validate_scope(scope: &str) -> Result<(), PluginWorkspaceError> {
    if scope == "." || scope.ends_with('/') {
        return Err(PluginWorkspaceError::InvalidPluginScope);
    }
    let probe = PluginSourceFile {
        path: scope.to_string(),
        digest: digest_bytes(&[])?,
        size_bytes: 0,
    };
    probe
        .validate()
        .map_err(|_| PluginWorkspaceError::InvalidPluginScope)?;
    let components = scope
        .split('/')
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    if components.iter().any(|component| {
        matches!(
            component.as_str(),
            ".cargo" | "wit" | "wits" | "sdk" | "agent-plugin" | "agent-plugin-sdk"
        )
    }) {
        return Err(PluginWorkspaceError::InvalidPluginScope);
    }
    Ok(())
}

fn validate_input_count(field: &'static str, count: usize) -> Result<(), PluginWorkspaceError> {
    if count > MAX_PLUGIN_WORKSPACE_FILES {
        return Err(PluginWorkspaceError::TooManyEntries {
            field,
            found: count,
            max: MAX_PLUGIN_WORKSPACE_FILES,
        });
    }
    Ok(())
}

fn validate_entries(
    scope: &str,
    field: &'static str,
    entries: BTreeMap<String, PluginWorkspaceEntry>,
) -> Result<BTreeMap<String, Vec<u8>>, PluginWorkspaceError> {
    let mut total_bytes = 0_u64;
    let mut regular_files = BTreeMap::new();
    for (path, entry) in entries {
        validate_scoped_path(scope, &path)?;
        let bytes = match entry {
            PluginWorkspaceEntry::RegularFile(bytes) => bytes,
            PluginWorkspaceEntry::SymbolicLink { .. } => {
                return Err(PluginWorkspaceError::SymbolicLinkEntry { path })
            }
        };
        let size_bytes = bytes.len() as u64;
        if size_bytes > MAX_PLUGIN_WORKSPACE_FILE_BYTES {
            return Err(PluginWorkspaceError::FileTooLarge {
                path,
                size_bytes,
                max_bytes: MAX_PLUGIN_WORKSPACE_FILE_BYTES,
            });
        }
        total_bytes =
            total_bytes
                .checked_add(size_bytes)
                .ok_or(PluginWorkspaceError::InputTooLarge {
                    field,
                    size_bytes: u64::MAX,
                    max_bytes: MAX_PLUGIN_WORKSPACE_TOTAL_BYTES,
                })?;
        if total_bytes > MAX_PLUGIN_WORKSPACE_TOTAL_BYTES {
            return Err(PluginWorkspaceError::InputTooLarge {
                field,
                size_bytes: total_bytes,
                max_bytes: MAX_PLUGIN_WORKSPACE_TOTAL_BYTES,
            });
        }
        regular_files.insert(path, bytes);
    }
    validate_no_path_conflicts(regular_files.keys().map(String::as_str))?;
    Ok(regular_files)
}

fn validate_scoped_path(scope: &str, path: &str) -> Result<(), PluginWorkspaceError> {
    let probe = PluginSourceFile {
        path: path.to_string(),
        digest: digest_bytes(&[])?,
        size_bytes: 0,
    };
    probe.validate()?;
    let prefix = format!("{scope}/");
    if !path.starts_with(&prefix) || path.len() == prefix.len() {
        return Err(PluginWorkspaceError::OutsidePluginScope {
            path: path.to_string(),
            scope: scope.to_string(),
        });
    }
    Ok(())
}

fn validate_no_path_conflicts<'a>(
    paths: impl Iterator<Item = &'a str>,
) -> Result<(), PluginWorkspaceError> {
    let paths = paths.collect::<Vec<_>>();
    for pair in paths.windows(2) {
        let prefix = format!("{}/", pair[0]);
        if pair[1].starts_with(&prefix) {
            return Err(PluginWorkspaceError::PathConflict {
                path: pair[1].to_string(),
            });
        }
    }
    Ok(())
}

fn validate_patch_order(patches: &[PluginFilePatch]) -> Result<(), PluginWorkspaceError> {
    if patches.len() > MAX_PLUGIN_WORKSPACE_PATCHES {
        return Err(PluginWorkspaceError::TooManyEntries {
            field: "patches",
            found: patches.len(),
            max: MAX_PLUGIN_WORKSPACE_PATCHES,
        });
    }
    if patches.is_empty()
        || patches
            .windows(2)
            .any(|pair| pair[0].path() >= pair[1].path())
    {
        return Err(PluginWorkspaceError::UnorderedOrDuplicatePatches);
    }
    Ok(())
}

fn validate_mutable_path(scope: &str, path: &str) -> Result<(), PluginWorkspaceError> {
    let relative = path
        .strip_prefix(scope)
        .and_then(|value| value.strip_prefix('/'))
        .ok_or_else(|| PluginWorkspaceError::OutsidePluginScope {
            path: path.to_string(),
            scope: scope.to_string(),
        })?;
    let components = relative
        .split('/')
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    let file_name = components.last().map(String::as_str).unwrap_or_default();
    let protected_name = matches!(
        file_name,
        "cargo.toml"
            | "cargo.lock"
            | "build.rs"
            | "plugin.toml"
            | "rust-toolchain"
            | "rust-toolchain.toml"
    );
    let protected_tree = components.iter().any(|component| {
        matches!(
            component.as_str(),
            ".cargo" | "wit" | "wits" | "sdk" | "agent-plugin" | "agent-plugin-sdk"
        )
    });
    let wit_file = file_name.ends_with(".wit");
    if protected_name || protected_tree || wit_file {
        return Err(PluginWorkspaceError::ProtectedPath {
            path: path.to_string(),
        });
    }
    Ok(())
}

fn apply_patch(
    patch: &PluginFilePatch,
    parent: &mut BTreeMap<String, Vec<u8>>,
    replacements: &mut BTreeMap<String, Vec<u8>>,
) -> Result<(), PluginWorkspaceError> {
    match patch {
        PluginFilePatch::Create { path, new_digest } => {
            if parent.contains_key(path) {
                return Err(PluginWorkspaceError::CreateTargetExists(path.clone()));
            }
            let bytes = replacements
                .remove(path)
                .ok_or_else(|| PluginWorkspaceError::MissingReplacement(path.clone()))?;
            verify_digest(path, new_digest, &bytes, false)?;
            parent.insert(path.clone(), bytes);
        }
        PluginFilePatch::Update {
            path,
            old_digest,
            new_digest,
        } => {
            let previous = parent
                .get(path)
                .ok_or_else(|| PluginWorkspaceError::PatchTargetMissing(path.clone()))?;
            verify_digest(path, old_digest, previous, true)?;
            let bytes = replacements
                .remove(path)
                .ok_or_else(|| PluginWorkspaceError::MissingReplacement(path.clone()))?;
            verify_digest(path, new_digest, &bytes, false)?;
            parent.insert(path.clone(), bytes);
        }
        PluginFilePatch::Delete { path, old_digest } => {
            let previous = parent
                .get(path)
                .ok_or_else(|| PluginWorkspaceError::PatchTargetMissing(path.clone()))?;
            verify_digest(path, old_digest, previous, true)?;
            parent.remove(path);
        }
    }
    Ok(())
}

fn verify_digest(
    path: &str,
    expected: &ArtifactDigest,
    bytes: &[u8],
    old: bool,
) -> Result<(), PluginWorkspaceError> {
    if &digest_bytes(bytes)? != expected {
        return if old {
            Err(PluginWorkspaceError::OldDigestMismatch {
                path: path.to_string(),
            })
        } else {
            Err(PluginWorkspaceError::NewDigestMismatch {
                path: path.to_string(),
            })
        };
    }
    Ok(())
}

fn source_artifact(
    plugin_id: &str,
    files: &BTreeMap<String, Vec<u8>>,
) -> Result<PluginSourceArtifact, PluginWorkspaceError> {
    validate_no_path_conflicts(files.keys().map(String::as_str))?;
    let files = files
        .iter()
        .map(|(path, bytes)| {
            Ok(PluginSourceFile {
                path: path.clone(),
                digest: digest_bytes(bytes)?,
                size_bytes: bytes.len() as u64,
            })
        })
        .collect::<Result<Vec<_>, PluginWorkspaceError>>()?;
    Ok(PluginSourceArtifact::new(plugin_id, files)?)
}

#[derive(Serialize)]
struct PatchDigestPayload<'a> {
    domain: &'static str,
    schema_version: u32,
    plugin_id: &'a str,
    plugin_scope: &'a str,
    parent_source_digest: &'a ArtifactDigest,
    source_digest: &'a ArtifactDigest,
    patches: &'a [PluginFilePatch],
}

fn patch_digest(
    plugin_id: &str,
    plugin_scope: &str,
    parent_source_digest: &ArtifactDigest,
    source_digest: &ArtifactDigest,
    patches: &[PluginFilePatch],
) -> Result<ArtifactDigest, PluginWorkspaceError> {
    let bytes = serde_json::to_vec(&PatchDigestPayload {
        domain: "ascnet.lucia.plugin-patch-plan.v1",
        schema_version: PLUGIN_PATCH_PLAN_SCHEMA_VERSION,
        plugin_id,
        plugin_scope,
        parent_source_digest,
        source_digest,
        patches,
    })
    .map_err(PluginWorkspaceError::Serialization)?;
    digest_bytes(&bytes)
}

fn digest_bytes(bytes: &[u8]) -> Result<ArtifactDigest, PluginWorkspaceError> {
    let hex = format!("{:x}", Sha256::digest(bytes));
    ArtifactDigest::from_sha256_hex(hex)
        .map_err(|error| PluginWorkspaceError::DigestConstruction(error.to_string()))
}

fn validate_destination(destination: &Path) -> Result<(), PluginWorkspaceError> {
    match fs::symlink_metadata(destination) {
        Ok(_) => {
            return Err(PluginWorkspaceError::DestinationExists(
                destination.to_path_buf(),
            ))
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => return Err(io_error("检查插件隔离目录", destination, source)),
    }
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let metadata = fs::symlink_metadata(parent)
        .map_err(|source| io_error("检查插件隔离目录父目录", parent, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(PluginWorkspaceError::UnsafeDestinationParent(
            parent.to_path_buf(),
        ));
    }
    Ok(())
}

fn materialize_files(
    destination: &Path,
    files: &[PlannedFile],
) -> Result<(), PluginWorkspaceError> {
    let mut directories = BTreeSet::new();
    for file in files {
        let relative = Path::new(&file.path);
        let mut current = PathBuf::new();
        if let Some(parent) = relative.parent() {
            for component in parent.components() {
                current.push(component.as_os_str());
                directories.insert(destination.join(&current));
            }
        }
    }
    for directory in &directories {
        fs::create_dir(directory)
            .map_err(|source| io_error("创建插件源码子目录", directory, source))?;
    }

    for file in files {
        let path = destination.join(&file.path);
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|source| io_error("创建插件源码文件", &path, source))?;
        output
            .write_all(&file.bytes)
            .map_err(|source| io_error("写入插件源码文件", &path, source))?;
        output
            .sync_all()
            .map_err(|source| io_error("同步插件源码文件", &path, source))?;
    }

    for file in files {
        let path = destination.join(&file.path);
        let metadata = fs::symlink_metadata(&path)
            .map_err(|source| io_error("检查已物化插件源码文件", &path, source))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(PluginWorkspaceError::MaterializedDigestMismatch {
                path: file.path.clone(),
            });
        }
        let bytes =
            fs::read(&path).map_err(|source| io_error("回读插件源码文件", &path, source))?;
        if digest_bytes(&bytes)? != digest_bytes(&file.bytes)? {
            return Err(PluginWorkspaceError::MaterializedDigestMismatch {
                path: file.path.clone(),
            });
        }
    }

    for directory in directories.iter().rev() {
        fs::File::open(directory)
            .and_then(|directory| directory.sync_all())
            .map_err(|source| io_error("同步插件源码子目录", directory, source))?;
    }
    fs::File::open(destination)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| io_error("同步插件隔离目录", destination, source))?;
    Ok(())
}

fn cleanup_after_error(root: &Path, original: PluginWorkspaceError) -> PluginWorkspaceError {
    match fs::remove_dir_all(root) {
        Ok(()) => original,
        Err(cleanup) => PluginWorkspaceError::CleanupFailed {
            root: root.to_path_buf(),
            original: original.to_string(),
            cleanup,
        },
    }
}

fn io_error(
    operation: &'static str,
    path: impl AsRef<Path>,
    source: std::io::Error,
) -> PluginWorkspaceError {
    PluginWorkspaceError::Io {
        operation,
        path: path.as_ref().to_path_buf(),
        source,
    }
}

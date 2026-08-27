//! 原生文件工具的工作区边界与能力门禁。
//!
//! 见 ADR-0001。本模块把 [`FilesystemScope`] 的**声明**变成可强制的路径解析：
//! 所有对外路径都必须先经过 [`WorkspaceGuard`]，解析失败即拒绝访问。
//!
//! 逃逸防护依赖 `canonicalize`：它同时解析 `..` 与 symlink，因此指向工作区外的
//! 符号链接在比较之前就已经被展开成真实路径，无法绕过包含性检查。

use crate::policy::{ExecutionPolicy, FilesystemScope};
use std::{
    fmt,
    path::{Component, Path, PathBuf},
};

/// 单项文件能力。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileCapability {
    /// 读取已有文件或列出目录。
    Read,
    /// 覆盖已有文件。
    Write,
    /// 创建新文件或新目录。
    Create,
    /// 删除已有文件或目录。
    Delete,
}

impl fmt::Display for FileCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Read => "读取",
            Self::Write => "写入",
            Self::Create => "创建",
            Self::Delete => "删除",
        };
        f.write_str(name)
    }
}

/// 工作区内允许的文件能力集合。
///
/// 四项能力单独开关，便于 Evaluation 只开放读取。`Delete` 目前没有对应的内置工具，
/// 唯一的删除路径是 `shell`；保留该能力是为了让策略能完整表达文件权限，
/// 并在写入 Genome 时不丢失维度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FileCapabilities {
    /// 是否允许读取。
    pub read: bool,
    /// 是否允许覆盖已有文件。
    pub write: bool,
    /// 是否允许创建新文件或目录。
    pub create: bool,
    /// 是否允许删除。
    pub delete: bool,
}

impl FileCapabilities {
    /// 只读能力集。
    pub fn read_only() -> Self {
        Self {
            read: true,
            ..Self::default()
        }
    }

    /// 读取、覆盖与创建，但不允许删除；对应内置工具集的实际需求。
    pub fn read_write() -> Self {
        Self {
            read: true,
            write: true,
            create: true,
            delete: false,
        }
    }

    /// 全部能力。
    pub fn all() -> Self {
        Self {
            read: true,
            write: true,
            create: true,
            delete: true,
        }
    }

    /// 判断是否包含指定能力。
    pub fn permits(&self, capability: FileCapability) -> bool {
        match capability {
            FileCapability::Read => self.read,
            FileCapability::Write => self.write,
            FileCapability::Create => self.create,
            FileCapability::Delete => self.delete,
        }
    }

    /// 逐项取逻辑与，结果不会多出任何能力。
    pub fn restrict(&self, requested: &Self) -> Self {
        Self {
            read: self.read && requested.read,
            write: self.write && requested.write,
            create: self.create && requested.create,
            delete: self.delete && requested.delete,
        }
    }
}

/// 路径解析被拒绝的原因。
///
/// 对外文案刻意不包含工作区根路径和解析后的绝对路径，避免模型据此推断可用的
/// 逃逸路径或宿主目录结构。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceError {
    /// 当前策略未授予所需能力。
    Denied(FileCapability),
    /// 目标位于工作区之外，或经 symlink 指向工作区之外。
    Escape,
    /// 策略完全禁止文件访问。
    NoFilesystem,
    /// 路径不存在或无法解析。
    Unresolvable(String),
}

impl fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Denied(capability) => {
                write!(f, "当前执行策略不允许{capability}文件")
            }
            Self::Escape => f.write_str("路径超出允许的工作区范围"),
            Self::NoFilesystem => f.write_str("当前执行策略禁止访问文件系统"),
            Self::Unresolvable(reason) => write!(f, "无法解析路径：{reason}"),
        }
    }
}

impl std::error::Error for WorkspaceError {}

impl WorkspaceError {
    /// 返回可写入原生 [`crate::ToolResult`] 的稳定错误类别。
    ///
    /// 路径不存在等普通解析失败不属于安全事件；权限缺失和路径逃逸保留各自类别，供
    /// Runtime 在确认结果来自原生工具后生成可信 Incident。
    pub const fn tool_error_kind(&self) -> crate::ToolErrorKind {
        match self {
            Self::Denied(_) | Self::NoFilesystem => crate::ToolErrorKind::PermissionDenied,
            Self::Escape => crate::ToolErrorKind::PathBoundaryViolation,
            Self::Unresolvable(_) => crate::ToolErrorKind::Execution,
        }
    }
}

/// 原生文件工具共用的路径守卫。
///
/// 每个工具在触碰文件系统之前都必须调用 [`WorkspaceGuard::resolve_existing`] 或
/// [`WorkspaceGuard::resolve_new`]，并使用返回的规范路径，而不是模型给出的原始字符串。
#[derive(Debug, Clone)]
pub struct WorkspaceGuard {
    /// 已 canonicalize 的工作区根目录；`None` 表示不限制目录。
    root: Option<PathBuf>,
    /// 策略是否完全禁止文件访问。
    denied: bool,
    capabilities: FileCapabilities,
}

impl WorkspaceGuard {
    /// 以指定根目录创建守卫，默认授予读写与创建能力。
    ///
    /// # Errors
    ///
    /// 根目录不存在或无法 canonicalize 时返回错误；调用方应视为配置错误而非运行期失败。
    pub fn rooted(root: impl AsRef<Path>) -> std::io::Result<Self> {
        Ok(Self {
            root: Some(root.as_ref().canonicalize()?),
            denied: false,
            capabilities: FileCapabilities::read_write(),
        })
    }

    /// 创建不限制目录的守卫，仅供需要全盘访问的嵌入方使用。
    ///
    /// 该模式不提供逃逸防护，不应用于 Evaluation 或任何运行不可信内容的场景。
    pub fn unrestricted() -> Self {
        Self {
            root: None,
            denied: false,
            capabilities: FileCapabilities::read_write(),
        }
    }

    /// 创建完全禁止文件访问的守卫。
    pub fn denied() -> Self {
        Self {
            root: None,
            denied: true,
            capabilities: FileCapabilities::default(),
        }
    }

    /// 按执行策略的文件范围构造守卫。
    ///
    /// `Mutation` 与 `Denied` 范围产出拒绝一切访问的守卫；`Root` 范围要求目录已存在。
    ///
    /// # Errors
    ///
    /// 策略声明的根目录无法 canonicalize 时返回错误。
    pub fn from_policy(policy: &ExecutionPolicy) -> std::io::Result<Self> {
        match &policy.filesystem {
            FilesystemScope::Unrestricted => Ok(Self::unrestricted()),
            FilesystemScope::Root(root) => Self::rooted(root),
            FilesystemScope::Denied => Ok(Self::denied()),
        }
    }

    /// 以 builder 风格替换能力集合。
    pub fn with_capabilities(mut self, capabilities: FileCapabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// 返回当前能力集合。
    pub fn capabilities(&self) -> FileCapabilities {
        self.capabilities
    }

    /// 返回工作区根目录；不限制目录时为 `None`。
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// 把外部传入的路径转为绝对路径。
    ///
    /// 相对路径针对工作区根解析，而不是进程当前目录，避免工具行为随进程 cwd 漂移。
    fn absolutize(&self, raw: &str) -> PathBuf {
        let path = Path::new(raw);
        if path.is_absolute() {
            return path.to_path_buf();
        }
        match &self.root {
            Some(root) => root.join(path),
            None => path.to_path_buf(),
        }
    }

    /// 判断目标是否已存在。
    ///
    /// 与 [`WorkspaceGuard::resolve_existing`] 使用同一套相对路径解析规则，
    /// 因此调用方据此选择"覆盖"还是"创建"能力时不会与实际解析结果错位。
    /// 该方法不做能力校验，只用于选择应当校验哪一项能力。
    pub fn exists(&self, raw: &str) -> bool {
        self.absolutize(raw).exists()
    }

    /// 校验解析后的路径确实位于工作区之内。
    fn ensure_contained(&self, resolved: &Path) -> Result<(), WorkspaceError> {
        let Some(root) = &self.root else {
            return Ok(());
        };
        if resolved.starts_with(root) {
            Ok(())
        } else {
            Err(WorkspaceError::Escape)
        }
    }

    /// 检查能力并在缺失时返回拒绝。
    fn ensure_capability(&self, capability: FileCapability) -> Result<(), WorkspaceError> {
        if self.denied {
            return Err(WorkspaceError::NoFilesystem);
        }
        if self.capabilities.permits(capability) {
            Ok(())
        } else {
            Err(WorkspaceError::Denied(capability))
        }
    }

    /// 解析必须已存在的目标，返回可安全使用的规范路径。
    ///
    /// `canonicalize` 会展开 `..` 与 symlink，因此指向工作区外的链接在包含性检查前
    /// 就已还原为真实路径。
    ///
    /// # Errors
    ///
    /// 能力不足、路径不存在，或解析结果落在工作区之外时返回对应的 [`WorkspaceError`]。
    pub fn resolve_existing(
        &self,
        raw: &str,
        capability: FileCapability,
    ) -> Result<PathBuf, WorkspaceError> {
        self.ensure_capability(capability)?;
        let resolved = self
            .absolutize(raw)
            .canonicalize()
            .map_err(|error| WorkspaceError::Unresolvable(error.to_string()))?;
        self.ensure_contained(&resolved)?;
        Ok(resolved)
    }

    /// 解析尚不存在的目标（新建文件或目录），返回可安全使用的路径。
    ///
    /// 逐级上溯到最近的已存在祖先并 canonicalize，再把剩余部分拼回去。剩余部分尚不存在，
    /// 因此不可能是 symlink；其中出现 `..` 会被直接拒绝，避免拼接出逃逸路径。
    ///
    /// # Errors
    ///
    /// 能力不足、路径非法，或最终目标落在工作区之外时返回对应的 [`WorkspaceError`]。
    pub fn resolve_new(
        &self,
        raw: &str,
        capability: FileCapability,
    ) -> Result<PathBuf, WorkspaceError> {
        self.ensure_capability(capability)?;
        let requested = self.absolutize(raw);

        // 已存在的目标直接走 canonicalize 路径，覆盖写入属于这种情况。
        if requested.exists() {
            let resolved = requested
                .canonicalize()
                .map_err(|error| WorkspaceError::Unresolvable(error.to_string()))?;
            self.ensure_contained(&resolved)?;
            return Ok(resolved);
        }

        let mut ancestor = requested.as_path();
        let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
        loop {
            if ancestor.exists() {
                break;
            }
            let (Some(parent), Some(name)) = (ancestor.parent(), ancestor.file_name()) else {
                return Err(WorkspaceError::Unresolvable(
                    "路径没有可解析的已存在父目录".to_string(),
                ));
            };
            tail.push(name);
            ancestor = parent;
        }

        let mut resolved = ancestor
            .canonicalize()
            .map_err(|error| WorkspaceError::Unresolvable(error.to_string()))?;
        for name in tail.iter().rev() {
            // 不存在的部分中出现 `..` 无法由 canonicalize 消解，只能拒绝。
            if Path::new(name)
                .components()
                .any(|component| matches!(component, Component::ParentDir))
            {
                return Err(WorkspaceError::Escape);
            }
            resolved.push(name);
        }

        self.ensure_contained(&resolved)?;
        Ok(resolved)
    }
}

impl Default for WorkspaceGuard {
    fn default() -> Self {
        Self::unrestricted()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// 在系统临时目录下创建一个唯一的测试工作区。
    fn temp_workspace(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "lucia-workspace-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("inside")).expect("应创建工作区");
        fs::write(base.join("inside/file.txt"), "内部文件").expect("应写入内部文件");
        base
    }

    #[test]
    fn rejects_parent_directory_traversal() {
        let base = temp_workspace("traversal");
        let guard = WorkspaceGuard::rooted(base.join("inside")).expect("应创建守卫");

        let error = guard
            .resolve_existing("../", FileCapability::Read)
            .expect_err("上溯到工作区之外必须被拒绝");
        assert_eq!(error, WorkspaceError::Escape);

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn rejects_absolute_path_outside_workspace() {
        let base = temp_workspace("absolute");
        let guard = WorkspaceGuard::rooted(base.join("inside")).expect("应创建守卫");

        let error = guard
            .resolve_existing("/etc/hosts", FileCapability::Read)
            .expect_err("工作区外的绝对路径必须被拒绝");
        assert_eq!(error, WorkspaceError::Escape);

        let _ = fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_pointing_outside_workspace() {
        let base = temp_workspace("symlink");
        let outside = base.join("outside.txt");
        fs::write(&outside, "外部机密").expect("应写入外部文件");
        let link = base.join("inside/escape.txt");
        std::os::unix::fs::symlink(&outside, &link).expect("应创建符号链接");

        let guard = WorkspaceGuard::rooted(base.join("inside")).expect("应创建守卫");
        let error = guard
            .resolve_existing("escape.txt", FileCapability::Read)
            .expect_err("指向工作区外的符号链接必须被拒绝");
        assert_eq!(error, WorkspaceError::Escape);

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn allows_paths_inside_workspace() {
        let base = temp_workspace("inside");
        let guard = WorkspaceGuard::rooted(base.join("inside")).expect("应创建守卫");

        let resolved = guard
            .resolve_existing("file.txt", FileCapability::Read)
            .expect("工作区内的相对路径应被放行");
        assert!(resolved.ends_with("file.txt"));

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn resolve_new_allows_nested_creation_inside_workspace() {
        let base = temp_workspace("create");
        let guard = WorkspaceGuard::rooted(base.join("inside")).expect("应创建守卫");

        let resolved = guard
            .resolve_new("nested/deep/new.txt", FileCapability::Create)
            .expect("工作区内的新建路径应被放行");
        assert!(resolved.starts_with(base.join("inside").canonicalize().expect("根应可解析")));

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn resolve_new_rejects_traversal_in_missing_segments() {
        let base = temp_workspace("create-escape");
        let guard = WorkspaceGuard::rooted(base.join("inside")).expect("应创建守卫");

        let error = guard
            .resolve_new("../outside-new.txt", FileCapability::Create)
            .expect_err("新建路径同样不允许逃逸");
        assert_eq!(error, WorkspaceError::Escape);

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn missing_capability_is_rejected_before_touching_disk() {
        let base = temp_workspace("capability");
        let guard = WorkspaceGuard::rooted(base.join("inside"))
            .expect("应创建守卫")
            .with_capabilities(FileCapabilities::read_only());

        let error = guard
            .resolve_new("new.txt", FileCapability::Create)
            .expect_err("只读守卫不应允许创建");
        assert_eq!(error, WorkspaceError::Denied(FileCapability::Create));

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn denied_guard_blocks_every_access() {
        let guard = WorkspaceGuard::denied();

        assert_eq!(
            guard
                .resolve_existing("/etc/hosts", FileCapability::Read)
                .expect_err("禁止访问时必须拒绝"),
            WorkspaceError::NoFilesystem
        );
    }

    #[test]
    fn guard_follows_execution_policy_scope() {
        let base = temp_workspace("policy");
        let policy = ExecutionPolicy::evaluation(base.join("inside"));
        let guard = WorkspaceGuard::from_policy(&policy).expect("应按策略创建守卫");

        assert!(guard.root().is_some());
        assert_eq!(
            guard
                .resolve_existing("/etc/hosts", FileCapability::Read)
                .expect_err("Evaluation 不应读取工作区外文件"),
            WorkspaceError::Escape
        );

        let _ = fs::remove_dir_all(&base);
    }
}

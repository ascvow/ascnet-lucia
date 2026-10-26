//! 原生文件工具的工作区边界与能力门禁。
//!
//! 见 ADR-0001。本模块把 [`FilesystemScope`] 的**声明**变成可强制的路径解析：
//! 所有对外路径都必须先经过 [`WorkspaceGuard`]，解析失败即拒绝访问。
//!
//! 逃逸防护依赖 `canonicalize`：它同时解析 `..` 与 symlink，因此指向工作区外的
//! 符号链接在比较之前就已经被展开成真实路径，无法绕过包含性检查。

use crate::policy::{ExecutionPolicy, FilesystemScope};
use std::{
    collections::HashSet,
    fmt,
    path::{Component, Path, PathBuf},
    sync::{Arc, RwLock},
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
/// 并在序列化时不丢失维度。
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
    /// 目标命中凭据、私钥或版本库内部路径。
    Sensitive,
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
            Self::Sensitive => f.write_str("路径指向受保护的凭据或版本库内部文件"),
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
            Self::Denied(_) | Self::NoFilesystem | Self::Sensitive => {
                crate::ToolErrorKind::PermissionDenied
            }
            Self::Escape => crate::ToolErrorKind::PathBoundaryViolation,
            Self::Unresolvable(_) => crate::ToolErrorKind::Execution,
        }
    }
}

/// 用户明确引用、允许只读访问的工作区外路径集合。
///
/// 只由宿主在用户提交消息时写入，模型与 Guest 无法自行扩充。集合保存 `canonicalize`
/// 之后的绝对路径，因此比较时不会再次展开符号链接；授权后目标若被替换成指向别处的
/// 链接，也不会命中旧授权。
#[derive(Debug, Clone, Default)]
pub struct ReadPathGrants {
    paths: Arc<RwLock<HashSet<PathBuf>>>,
}

/// 沿已规范化绝对路径逐段打开，Unix 上每段都拒绝符号链接并用目录句柄锚定下一段。
///
/// 此函数不执行业务授权；调用者必须先用工作区/manifest 策略确认路径。返回只读常规文件，
/// 特殊文件、点路径或任一符号链接返回错误。Windows 回退为前后元数据校验，不承诺同等竞态隔离。
/// 同 UID 的恶意目录重命名、硬链接或挂载替换仍需操作系统权限边界。
pub fn open_canonical_file_read(path: &Path) -> std::io::Result<std::fs::File> {
    let invalid = || {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "读取路径必须是规范化绝对常规文件",
        )
    };
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
    {
        return Err(invalid());
    }
    #[cfg(unix)]
    {
        use std::{
            ffi::CString,
            os::{
                fd::{AsRawFd, FromRawFd},
                unix::ffi::OsStrExt,
            },
        };
        let mut parent = std::fs::File::open("/")?;
        let parts = path
            .components()
            .filter_map(|part| {
                if let Component::Normal(name) = part {
                    Some(name)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        if parts.is_empty() {
            return Err(invalid());
        }
        for (index, part) in parts.iter().enumerate() {
            let name = CString::new(part.as_bytes()).map_err(|_| invalid())?;
            let directory = index + 1 < parts.len();
            let flags = libc::O_RDONLY
                | libc::O_CLOEXEC
                | libc::O_NOFOLLOW
                | libc::O_NONBLOCK
                | libc::O_NOCTTY
                | if directory { libc::O_DIRECTORY } else { 0 };
            // SAFETY: parent 是活跃文件描述符，name 为有效 CString；成功返回的 fd 独占转交 File。
            let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: fd 是本次 openat 新建且尚未转移所有权的描述符。
            parent = unsafe { std::fs::File::from_raw_fd(fd) };
        }
        if !parent.metadata()?.is_file() {
            return Err(invalid());
        }
        Ok(parent)
    }
    #[cfg(not(unix))]
    {
        let metadata = std::fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(invalid());
        }
        let file = std::fs::File::open(path)?;
        if !file.metadata()?.is_file() {
            return Err(invalid());
        }
        Ok(file)
    }
}

const MAX_READ_GRANTS: usize = 256;

impl ReadPathGrants {
    /// 创建空的只读授权集合。
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一个已存在的路径，返回规范化后的绝对路径。
    ///
    /// 文件与目录都可以登记；路径不存在或无法解析时返回 IO 错误。超过上限后新路径
    /// 被静默忽略，调用方不需要把授权失败当作消息发送失败。
    pub fn grant(&self, path: impl AsRef<Path>) -> std::io::Result<PathBuf> {
        let resolved = path.as_ref().canonicalize()?;
        let mut paths = self
            .paths
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if paths.len() < MAX_READ_GRANTS {
            paths.insert(resolved.clone());
        }
        Ok(resolved)
    }

    /// 提交已在后台规范化的用户只读授权；此处只做集合插入，不再访问文件系统。
    ///
    /// 本入口仅供可信应用使用，调用者必须先验证并规范化路径；不得接收模型自报路径。
    pub fn grant_canonical(&self, resolved: PathBuf) -> std::io::Result<()> {
        if !resolved.is_absolute()
            || resolved
                .components()
                .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "授权路径必须已规范化",
            ));
        }
        let mut paths = self
            .paths
            .write()
            .unwrap_or_else(|error| error.into_inner());
        if paths.len() < MAX_READ_GRANTS {
            paths.insert(resolved);
        }
        Ok(())
    }

    /// 判断规范化路径是否已被用户授权。
    pub fn contains(&self, resolved: &Path) -> bool {
        self.paths
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(resolved)
    }

    /// 清空全部授权；切换会话时必须调用。
    pub fn clear(&self) {
        self.paths
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }

    /// 返回当前授权数量。
    pub fn len(&self) -> usize {
        self.paths
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    /// 判断当前没有任何授权。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
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
    /// 用户显式引用、允许只读访问的工作区外路径；只对读取能力生效。
    read_grants: Option<ReadPathGrants>,
    /// 可信应用注入的额外目录及其能力上限，保存规范路径并覆盖后代。
    extra_roots: Vec<(PathBuf, FileCapabilities)>,
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
            read_grants: None,
            extra_roots: Vec::new(),
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
            read_grants: None,
            extra_roots: Vec::new(),
        }
    }

    /// 创建完全禁止文件访问的守卫。
    pub fn denied() -> Self {
        Self {
            root: None,
            denied: true,
            capabilities: FileCapabilities::default(),
            read_grants: None,
            extra_roots: Vec::new(),
        }
    }

    /// 附加用户显式引用的只读路径集合。
    ///
    /// 此授权只放宽读取，不改变写入、创建与删除的既有目录范围。
    pub fn with_read_grants(mut self, grants: ReadPathGrants) -> Self {
        self.read_grants = Some(grants);
        self
    }

    /// 附加可信应用指定的只读目录，允许读取该目录及其后代。
    ///
    /// `root` 必须由应用配置或用户授权确定，不得直接采用模型或 Guest 自报的授权范围。
    /// 返回保存了规范目录路径的守卫；访问时仍校验读取能力、敏感路径和符号链接真实目标，
    /// 不放宽写入、创建或删除，也不受会话级 [`ReadPathGrants::clear`] 影响。
    ///
    /// # Errors
    ///
    /// 目录不存在、无法规范化或目标不是目录时返回 IO 错误；不会创建目录。
    pub fn with_read_root(self, root: impl AsRef<Path>) -> std::io::Result<Self> {
        self.with_allowed_root(root, FileCapabilities::read_only())
    }

    /// 附加可信应用授权的目录及其能力上限，允许访问该目录及其后代。
    ///
    /// `root` 必须由可信应用确定，不能直接采用模型或 Guest 自报的范围。`capabilities`
    /// 仅限制此目录的授权，不会恢复守卫全局已禁用的能力；敏感路径和真实目标检查仍然生效。
    /// 本方法不改变默认工作目录，也不创建目录或修改会话级只读授权。
    ///
    /// # Errors
    ///
    /// 目录不存在、无法规范化或目标不是目录时返回 IO 错误。
    pub fn with_allowed_root(
        mut self,
        root: impl AsRef<Path>,
        capabilities: FileCapabilities,
    ) -> std::io::Result<Self> {
        let resolved = root.as_ref().canonicalize()?;
        if !resolved.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "授权根路径必须是目录",
            ));
        }
        let grant = (resolved, capabilities);
        if !self.extra_roots.contains(&grant) {
            self.extra_roots.push(grant);
        }
        Ok(self)
    }

    /// Shell 的工作目录不继承额外路径授权，避免文件授权扩大进程工作目录。
    #[cfg(feature = "builtins")]
    pub(crate) fn without_path_overrides(mut self) -> Self {
        self.read_grants = None;
        self.extra_roots.clear();
        self
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

    /// 校验目标位于工作区、具有所需能力的额外目录内，或已被用户显式授权读取。
    ///
    /// 额外目录只能提供显式授予的能力；用户消息中的路径授权仍仅对读取生效。
    fn ensure_contained(
        &self,
        resolved: &Path,
        capability: FileCapability,
    ) -> Result<(), WorkspaceError> {
        let Some(root) = &self.root else {
            return Ok(());
        };
        if resolved.starts_with(root) {
            return Ok(());
        }
        if self.extra_roots.iter().any(|(root, capabilities)| {
            capabilities.permits(capability) && resolved.starts_with(root)
        }) {
            return Ok(());
        }
        if capability == FileCapability::Read
            && self
                .read_grants
                .as_ref()
                .is_some_and(|grants| grants.contains(resolved))
        {
            return Ok(());
        }
        Err(WorkspaceError::Escape)
    }

    /// 按规范路径检查敏感文件；有根守卫只检查根内相对部分，避免父目录名称误伤。
    fn ensure_not_sensitive(&self, resolved: &Path) -> Result<(), WorkspaceError> {
        let relative = self
            .root
            .as_ref()
            .and_then(|root| resolved.strip_prefix(root).ok())
            .unwrap_or(resolved);
        if is_sensitive_path(relative) {
            Err(WorkspaceError::Sensitive)
        } else {
            Ok(())
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
    /// 能力不足、路径不存在，或解析结果不在获准目录范围内时返回对应的 [`WorkspaceError`]。
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
        self.ensure_contained(&resolved, capability)?;
        self.ensure_not_sensitive(&resolved)?;
        Ok(resolved)
    }

    /// 解析尚不存在的目标（新建文件或目录），返回可安全使用的路径。
    ///
    /// 逐级上溯到最近的已存在祖先并 canonicalize，再把剩余部分拼回去。剩余部分尚不存在，
    /// 因此不可能是 symlink；其中出现 `..` 会被直接拒绝，避免拼接出逃逸路径。
    ///
    /// # Errors
    ///
    /// 能力不足、路径非法，或最终目标不在获准目录范围内时返回对应的 [`WorkspaceError`]。
    pub fn resolve_new(
        &self,
        raw: &str,
        capability: FileCapability,
    ) -> Result<PathBuf, WorkspaceError> {
        self.ensure_capability(capability)?;
        let requested = self.absolutize(raw);

        let mut ancestor = requested.as_path();
        let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
        loop {
            // 悬空链接也属于已存在条目，必须交给 canonicalize 拒绝；不能当作缺失后缀
            // 拼回路径，否则新建工具可能跟随链接写入授权目录之外。
            match ancestor.symlink_metadata() {
                Ok(_) => break,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(WorkspaceError::Unresolvable(error.to_string())),
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

        self.ensure_contained(&resolved, capability)?;
        self.ensure_not_sensitive(&resolved)?;
        Ok(resolved)
    }
}

/// 判断路径是否包含常见密钥、身份、凭据或版本库内部文件名。
pub fn is_sensitive_path(path: &Path) -> bool {
    path.components().any(|component| {
        let Component::Normal(segment) = component else {
            return false;
        };
        let name = segment.to_string_lossy().to_ascii_lowercase();
        name == ".git"
            || name == ".ssh"
            || name == ".gnupg"
            || name == ".aws"
            || name == ".config"
            || name == "credentials"
            || name == "id_rsa"
            || name == "id_ed25519"
            || name.starts_with(".env")
            || name.ends_with(".pem")
            || name.ends_with(".key")
            || name.ends_with(".p12")
    })
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

    /// 校验后父目录被替换为外部符号链接时，逐段 openat 必须拒绝而不是跟随。
    #[cfg(unix)]
    #[test]
    fn canonical_open_rejects_parent_substitution() {
        use std::io::Read;
        use std::os::unix::fs::symlink;
        let base = temp_workspace("parent-open").canonicalize().unwrap();
        fs::create_dir(base.join("outside")).unwrap();
        fs::write(base.join("outside/file.txt"), "secret").unwrap();
        let resolved = base.join("inside/file.txt").canonicalize().unwrap();
        let mut file = open_canonical_file_read(&resolved).unwrap();
        fs::rename(base.join("inside"), base.join("moved")).unwrap();
        symlink(base.join("outside"), base.join("inside")).unwrap();
        assert!(open_canonical_file_read(&resolved).is_err());
        let mut data = String::new();
        file.read_to_string(&mut data).unwrap();
        assert_eq!(data, "内部文件", "已打开句柄不随路径替换改变");
        assert!(open_canonical_file_read(Path::new("relative")).is_err());
        assert!(open_canonical_file_read(&base).is_err());
        fs::remove_dir_all(base).unwrap();
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

    /// 指向工作区内敏感文件的无害别名仍必须按规范目标拒绝。
    #[cfg(unix)]
    #[test]
    fn rejects_symlink_alias_to_sensitive_file() {
        let base = temp_workspace("sensitive-symlink");
        let sensitive = base.join("inside/.env.local");
        fs::write(&sensitive, "TOKEN=secret").expect("应创建敏感测试文件");
        let alias = base.join("inside/settings.txt");
        std::os::unix::fs::symlink(&sensitive, &alias).expect("应创建敏感文件别名");
        let guard = WorkspaceGuard::rooted(base.join("inside")).expect("应创建守卫");

        let error = guard
            .resolve_existing("settings.txt", FileCapability::Read)
            .expect_err("敏感文件别名必须拒绝");

        assert_eq!(error, WorkspaceError::Sensitive);
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

    /// 应用只读目录覆盖后代，但不扩大到父目录、同名前缀目录、敏感文件或写入能力。
    #[test]
    fn read_roots_allow_descendants_without_widening_other_access() {
        let base = temp_workspace("read-roots");
        let resources = base.join("resources");
        fs::create_dir_all(resources.join("skills/demo")).unwrap();
        let skill = resources.join("skills/demo/SKILL.md");
        fs::write(&skill, "skill instructions").unwrap();
        fs::write(resources.join(".env"), "SECRET=test").unwrap();
        fs::create_dir(base.join("resources-other")).unwrap();
        let grants = ReadPathGrants::new();
        let guard = WorkspaceGuard::rooted(base.join("inside"))
            .unwrap()
            .with_capabilities(FileCapabilities::all())
            .with_read_root(&resources)
            .unwrap()
            .with_read_grants(grants.clone());
        grants.clear();

        for path in [&resources, &resources.join("skills"), &skill] {
            assert_eq!(
                guard.resolve_existing(path.to_str().unwrap(), FileCapability::Read),
                Ok(path.canonicalize().unwrap()),
                "应用目录授权不随会话清空"
            );
        }
        for path in [&base, &base.join("resources-other"), &resources.join("..")] {
            assert_eq!(
                guard.resolve_existing(path.to_str().unwrap(), FileCapability::Read),
                Err(WorkspaceError::Escape)
            );
        }
        for capability in [FileCapability::Write, FileCapability::Delete] {
            assert_eq!(
                guard.resolve_existing(skill.to_str().unwrap(), capability),
                Err(WorkspaceError::Escape)
            );
        }
        assert_eq!(
            guard.resolve_new(skill.to_str().unwrap(), FileCapability::Write),
            Err(WorkspaceError::Escape)
        );
        assert_eq!(
            guard.resolve_new(
                resources.join("new/file.txt").to_str().unwrap(),
                FileCapability::Create,
            ),
            Err(WorkspaceError::Escape)
        );
        assert_eq!(
            guard.resolve_existing(
                resources.join(".env").to_str().unwrap(),
                FileCapability::Read,
            ),
            Err(WorkspaceError::Sensitive)
        );
        fs::remove_dir_all(base).unwrap();
    }

    /// 只读根必须已经是目录，且不能恢复策略已拒绝的读取能力。
    #[test]
    fn read_roots_validate_directories_and_preserve_capability_denials() {
        let base = temp_workspace("read-root-capability");
        let guard = WorkspaceGuard::rooted(base.join("inside")).unwrap();
        assert!(guard.clone().with_read_root(base.join("missing")).is_err());
        assert!(guard
            .clone()
            .with_read_root(base.join("inside/file.txt"))
            .is_err());
        let denied_read = guard
            .with_read_root(&base)
            .unwrap()
            .with_capabilities(FileCapabilities::default());
        assert_eq!(
            denied_read.resolve_existing(base.to_str().unwrap(), FileCapability::Read),
            Err(WorkspaceError::Denied(FileCapability::Read))
        );
        let denied = WorkspaceGuard::denied().with_read_root(&base).unwrap();
        assert_eq!(
            denied.resolve_existing(base.to_str().unwrap(), FileCapability::Read),
            Err(WorkspaceError::NoFilesystem)
        );
        fs::remove_dir_all(base).unwrap();
    }

    /// 授权根的规范路径固定；后代链接或授权后替换根目录都不能指向未授权内容。
    #[cfg(unix)]
    #[test]
    fn read_roots_reject_symlink_escape_and_root_substitution() {
        use std::os::unix::fs::symlink;
        let base = temp_workspace("read-root-symlink");
        let resources = base.join("resources");
        let outside = base.join("outside");
        fs::create_dir(&resources).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("file.txt"), "outside").unwrap();
        fs::write(resources.join(".env"), "SECRET=test").unwrap();
        symlink(&outside, resources.join("escape")).unwrap();
        symlink(resources.join(".env"), resources.join("alias.txt")).unwrap();
        let guard = WorkspaceGuard::rooted(base.join("inside"))
            .unwrap()
            .with_read_root(&resources)
            .unwrap();
        assert_eq!(
            guard.resolve_existing(
                resources.join("escape/file.txt").to_str().unwrap(),
                FileCapability::Read,
            ),
            Err(WorkspaceError::Escape)
        );
        assert_eq!(
            guard.resolve_existing(
                resources.join("alias.txt").to_str().unwrap(),
                FileCapability::Read,
            ),
            Err(WorkspaceError::Sensitive)
        );
        fs::rename(&resources, base.join("moved")).unwrap();
        symlink(&outside, &resources).unwrap();
        assert_eq!(
            guard.resolve_existing(
                resources.join("file.txt").to_str().unwrap(),
                FileCapability::Read,
            ),
            Err(WorkspaceError::Escape)
        );
        fs::remove_dir_all(base).unwrap();
    }

    /// 额外可写目录覆盖后代，但不放行同前缀兄弟、父目录和敏感文件。
    #[test]
    fn allowed_roots_preserve_path_and_capability_boundaries() {
        let base = temp_workspace("allowed-roots");
        let temporary = base.join("scratch");
        let sibling = base.join("scratch-other");
        fs::create_dir_all(&temporary).unwrap();
        fs::create_dir_all(&sibling).unwrap();
        let file = temporary.join("result.log");
        fs::write(&file, "temporary result").unwrap();
        fs::write(sibling.join("result.log"), "outside").unwrap();
        let guard = WorkspaceGuard::rooted(base.join("inside"))
            .unwrap()
            .with_allowed_root(&temporary, FileCapabilities::read_write())
            .unwrap();
        for capability in [FileCapability::Read, FileCapability::Write] {
            assert_eq!(
                guard
                    .resolve_existing(file.to_str().unwrap(), capability)
                    .unwrap(),
                file.canonicalize().unwrap()
            );
            assert_eq!(
                guard.resolve_existing(sibling.join("result.log").to_str().unwrap(), capability),
                Err(WorkspaceError::Escape)
            );
        }
        assert_eq!(
            guard
                .resolve_new(
                    temporary.join("nested/new.log").to_str().unwrap(),
                    FileCapability::Create,
                )
                .unwrap(),
            temporary.canonicalize().unwrap().join("nested/new.log")
        );
        assert_eq!(
            guard.resolve_new(
                temporary.join("../outside.log").to_str().unwrap(),
                FileCapability::Create,
            ),
            Err(WorkspaceError::Escape)
        );
        for sensitive in [".env", ".ssh/id_rsa", "credentials", "private.pem"] {
            let path = temporary.join(sensitive);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "secret").unwrap();
            for capability in [
                FileCapability::Read,
                FileCapability::Write,
                FileCapability::Create,
            ] {
                assert_eq!(
                    guard.resolve_new(path.to_str().unwrap(), capability),
                    Err(WorkspaceError::Sensitive)
                );
            }
        }

        let read_only = guard
            .clone()
            .with_capabilities(FileCapabilities::read_only());
        assert_eq!(
            read_only.resolve_existing(file.to_str().unwrap(), FileCapability::Write),
            Err(WorkspaceError::Denied(FileCapability::Write))
        );
        assert_eq!(
            read_only.resolve_new(
                temporary.join("new.log").to_str().unwrap(),
                FileCapability::Create
            ),
            Err(WorkspaceError::Denied(FileCapability::Create))
        );
        let create_only = WorkspaceGuard::rooted(base.join("inside"))
            .unwrap()
            .with_allowed_root(
                &temporary,
                FileCapabilities {
                    create: true,
                    ..FileCapabilities::default()
                },
            )
            .unwrap();
        for capability in [FileCapability::Read, FileCapability::Write] {
            assert_eq!(
                create_only.resolve_existing(file.to_str().unwrap(), capability),
                Err(WorkspaceError::Escape)
            );
        }
        assert!(create_only
            .resolve_new(
                temporary.join("new.log").to_str().unwrap(),
                FileCapability::Create
            )
            .is_ok());
        let denied = WorkspaceGuard::denied()
            .with_allowed_root(&temporary, FileCapabilities::read_write())
            .unwrap();
        assert_eq!(
            denied.resolve_existing(file.to_str().unwrap(), FileCapability::Read),
            Err(WorkspaceError::NoFilesystem)
        );
        assert!(guard
            .clone()
            .with_allowed_root(&file, FileCapabilities::read_write())
            .is_err());
        assert!(guard
            .with_allowed_root(base.join("missing"), FileCapabilities::read_write())
            .is_err());
        fs::remove_dir_all(base).unwrap();
    }

    /// 可写临时目录内的已有链接、悬空链接和目录替换均不得扩大授权范围。
    #[cfg(unix)]
    #[test]
    fn allowed_roots_reject_symlink_escapes_including_dangling_targets() {
        use std::os::unix::fs::symlink;
        let base = temp_workspace("allowed-root-links");
        let temporary = base.join("scratch");
        let outside = base.join("outside");
        fs::create_dir_all(&temporary).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("file.log"), "outside").unwrap();
        symlink(&outside, temporary.join("escape")).unwrap();
        symlink(outside.join("new.log"), temporary.join("dangling.log")).unwrap();
        symlink(outside.join("missing"), temporary.join("dangling-dir")).unwrap();
        let guard = WorkspaceGuard::rooted(base.join("inside"))
            .unwrap()
            .with_allowed_root(&temporary, FileCapabilities::read_write())
            .unwrap();
        for capability in [FileCapability::Read, FileCapability::Write] {
            assert_eq!(
                guard.resolve_existing(
                    temporary.join("escape/file.log").to_str().unwrap(),
                    capability
                ),
                Err(WorkspaceError::Escape)
            );
        }
        assert_eq!(
            guard.resolve_new(
                temporary.join("escape/new.log").to_str().unwrap(),
                FileCapability::Create
            ),
            Err(WorkspaceError::Escape)
        );
        for path in ["dangling.log", "dangling-dir/new.log"] {
            assert!(matches!(
                guard.resolve_new(
                    temporary.join(path).to_str().unwrap(),
                    FileCapability::Create
                ),
                Err(WorkspaceError::Unresolvable(_))
            ));
        }
        fs::write(temporary.join("safe.log"), "temporary").unwrap();
        symlink(temporary.join("safe.log"), temporary.join("alias.log")).unwrap();
        assert!(guard
            .resolve_new(
                temporary.join("alias.log").to_str().unwrap(),
                FileCapability::Write
            )
            .is_ok());
        fs::rename(&temporary, base.join("moved")).unwrap();
        symlink(&outside, &temporary).unwrap();
        assert_eq!(
            guard.resolve_new(
                temporary.join("new.log").to_str().unwrap(),
                FileCapability::Create
            ),
            Err(WorkspaceError::Escape)
        );
        fs::remove_dir_all(base).unwrap();
    }

    /// 用户显式登记的外部路径只放宽读取：写入仍被拒绝，未登记与敏感路径也不放行。
    #[test]
    fn read_grants_allow_only_explicit_external_reads() {
        let base = temp_workspace("grants");
        let outside = base.join("outside.txt");
        fs::write(&outside, "外部内容").expect("应写入外部文件");
        let grants = ReadPathGrants::new();
        let granted = grants.grant(&outside).expect("应登记外部路径");
        assert!(grants.contains(&granted));
        assert_eq!(grants.len(), 1);

        let guard = WorkspaceGuard::rooted(base.join("inside"))
            .expect("应创建守卫")
            .with_read_grants(grants);
        assert_eq!(
            guard
                .resolve_existing(
                    outside.to_str().expect("测试路径应为 UTF-8"),
                    FileCapability::Read,
                )
                .expect("已登记路径应可读"),
            granted
        );
        assert_eq!(
            guard
                .resolve_existing("/etc/hosts", FileCapability::Read)
                .expect_err("未登记的外部路径必须拒绝"),
            WorkspaceError::Escape
        );
        assert_eq!(
            guard
                .resolve_new(
                    outside.to_str().expect("测试路径应为 UTF-8"),
                    FileCapability::Write,
                )
                .expect_err("授权不得放宽写入"),
            WorkspaceError::Escape
        );

        // 敏感文件即使被登记也不放行。
        let sensitive = base.join("outside.pem");
        fs::write(&sensitive, "TOKEN").expect("应写入敏感文件");
        let grants = ReadPathGrants::new();
        grants.grant(&sensitive).expect("登记本身应成功");
        let guard = WorkspaceGuard::rooted(base.join("inside"))
            .expect("应创建守卫")
            .with_read_grants(grants);
        assert_eq!(
            guard
                .resolve_existing(
                    sensitive.to_str().expect("测试路径应为 UTF-8"),
                    FileCapability::Read,
                )
                .expect_err("敏感文件必须拒绝"),
            WorkspaceError::Sensitive
        );

        let _ = fs::remove_dir_all(&base);
    }
}

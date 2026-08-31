//! 插件锁文件与安装记录的稳定数据结构。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
/// 当前 `plugins.lock.toml` 的格式版本。
///
/// v2 持久化用户确认的完整授权 profile 摘要；v1 的启用位会在读取时安全降级为待重新审查。
pub const LOCK_SCHEMA_VERSION: u32 = 2;

/// 插件锁文件名。
pub const LOCK_FILE_NAME: &str = "plugins.lock.toml";

/// 插件安装结果和锁文件中的持久化记录。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstalledPlugin {
    /// 插件稳定 ID。
    pub id: String,
    /// 插件显示名称。
    pub name: String,
    /// 已安装的语义化版本。
    pub version: String,
    /// 插件 ABI 版本。
    pub api_version: String,
    /// 插件当前是否启用。
    pub enabled: bool,
    /// 相对于管理根目录的 manifest 路径。
    pub manifest: String,
    /// bundle 的 SHA-256 摘要。
    pub sha256: String,
    /// 本地 bundle 绝对路径或不可变 GitHub Release 来源。
    pub source: String,
}

impl InstalledPlugin {
    /// 返回用户审批必须绑定的不可变 Bundle 身份。
    ///
    /// 启用操作在提交前应再次比较该身份，避免插件 ID 未变但来源、版本或内容已被替换。
    #[must_use]
    pub fn bundle_identity(&self) -> PluginBundleIdentity {
        PluginBundleIdentity {
            source: self.source.clone(),
            version: self.version.clone(),
            sha256: self.sha256.clone(),
        }
    }
}

/// 用户确认时看到的不可变插件 Bundle 身份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginBundleIdentity {
    /// Bundle 的安装来源；本地路径与远端 Release 均按锁文件中的规范值精确比较。
    pub source: String,
    /// 用户确认时看到的插件版本。
    pub version: String,
    /// 用户确认时完整 Bundle 的 SHA-256 摘要。
    pub sha256: String,
}

/// 插件锁文件的可序列化结构。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginLock {
    /// 锁文件格式版本。
    pub schema_version: u32,
    /// 当前受管理的插件记录。
    #[serde(default)]
    pub plugins: Vec<InstalledPlugin>,
    /// 已归档但不参与运行时加载的人工安装版本历史。
    ///
    /// 该字段只支持人工回滚，不表示自动升级链路。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub version_history: Vec<InstalledPlugin>,
    /// 插件 ID 到用户最后确认的完整授权 profile SHA-256。
    ///
    /// 只有当前启用插件允许出现在该映射中；禁用、替换、移除和人工回滚都会撤销对应摘要。
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub authorization_grants: HashMap<String, String>,
    /// 独占能力 ID 到启用插件 ID 的选择。
    #[serde(default)]
    pub capability_selection: HashMap<String, String>,
}

impl Default for PluginLock {
    fn default() -> Self {
        Self {
            schema_version: LOCK_SCHEMA_VERSION,
            plugins: Vec::new(),
            version_history: Vec::new(),
            authorization_grants: HashMap::new(),
            capability_selection: HashMap::new(),
        }
    }
}

/// 安装行为选项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstallOptions {
    /// 是否请求以启用状态提交 Bundle。
    ///
    /// 公开人工安装入口只接受 `false`；人工替换即使收到 `true` 也会保持禁用并要求重新审查。
    /// 该标记仅供 Manager 内部可信迁移在重新核验来源、身份、能力和依赖后受限继承授权。
    pub enabled: bool,
}

impl Default for InstallOptions {
    fn default() -> Self {
        // 安装只验证并落盘 Bundle；启用必须是展示 capability 后的独立用户动作。
        Self { enabled: false }
    }
}

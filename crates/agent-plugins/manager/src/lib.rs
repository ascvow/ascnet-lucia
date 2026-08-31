//! Lucia 插件的可信获取、安装、状态管理和完整性诊断。
//!
//! 管理器负责可信来源获取和本地 bundle 状态，但不负责实例化插件。

#![deny(missing_docs)]

mod bundle;
mod doctor;
mod github;
mod manager;
mod registry;
#[cfg(test)]
mod tests;
mod types;

pub use bundle::{hash_plugin_bundle, pack_plugin_bundle, unpack_plugin_bundle};
pub use doctor::{DoctorIssue, DoctorReport, DoctorSeverity};
pub use github::{
    check_github_connectivity, GithubInstallOptions, GithubInstallResult, GithubPluginSource,
    DEFAULT_GITHUB_PUBLISHER,
};
pub use manager::PluginManager;
pub use registry::{
    RegistryInstallResult, RegistryOutdatedPlugin, RegistryRequest, RegistrySearchResult,
    RegistryUpdateResult,
};
pub use types::{
    InstallOptions, InstalledPlugin, PluginBundleIdentity, PluginLock, LOCK_FILE_NAME,
    LOCK_SCHEMA_VERSION,
};

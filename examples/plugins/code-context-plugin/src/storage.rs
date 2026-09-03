//! 索引持久化。
//!
//! 按 `docs/code-context-engine/adr-storage.md` 的分层决策：**只有体量受控的元数据**
//! 走宿主的 JSON KV 持久存储，chunk、倒排索引与向量不放在这里。宿主存储当前是单个
//! JSON 文件、8 MiB 上限、每次写入重写全量，装不下索引本体。
//!
//! 存储位置由宿主按插件 ID 与作用域派生，插件不能指定磁盘路径，因此索引不会污染
//! 用户仓库。`Workspace` 作用域天然实现按项目隔离。
//!
//! # 键命名
//!
//! 全部键使用 `index.` 前缀，为将来的块存储迁移留出独立命名空间。当前只有
//! [`MANIFEST_KEY`] 一个键；Phase 2 起会增加文件哈希表，Phase 10 起向量迁往块存储。

use crate::error::CodeContextError;
use crate::model::IndexManifest;
use agent_plugin::{PluginHostApi, PluginStoreScope};

/// 保存索引 manifest 的存储键。
const MANIFEST_KEY: &str = "index.manifest";

/// 一次 manifest 读取的结果。
///
/// 「读不出来」是正常业务分支而不是错误：损坏或版本过高的 manifest 应当让引擎进入
/// 重建状态，而不是让插件加载失败或让宿主收到一个异常。
#[derive(Debug)]
pub enum ManifestLoad {
    /// 该仓库还没有 manifest。
    Missing,
    /// 成功读取并通过校验。
    Loaded(Box<IndexManifest>),
    /// manifest 存在但不可用，附带具体原因。
    Unreadable(CodeContextError),
}

/// 索引元数据存储。
///
/// 只封装寻址与序列化，不理解索引语义。后续迁移到块存储时替换实现即可，
/// 领域模型与生命周期代码不需要改动。
#[derive(Debug, Clone, Copy)]
pub struct IndexStore {
    /// 项目作用域是否可用。
    available: bool,
}

impl IndexStore {
    /// 探测宿主是否提供了按项目隔离的持久存储。
    ///
    /// 宿主未注入项目作用域时返回不可用，而不是退回用户级作用域——那会让一个项目的
    /// 索引泄漏到另一个项目。探测用一次 `store_list` 完成，它在作用域缺失时失败。
    pub fn probe(host: &dyn PluginHostApi) -> Self {
        Self {
            available: host.store_list(PluginStoreScope::Workspace).is_ok(),
        }
    }

    /// 返回项目作用域存储是否可用。
    pub fn is_available(self) -> bool {
        self.available
    }

    /// 读取当前项目的索引 manifest。
    ///
    /// 存储不可用时返回 [`ManifestLoad::Missing`]：调用方据此进入未索引状态，
    /// 与「这个项目还没建过索引」的处理路径一致。
    pub fn load_manifest(self, host: &dyn PluginHostApi) -> ManifestLoad {
        if !self.available {
            return ManifestLoad::Missing;
        }
        let stored = match host.store_get(PluginStoreScope::Workspace, MANIFEST_KEY) {
            Ok(Some(value)) => value,
            Ok(None) => return ManifestLoad::Missing,
            Err(error) => {
                return ManifestLoad::Unreadable(CodeContextError::Storage(error.to_string()))
            }
        };
        let manifest: IndexManifest = match serde_json::from_value(stored) {
            Ok(manifest) => manifest,
            Err(error) => {
                return ManifestLoad::Unreadable(CodeContextError::InvalidManifest(
                    error.to_string(),
                ))
            }
        };
        match manifest.validate() {
            Ok(()) => ManifestLoad::Loaded(Box::new(manifest)),
            Err(error) => ManifestLoad::Unreadable(error),
        }
    }

    /// 保存索引 manifest。
    ///
    /// 先校验再写入，避免把自相矛盾的 manifest 落盘——下次启动读到它只会进入重建，
    /// 等于白白丢掉一次索引。
    ///
    /// # Errors
    ///
    /// 存储不可用、manifest 校验失败或宿主写入失败时返回错误。
    pub fn save_manifest(
        self,
        host: &dyn PluginHostApi,
        manifest: &IndexManifest,
    ) -> Result<(), CodeContextError> {
        if !self.available {
            return Err(CodeContextError::CapabilityUnavailable(
                "宿主未提供项目作用域持久存储，索引无法保存".into(),
            ));
        }
        manifest.validate()?;
        let value = serde_json::to_value(manifest)
            .map_err(|error| CodeContextError::Storage(error.to_string()))?;
        host.store_set(PluginStoreScope::Workspace, MANIFEST_KEY, &value)
            .map_err(|error| CodeContextError::Storage(error.to_string()))
    }

    /// 删除当前项目的索引 manifest。
    ///
    /// 用于全量重建前清理，保证不会出现「manifest 说有索引、实际没有」的中间态。
    ///
    /// # Errors
    ///
    /// 宿主删除失败时返回错误；存储不可用时视为已清理。
    pub fn clear_manifest(self, host: &dyn PluginHostApi) -> Result<(), CodeContextError> {
        if !self.available {
            return Ok(());
        }
        host.store_remove(PluginStoreScope::Workspace, MANIFEST_KEY)
            .map(|_| ())
            .map_err(|error| CodeContextError::Storage(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RepositoryIdentity;
    use crate::testing::TestHost;

    /// 构造测试用仓库身份。
    fn identity(root: &str) -> RepositoryIdentity {
        RepositoryIdentity::from_workspace_root(root).expect("测试工作区根应有效")
    }

    /// manifest 必须能写入并在重新读取后完全一致。
    #[test]
    fn manifest_round_trips_through_store() {
        let host = TestHost::new();
        let store = IndexStore::probe(&host);
        assert!(store.is_available());

        let manifest = IndexManifest::new(identity("/repo/a"));
        store
            .save_manifest(&host, &manifest)
            .expect("保存 manifest");

        match store.load_manifest(&host) {
            ManifestLoad::Loaded(loaded) => assert_eq!(*loaded, manifest),
            other => panic!("应读回已保存的 manifest，实际为 {other:?}"),
        }
    }

    /// 尚未建立索引的项目必须读到 Missing，而不是错误。
    #[test]
    fn absent_manifest_reports_missing() {
        let host = TestHost::new();
        let store = IndexStore::probe(&host);
        assert!(matches!(store.load_manifest(&host), ManifestLoad::Missing));
    }

    /// 损坏的 manifest 必须降级为 Unreadable，不能让调用方拿到异常。
    #[test]
    fn corrupted_manifest_is_unreadable() {
        let host = TestHost::new();
        host.seed_workspace(
            MANIFEST_KEY,
            serde_json::json!({"schema_version": "不是数字"}),
        );

        let store = IndexStore::probe(&host);
        assert!(matches!(
            store.load_manifest(&host),
            ManifestLoad::Unreadable(CodeContextError::InvalidManifest(_))
        ));
    }

    /// 未来版本的 manifest 必须报告为版本不支持，供调用方进入全量重建。
    #[test]
    fn future_version_manifest_is_unreadable_with_version_reason() {
        let host = TestHost::new();
        let mut manifest = IndexManifest::new(identity("/repo/a"));
        manifest.schema_version = 999;
        host.seed_workspace(
            MANIFEST_KEY,
            serde_json::to_value(&manifest).expect("序列化未来版本 manifest"),
        );

        let store = IndexStore::probe(&host);
        assert!(matches!(
            store.load_manifest(&host),
            ManifestLoad::Unreadable(CodeContextError::UnsupportedManifestVersion { .. })
        ));
    }

    /// 宿主未提供项目作用域时必须判定为不可用，并拒绝写入而不是改写用户级存储。
    #[test]
    fn missing_workspace_scope_disables_persistence() {
        let host = TestHost::without_workspace_scope();
        let store = IndexStore::probe(&host);

        assert!(!store.is_available());
        assert!(matches!(store.load_manifest(&host), ManifestLoad::Missing));

        let manifest = IndexManifest::new(identity("/repo/a"));
        assert!(matches!(
            store.save_manifest(&host, &manifest),
            Err(CodeContextError::CapabilityUnavailable(_))
        ));
        assert!(
            host.user_keys().is_empty(),
            "项目存储不可用时不得把项目索引写进用户级作用域"
        );
    }

    /// 校验失败的 manifest 不得落盘。
    #[test]
    fn invalid_manifest_is_not_persisted() {
        let host = TestHost::new();
        let store = IndexStore::probe(&host);
        let mut manifest = IndexManifest::new(identity("/repo/a"));
        manifest.created_at = 100;
        manifest.updated_at = 50;

        assert!(store.save_manifest(&host, &manifest).is_err());
        assert!(matches!(store.load_manifest(&host), ManifestLoad::Missing));
    }

    /// 清理后必须回到未索引状态。
    #[test]
    fn clearing_manifest_returns_to_missing() {
        let host = TestHost::new();
        let store = IndexStore::probe(&host);
        store
            .save_manifest(&host, &IndexManifest::new(identity("/repo/a")))
            .expect("保存 manifest");

        store.clear_manifest(&host).expect("清理 manifest");
        assert!(matches!(store.load_manifest(&host), ManifestLoad::Missing));
    }
}

//! Code Context Engine 插件。
//!
//! 为 Agent 提供 `code_search`：按自然语言或标识符定位仓库中的代码，替代多轮
//! `grep` 与读文件。设计依据见 `docs/code-context-engine/`。
//!
//! # 当前阶段
//!
//! **骨架阶段。** 已经建立的是生命周期、仓库身份、索引 manifest 持久化、配置、
//! 存储边界与工具契约；**没有任何索引实现**。`code_search` 在索引就绪前一律返回
//! 明确的不可用状态，不返回任何伪造结果。
//!
//! # 能力边界
//!
//! 本插件只申请 `workspace_read` 与 `storage`。工作区读取由 Host 注入可信根并限制为只读，
//! 权限下限仍为 `read`。插件刻意不申请进程、网络与会话能力——那些会抬高工具权限，
//! 使 `code_search` 每次调用都需要额外审批。详见 `plugin.toml` 中逐项说明。
//!
//! # 并发与取消
//!
//! 宿主对同一插件的全部导出调用串行执行（单 Store + 独占锁），因此插件内部不需要任何
//! 锁或并发原语。宿主也不提供调用级取消：已进入 component 的调用一定会跑到返回。
//! 长任务的唯一可行形态是**按 fuel 预算分片**——单次调用消耗到阈值就保存游标返回，
//! 下次调用继续。该游标协议属于 Phase 2 的扫描器，不在骨架阶段预先发明。

mod config;
mod embedding;
mod error;
mod model;
mod parser;
mod repository;
mod storage;
mod tool;

#[cfg(test)]
mod testing;

use agent_plugin::{
    export_plugin, ActivationContext, AgentPlugin, EventPresentation, EventPresentationTone,
    ExtensionEvent, PluginHostApi, Result, ToolCall, ToolResult, ToolSpec,
};
use serde_json::json;

pub use config::{CodeContextConfig, EmbeddingMode};
pub use embedding::{EmbeddingInputKind, EmbeddingProvider, UnconfiguredEmbeddingProvider};
pub use error::CodeContextError;
pub use model::{
    CodeChunk, EmbeddingSpace, IndexManifest, IndexStatus, ManifestCompatibility,
    RepositoryIdentity, Symbol, SymbolKind, INDEX_SCHEMA_VERSION, STRUCTURAL_INDEX_VERSION,
};
pub use parser::{parse_file, Declaration, ParseOutput, ParseTier, ParsedFile};
pub use repository::ignore::{IgnoreRuleSet, IgnoreStack, AGENT_IGNORE_FILE, GIT_IGNORE_FILE};
pub use repository::language::{classify, Language, PathClass, SkipReason};
pub use repository::{
    content_hash, hash_index, IndexableFile, RepositoryScan, ScanBudget, ScanLimits, ScanProgress,
    ScanStats, ScanWarning, ScanWarningKind,
};
pub use storage::{IndexStore, ManifestLoad};
pub use tool::{CodeSearchRequest, TOOL_NAME};

/// 宿主注入工作区根的激活元数据键。
const WORKSPACE_ROOT_KEY: &str = "workspace_root";

/// 插件发布状态变化使用的扩展事件名。
const STATUS_EVENT: &str = "code-context.status";

/// 已附着仓库的运行时上下文。
///
/// 宿主当前没有仓库打开/切换事件：工作区在进程启动时确定，换项目等于重启进程
/// （见 `architecture.md` 第 11 问）。附着与解除因此各只发生一次，但仍然建模为显式
/// 生命周期——将来宿主若引入仓库事件，接入点已经在这里。
#[derive(Debug)]
struct RepositoryContext {
    /// 当前仓库的稳定身份。
    identity: RepositoryIdentity,
    /// 索引元数据存储。
    store: IndexStore,
    /// 当前仓库的索引 manifest。
    manifest: IndexManifest,
    /// manifest 是否有尚未落盘的改动。
    dirty: bool,
}

/// Code Context Engine 插件。
#[derive(Default)]
pub struct CodeContextPlugin {
    /// 解析后的插件配置。
    config: CodeContextConfig,
    /// 当前附着的仓库；未附着时为 `None`。
    repository: Option<RepositoryContext>,
    /// 索引生命周期状态。
    index_status: IndexStatus,
    /// 引擎不可用时面向用户的原因。
    unavailable_reason: Option<String>,
    /// Host 返回的 `code_search` 公开名称；存在时表示工具已经注册给 Agent。
    registered_tool_name: Option<String>,
}

impl CodeContextPlugin {
    /// 返回当前索引状态。
    pub fn index_status(&self) -> IndexStatus {
        self.index_status
    }

    /// 返回当前仓库身份；未附着仓库时为 `None`。
    pub fn repository_identity(&self) -> Option<&RepositoryIdentity> {
        self.repository.as_ref().map(|context| &context.identity)
    }

    /// 返回引擎不可用的原因。
    pub fn unavailable_reason(&self) -> Option<&str> {
        self.unavailable_reason.as_deref()
    }

    /// 返回当前生效的嵌入 provider。
    ///
    /// 两种模式的真实实现都尚未存在，因此这里始终返回未配置 provider。返回类型保持
    /// trait 对象，使 Phase 9 与 Phase 12 接入真实实现时调用方不需要改动。
    pub fn embedding_provider(&self) -> Box<dyn EmbeddingProvider> {
        Box::new(UnconfiguredEmbeddingProvider::new(
            self.config.embedding_mode,
        ))
    }

    /// 把当前仓库附着到引擎。
    ///
    /// 完整流程：解析仓库身份 → 探测插件存储命名空间 → 读取索引 manifest →
    /// 判断兼容性 → 设置索引状态。**不执行任何索引构建。**
    ///
    /// # Errors
    ///
    /// 工作区根缺失或存储写入失败时返回错误。调用方负责决定这是否应当阻止插件加载。
    fn attach_repository(
        &mut self,
        host: &dyn PluginHostApi,
        workspace_root: &str,
    ) -> std::result::Result<(), CodeContextError> {
        let identity = RepositoryIdentity::from_workspace_root(workspace_root)?;
        let store = IndexStore::probe(host);

        let (mut manifest, mut status, mut dirty) = match store.load_manifest(host) {
            ManifestLoad::Loaded(existing) => {
                let expected_space = self.embedding_provider().space().map(|space| space.id());
                let compatibility = existing.compatibility(&identity, expected_space.as_deref());
                match compatibility {
                    // 结构索引产物尚未实现：即使 manifest 兼容，磁盘上也没有可检索的
                    // 数据，报告 Ready 会让 code_search 去搜一个空索引。Phase 3 落地
                    // 真实分块后，这里直接改用 compatibility.to_status()。
                    ManifestCompatibility::Compatible => {
                        (*existing, IndexStatus::NotIndexed, false)
                    }
                    ManifestCompatibility::FullRebuildRequired => (
                        IndexManifest::new(identity.clone()),
                        IndexStatus::NeedsFullRebuild,
                        true,
                    ),
                    ManifestCompatibility::SemanticRebuildRequired => {
                        (*existing, IndexStatus::NeedsSemanticRebuild, false)
                    }
                }
            }
            // 损坏或版本过高的 manifest 不是错误路径：丢弃它并进入全量重建，
            // 让引擎能自愈，而不是让插件加载失败或让宿主收到异常。
            ManifestLoad::Unreadable(_) => (
                IndexManifest::new(identity.clone()),
                IndexStatus::NeedsFullRebuild,
                true,
            ),
            ManifestLoad::Missing => (
                IndexManifest::new(identity.clone()),
                IndexStatus::NotIndexed,
                true,
            ),
        };

        if dirty && store.is_available() {
            manifest.touch();
            store.save_manifest(host, &manifest)?;
            dirty = false;
        }
        // 宿主未提供项目作用域时索引无法持久化，但引擎仍可在内存中工作；
        // 这里保留 dirty 标记，使状态如实反映"这份 manifest 没有落盘"。
        if !store.is_available() {
            status = IndexStatus::NotIndexed;
        }

        self.repository = Some(RepositoryContext {
            identity,
            store,
            manifest,
            dirty,
        });
        self.index_status = status;
        Ok(())
    }

    /// 解除仓库附着并尽力落盘未保存的元数据。
    ///
    /// **这不是保存点。** 宿主在加载被取消或进程异常退出时不保证调用 `deactivate`，
    /// 因此 manifest 在产生时就已经写入；这里只处理内存中尚未落盘的剩余改动。
    fn detach_repository(
        &mut self,
        host: &dyn PluginHostApi,
    ) -> std::result::Result<(), CodeContextError> {
        let Some(mut context) = self.repository.take() else {
            return Ok(());
        };
        self.index_status = IndexStatus::NotIndexed;
        if context.dirty && context.store.is_available() {
            context.manifest.touch();
            context.store.save_manifest(host, &context.manifest)?;
        }
        Ok(())
    }

    /// 注册 `code_search`。
    ///
    /// 采用动态注册而不是静态 `list_tools`：静态工具在 `activate` 之前就被收集，
    /// 那时还读不到配置，无法根据 `enabled` 与仓库可用性决定是否暴露工具。
    fn register_tool(&mut self, host: &dyn PluginHostApi) -> Result<()> {
        let public_name = host.upsert_tool(TOOL_NAME, &tool::spec())?;
        self.registered_tool_name = Some(public_name);
        Ok(())
    }

    /// 注销当前激活实例动态注册的工具。
    ///
    /// Host 可能重写公开工具名，因此必须使用 [`PluginHostApi::upsert_tool`] 的返回值，
    /// 不能假定它永远等于插件内部名称。
    fn unregister_tool(&mut self, host: &dyn PluginHostApi) -> Result<()> {
        let Some(public_name) = self.registered_tool_name.take() else {
            return Ok(());
        };
        host.remove_tool(&public_name)
    }

    /// 发布一条状态事件，让用户在启动摘要中看到引擎当前处境。
    fn emit_status(&self, host: &dyn PluginHostApi) -> Result<()> {
        let (text, tone) = match (&self.unavailable_reason, self.index_status) {
            (Some(reason), _) => (
                format!("Code Context: 不可用（{reason}）"),
                EventPresentationTone::Warning,
            ),
            (None, status) => (
                format!("Code Context: {}", status.as_str()),
                EventPresentationTone::Info,
            ),
        };
        host.emit_event(&ExtensionEvent {
            name: STATUS_EVENT.into(),
            data: json!({
                "status": self.index_status.as_str(),
                "tool_registered": self.registered_tool_name.is_some(),
                "embedding_mode": self.config.embedding_mode.as_str(),
                "reason": self.unavailable_reason,
                // 仓库身份是摘要而不是路径，可以安全地出现在事件与日志里，
                // 同时让宿主侧测试与诊断能确认引擎附着到了哪个仓库。
                "repository_identity": self
                    .repository_identity()
                    .map(RepositoryIdentity::as_str),
            }),
            presentation: Some(EventPresentation::footer(text, tone)),
        })
    }

    /// 执行一次 `code_search` 调用。
    ///
    /// 索引尚未建立，因此这里只做参数校验并如实报告状态。参数校验现在就做，是为了让
    /// 工具契约从第一天起就是稳定的：模型学到的调用形态不会在索引落地后改变。
    fn search(&mut self, call: ToolCall) -> Result<ToolResult> {
        let request = match CodeSearchRequest::parse(&call.args) {
            Ok(request) => request,
            Err(error) => return Ok(ToolResult::error(call.id, call.name, error.to_string())),
        };
        // 请求已通过校验但无法服务；保留解析结果，使参数错误与索引缺失在返回值中可区分。
        let _ = request;

        if !self.config.enabled {
            return Ok(tool::unavailable_result(
                &call,
                self.index_status,
                "Code Context Engine 已在配置中禁用",
            ));
        }
        if let Some(reason) = self.unavailable_reason.clone() {
            return Ok(tool::unavailable_result(&call, self.index_status, &reason));
        }
        if !self.index_status.can_serve_search() {
            return Ok(tool::unavailable_result(
                &call,
                self.index_status,
                "代码索引尚未建立；本插件当前仅完成骨架，请改用常规检索工具",
            ));
        }
        // 状态可服务却没有检索实现，说明状态机与实现进度不一致，必须如实报告而不是
        // 返回空结果——空结果会被模型读成"仓库里没有相关代码"。
        Ok(tool::unavailable_result(
            &call,
            self.index_status,
            "检索实现尚未接入",
        ))
    }
}

impl AgentPlugin for CodeContextPlugin {
    /// 解析配置、附着仓库并注册工具。
    ///
    /// 缺少工作区身份或存储不是加载失败：引擎会进入明确的不可用状态并把原因发布出去，
    /// 而不是阻止整个应用启动。只有配置本身非法才拒绝加载——那是用户能立刻修好的问题。
    fn activate(&mut self, host: &dyn PluginHostApi, context: ActivationContext) -> Result<()> {
        self.config = CodeContextConfig::from_metadata(&context.metadata)?;
        self.unavailable_reason = None;

        if !self.config.enabled {
            self.index_status = IndexStatus::NotIndexed;
            self.unavailable_reason = Some("已在配置中禁用".into());
            return self.emit_status(host);
        }

        self.index_status = IndexStatus::Initializing;
        let workspace_root = context
            .metadata
            .get(WORKSPACE_ROOT_KEY)
            .map(String::as_str)
            .unwrap_or_default();
        if workspace_root.trim().is_empty() {
            self.index_status = IndexStatus::Error;
            self.unavailable_reason = Some(CodeContextError::RepositoryUnavailable.to_string());
            return self.emit_status(host);
        }
        // 只列出仓库根一次来验证 Host 已提供 manifest 声明的可信读取边界；本阶段不会
        // 递归扫描，也不会读取任何源码文件。
        if let Err(error) = host.list_dir(".") {
            self.index_status = IndexStatus::Error;
            self.unavailable_reason = Some(
                CodeContextError::CapabilityUnavailable(format!(
                    "无法访问当前工作区根目录：{error}"
                ))
                .to_string(),
            );
            return self.emit_status(host);
        }
        if let Err(error) = self.attach_repository(host, workspace_root) {
            self.index_status = IndexStatus::Error;
            self.unavailable_reason = Some(error.to_string());
            return self.emit_status(host);
        }

        self.register_tool(host)?;
        self.emit_status(host)
    }

    /// 注销动态工具、落盘未保存的元数据并释放仓库上下文。
    ///
    /// 没有子进程、后台任务或 Agent 资源需要释放：本插件不申请这些能力。
    fn deactivate(&mut self, host: &dyn PluginHostApi) -> Result<()> {
        // 两项清理必须都执行：工具注销失败不能阻止 manifest flush，存储失败也不能让
        // 动态工具继续暴露。聚合错误后交给 Host 记录诊断。
        let unregister_result = self.unregister_tool(host);
        let detach_result = self.detach_repository(host);
        self.unavailable_reason = None;
        match (unregister_result, detach_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(tool_error), Ok(())) => Err(tool_error),
            (Ok(()), Err(storage_error)) => Err(storage_error.into()),
            (Err(tool_error), Err(storage_error)) => Err(anyhow::anyhow!(
                "Code Context 卸载清理失败：工具注销：{tool_error:#}；仓库解除：{storage_error}"
            )),
        }
    }

    /// 不使用静态工具注册。
    ///
    /// `list_tools` 在 `activate` 之前被调用，那时配置与仓库可用性都还未知。
    /// 本插件在 `activate` 中动态注册，见 [`CodeContextPlugin::register_tool`]。
    fn list_tools(&self) -> Vec<ToolSpec> {
        Vec::new()
    }

    fn call_tool_with_host(
        &mut self,
        _host: &dyn PluginHostApi,
        call: ToolCall,
    ) -> Result<ToolResult> {
        if call.name != TOOL_NAME {
            return Ok(ToolResult::error(
                call.id,
                call.name,
                "Code Context Engine 未实现该工具",
            ));
        }
        self.search(call)
    }
}

export_plugin!(CodeContextPlugin);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{activation_context, TestHost};
    use serde_json::Value;

    /// 激活一个附着到指定工作区的插件。
    fn activated(host: &TestHost, workspace_root: &str) -> CodeContextPlugin {
        let mut plugin = CodeContextPlugin::default();
        plugin
            .activate(host, activation_context(workspace_root))
            .expect("激活插件");
        plugin
    }

    /// 构造一次 code_search 调用。
    fn search_call(args: Value) -> ToolCall {
        ToolCall::new("call-1", TOOL_NAME, args)
    }

    /// 激活后必须建立仓库身份、注册工具并进入未索引状态。
    #[test]
    fn activation_attaches_repository_and_registers_tool() {
        let host = TestHost::new();
        let plugin = activated(&host, "/repo/a");

        assert!(plugin.repository_identity().is_some());
        assert_eq!(plugin.index_status(), IndexStatus::NotIndexed);
        assert!(plugin.unavailable_reason().is_none());
        assert_eq!(host.registered_tools(), vec![TOOL_NAME.to_string()]);
        assert_eq!(host.emitted_events(), vec![STATUS_EVENT.to_string()]);
    }

    /// 首次附着必须写入 manifest，使重启后能识别同一仓库。
    #[test]
    fn activation_persists_manifest() {
        let host = TestHost::new();
        let plugin = activated(&host, "/repo/a");

        let store = IndexStore::probe(&host);
        match store.load_manifest(&host) {
            ManifestLoad::Loaded(manifest) => {
                assert_eq!(
                    Some(&manifest.repository_identity),
                    plugin.repository_identity()
                );
            }
            other => panic!("激活后应存在 manifest，实际为 {other:?}"),
        }
    }

    /// 重启插件后必须恢复同一份 manifest，而不是重新创建。
    #[test]
    fn manifest_survives_plugin_restart() {
        let host = TestHost::new();
        let mut first = activated(&host, "/repo/a");
        let created_at = match IndexStore::probe(&host).load_manifest(&host) {
            ManifestLoad::Loaded(manifest) => manifest.created_at,
            other => panic!("首次激活应写入 manifest，实际为 {other:?}"),
        };
        first.deactivate(&host).expect("卸载插件");

        let restored_host = TestHost::restored_from(host.snapshot());
        let second = activated(&restored_host, "/repo/a");

        match IndexStore::probe(&restored_host).load_manifest(&restored_host) {
            ManifestLoad::Loaded(manifest) => {
                assert_eq!(manifest.created_at, created_at, "重启不得重建 manifest");
                assert_eq!(
                    Some(&manifest.repository_identity),
                    second.repository_identity()
                );
            }
            other => panic!("重启后应恢复 manifest，实际为 {other:?}"),
        }
    }

    /// 不同仓库必须得到互相隔离的身份与 manifest。
    #[test]
    fn repositories_stay_isolated() {
        let host_a = TestHost::new();
        let host_b = TestHost::new();
        let plugin_a = activated(&host_a, "/repo/a");
        let plugin_b = activated(&host_b, "/repo/b");

        assert_ne!(
            plugin_a.repository_identity(),
            plugin_b.repository_identity()
        );

        // 同一份存储被另一个仓库附着时，必须判定为需要全量重建而不是复用。
        let cross_host = TestHost::restored_from(host_a.snapshot());
        let cross = activated(&cross_host, "/repo/b");
        assert_eq!(cross.index_status(), IndexStatus::NeedsFullRebuild);
    }

    /// 损坏的 manifest 必须进入全量重建，且不能让激活失败。
    #[test]
    fn corrupted_manifest_enters_full_rebuild() {
        let host = TestHost::new();
        host.seed_workspace("index.manifest", json!({"schema_version": "损坏"}));

        let plugin = activated(&host, "/repo/a");
        assert_eq!(plugin.index_status(), IndexStatus::NeedsFullRebuild);
        assert!(plugin.unavailable_reason().is_none());
    }

    /// 未来 schema 版本必须进入全量重建，而不是让插件加载失败。
    #[test]
    fn future_schema_version_enters_full_rebuild() {
        let host = TestHost::new();
        let mut manifest =
            IndexManifest::new(RepositoryIdentity::from_workspace_root("/repo/a").expect("身份"));
        manifest.schema_version = 999;
        host.seed_workspace(
            "index.manifest",
            serde_json::to_value(&manifest).expect("序列化"),
        );

        let plugin = activated(&host, "/repo/a");
        assert_eq!(plugin.index_status(), IndexStatus::NeedsFullRebuild);
    }

    /// 宿主未注入工作区身份时必须优雅降级，而不是阻止插件加载。
    #[test]
    fn missing_workspace_root_degrades_gracefully() {
        let host = TestHost::new();
        let mut plugin = CodeContextPlugin::default();
        let mut context = activation_context("/repo/a");
        context.metadata.remove(WORKSPACE_ROOT_KEY);

        plugin
            .activate(&host, context)
            .expect("缺少工作区身份不应阻止加载");

        assert_eq!(plugin.index_status(), IndexStatus::Error);
        assert!(plugin.unavailable_reason().is_some());
        assert!(host.registered_tools().is_empty(), "不可用时不应注册工具");
    }

    /// Manifest 声明了工作区读取能力，但 Host 未提供实现时必须优雅降级。
    #[test]
    fn missing_workspace_read_capability_degrades_gracefully() {
        let host = TestHost::without_workspace_read();
        let mut plugin = CodeContextPlugin::default();

        plugin
            .activate(&host, activation_context("/repo/a"))
            .expect("缺少工作区读取能力不应导致插件 panic");

        assert_eq!(plugin.index_status(), IndexStatus::Error);
        assert!(plugin
            .unavailable_reason()
            .is_some_and(|reason| reason.contains("工作区")));
        assert!(host.registered_tools().is_empty());
    }

    /// 宿主未提供项目作用域存储时仍应加载，并如实报告未索引。
    #[test]
    fn missing_workspace_storage_degrades_gracefully() {
        let host = TestHost::without_workspace_scope();
        let plugin = activated(&host, "/repo/a");

        assert_eq!(plugin.index_status(), IndexStatus::NotIndexed);
        assert!(host.user_keys().is_empty(), "不得把项目索引写进用户级存储");
    }

    /// 配置禁用时不得注册工具。
    #[test]
    fn disabled_plugin_registers_no_tool() {
        let host = TestHost::new();
        let mut plugin = CodeContextPlugin::default();
        let mut context = activation_context("/repo/a");
        context.metadata.insert("enabled".into(), "false".into());

        plugin.activate(&host, context).expect("禁用配置应正常加载");

        assert!(host.registered_tools().is_empty());
        assert!(plugin.unavailable_reason().is_some());
    }

    /// 非法配置必须阻止加载，让用户立刻看到问题。
    #[test]
    fn invalid_configuration_blocks_activation() {
        let host = TestHost::new();
        let mut plugin = CodeContextPlugin::default();
        let mut context = activation_context("/repo/a");
        context
            .metadata
            .insert("embedding_mode".into(), "cloud".into());

        assert!(plugin.activate(&host, context).is_err());
    }

    /// 索引未建立时 code_search 必须返回明确的不可用状态。
    #[test]
    fn search_reports_not_indexed() {
        let host = TestHost::new();
        let mut plugin = activated(&host, "/repo/a");

        let result = plugin
            .call_tool_with_host(&host, search_call(json!({"query": "权限在哪里检查"})))
            .expect("工具调用不应失败");

        assert!(result.is_error, "不可用必须是错误结果，避免被读成没有匹配");
        let details = result.details.expect("应带结构化细节");
        assert_eq!(details["status"], IndexStatus::NotIndexed.as_str());
        assert_eq!(details["results"], json!([]));
    }

    /// 参数错误必须与索引缺失区分开。
    #[test]
    fn search_rejects_blank_query() {
        let host = TestHost::new();
        let mut plugin = activated(&host, "/repo/a");

        let result = plugin
            .call_tool_with_host(&host, search_call(json!({"query": "  "})))
            .expect("工具调用不应失败");

        assert!(result.is_error);
        assert!(result.details.is_none(), "参数错误不应带索引状态细节");
    }

    /// 未知工具名必须安全返回错误。
    #[test]
    fn unknown_tool_is_rejected() {
        let host = TestHost::new();
        let mut plugin = activated(&host, "/repo/a");

        let result = plugin
            .call_tool_with_host(&host, ToolCall::new("call-1", "other_tool", json!({})))
            .expect("未知工具不应 panic");
        assert!(result.is_error);
    }

    /// 骨架阶段绝不能返回任何伪造向量。
    #[test]
    fn embedding_provider_is_unavailable() {
        let host = TestHost::new();
        let plugin = activated(&host, "/repo/a");

        let provider = plugin.embedding_provider();
        assert!(provider.space().is_none());
        assert!(provider
            .embed(EmbeddingInputKind::Query, &["x".to_string()])
            .is_err());
    }

    /// 卸载后必须释放仓库上下文并回到未索引状态。
    #[test]
    fn deactivate_releases_repository() {
        let host = TestHost::new();
        let mut plugin = activated(&host, "/repo/a");

        plugin.deactivate(&host).expect("卸载插件");

        assert!(plugin.repository_identity().is_none());
        assert_eq!(plugin.index_status(), IndexStatus::NotIndexed);
        assert!(host.registered_tools().is_empty());
    }
}

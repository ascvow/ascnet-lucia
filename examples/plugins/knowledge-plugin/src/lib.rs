//! 为 Agent 提供可检索的历史会话线索。
//!
//! 插件不调用模型，也不生成新的语义摘要。它按“用户消息到下一条用户消息之前”切分
//! 历史会话，保存会话标题、用户请求、最后助手回复、工具名、原文证据与会话时间。
//! Agent 在用户提到“继续”“上次”“之前”或怀疑任务曾处理过时，自行决定是否检索。
//!
//! # 与上下文压缩的边界
//!
//! 上下文压缩负责控制当前模型请求的上下文体积，本插件只索引会话存储已经持久化的消息
//! 投影，不复制压缩器，也不再次调用模型。历史命中只是线索，继续工作前仍需核对当前代码。

mod search;

use agent_plugin::{
    export_plugin, ActivationContext, AgentPlugin, EventOutcome, PluginEvent, PluginHostApi,
    PluginSessionMessage, PluginSessionSummary, PluginStoreScope, PromptContribution, Result,
    ToolCall, ToolPermissionScope, ToolRenderRequest, ToolRenderState, ToolRendererContribution,
    ToolResult, ToolSpec, UiColor, UiDeclaration, UiFrame, UiInput, UiInputEvent, UiLine,
    UiPlacement, UiRenderRequest, UiSize, UiSpan, UiStyle,
};
use anyhow::{anyhow, Context};
use search::{search, searchable_text};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

/// 持久存储中保存索引元数据的键。
const META_STORE_KEY: &str = "meta";
/// 持久存储中保存历史片段的键。
const ENTRIES_STORE_KEY: &str = "entries";
/// 告知主 Agent 当前项目已有历史索引的提示 ID。
const HISTORY_PROMPT_ID: &str = "knowledge.discovery";
/// 历史面板的视图 ID。
const HISTORY_VIEW: &str = "history-panel";
/// 打开历史面板的主输入触发前缀。
const HISTORY_TRIGGER: &str = "/history";
/// 兼容旧知识库面板的触发前缀。
const LEGACY_HISTORY_TRIGGER: &str = "/kb";
/// TUI 在会话已经持久化后发布的运行结束事件。
const TUI_RUN_FINISH_EVENT: &str = "tui.run.finish";
/// 当前磁盘结构版本；v1 是模型蒸馏索引，可直接重建。
const SCHEMA_VERSION: u32 = 2;

/// 内存和持久存储允许保留的最大历史片段数。
const MAX_ENTRIES: usize = 2_000;
/// 从宿主分页读取会话时请求的消息数。
const SESSION_PAGE_SIZE: usize = 64;
/// 单条用户请求进入索引的最大字符数。
const MAX_REQUEST_CHARS: usize = 600;
/// 单条助手结果进入索引的最大字符数。
const MAX_OUTCOME_CHARS: usize = 900;
/// 单个历史片段保存的原文证据最大字符数。
const MAX_EVIDENCE_CHARS: usize = 1_200;
/// 检索默认返回的片段数。
const DEFAULT_SEARCH_LIMIT: usize = 8;
/// 检索允许返回的最大片段数。
const MAX_SEARCH_LIMIT: usize = 30;
/// 回溯原文时单次返回的最大消息数。
const MAX_READ_MESSAGES: usize = 60;

/// 一条可检索的历史会话片段。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct HistoryEntry {
    /// 插件内稳定唯一的短 ID。
    id: String,
    /// 来源会话标识，回溯原文时使用。
    session_id: String,
    /// 来源消息区间起点，闭区间。
    range_start: usize,
    /// 来源消息区间终点，开区间。
    range_end: usize,
    /// 会话标题；缺少宿主标题时由用户请求首行截断生成。
    title: String,
    /// 开启该片段的用户请求。
    request: String,
    /// 片段内最后一条非空助手回复；未形成回复时为空。
    outcome: String,
    /// 片段内出现过的工具名，按首次出现顺序去重。
    #[serde(default)]
    tools: Vec<String>,
    /// 有界原文证据，用于词法召回，不替代按消息区间回读。
    evidence: String,
    /// 来源会话创建时间，单位为 UNIX epoch 毫秒。
    created_at_ms: u64,
    /// 来源会话最近保存时间，单位为 UNIX epoch 毫秒。
    updated_at_ms: u64,
}

impl HistoryEntry {
    /// 拼出该片段参与检索的文本。
    fn searchable(&self) -> String {
        let tools = self.tools.join(" ");
        searchable_text(&[
            &self.session_id,
            &self.title,
            &self.request,
            &self.outcome,
            &tools,
            &self.evidence,
        ])
    }
}

/// 历史索引元数据。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct HistoryMeta {
    /// 磁盘结构版本。
    #[serde(default)]
    schema_version: u32,
    /// 已扫描会话及扫描时的修订号；修订变化后重建该会话片段。
    #[serde(default)]
    indexed: BTreeMap<String, u64>,
    /// 已分配的最大片段序号，保证 ID 不因删除而复用。
    #[serde(default)]
    next_sequence: u64,
    /// 最近一次同步失败的说明；完整成功后清空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_error: Option<String>,
}

/// 一次增量同步的统计结果。
#[derive(Debug, Default, PartialEq, Eq)]
struct SyncReport {
    /// 本次重新扫描的会话数。
    updated_sessions: usize,
    /// 本次移除的已删除会话数。
    removed_sessions: usize,
}

/// 历史会话检索插件。
#[derive(Default)]
struct KnowledgePlugin {
    /// 索引元数据快照。
    meta: HistoryMeta,
    /// 全部历史片段快照，始终按更新时间从新到旧排列。
    entries: Vec<HistoryEntry>,
    /// 与 `entries` 一一对应的可检索文本缓存。
    haystacks: Vec<String>,
    /// 项目持久存储是否可用。
    available: bool,
    /// 面板当前是否可见。
    panel_visible: bool,
}

impl KnowledgePlugin {
    /// 从持久存储载入元数据与历史片段。
    ///
    /// v1 保存的是模型蒸馏结果，不能无损映射为按用户轮次切分的 v2 结构，因此把它视为
    /// 可重建缓存并清空；会话原文仍由宿主保存，不会丢失用户数据。
    fn load(&mut self, host: &dyn PluginHostApi) -> Result<()> {
        let stored_meta = host.store_get(PluginStoreScope::Workspace, META_STORE_KEY)?;
        let current_schema = stored_meta
            .as_ref()
            .and_then(|value| serde_json::from_value::<HistoryMeta>(value.clone()).ok())
            .filter(|meta| meta.schema_version == SCHEMA_VERSION);

        if let Some(meta) = current_schema {
            self.meta = meta;
            self.entries = match host.store_get(PluginStoreScope::Workspace, ENTRIES_STORE_KEY)? {
                Some(value) => serde_json::from_value(value).context("解析历史会话片段失败")?,
                None => Vec::new(),
            };
        } else {
            self.meta = HistoryMeta {
                schema_version: SCHEMA_VERSION,
                ..HistoryMeta::default()
            };
            self.entries.clear();
        }
        self.sort_and_limit_entries();
        self.rebuild_haystacks();
        Ok(())
    }

    /// 保存元数据。
    fn save_meta(&self, host: &dyn PluginHostApi) -> Result<()> {
        host.store_set(
            PluginStoreScope::Workspace,
            META_STORE_KEY,
            &serde_json::to_value(&self.meta)?,
        )
    }

    /// 保存历史片段。
    fn save_entries(&self, host: &dyn PluginHostApi) -> Result<()> {
        host.store_set(
            PluginStoreScope::Workspace,
            ENTRIES_STORE_KEY,
            &serde_json::to_value(&self.entries)?,
        )
    }

    /// 重建可检索文本缓存。
    fn rebuild_haystacks(&mut self) {
        self.haystacks = self.entries.iter().map(HistoryEntry::searchable).collect();
    }

    /// 按更新时间排序并限制索引规模。
    fn sort_and_limit_entries(&mut self) {
        self.entries.sort_by(|left, right| {
            right
                .updated_at_ms
                .cmp(&left.updated_at_ms)
                .then_with(|| right.created_at_ms.cmp(&left.created_at_ms))
                .then_with(|| left.session_id.cmp(&right.session_id))
                .then_with(|| right.range_start.cmp(&left.range_start))
        });
        self.entries.truncate(MAX_ENTRIES);
    }

    /// 按当前片段数量维护主 Agent 的历史发现提示。
    ///
    /// 提示只声明能力和片段数，不注入历史正文；空索引时移除，避免为不可用能力占用上下文。
    fn refresh_prompt(&self, host: &dyn PluginHostApi) -> Result<()> {
        if !self.available || self.entries.is_empty() {
            return host.remove_prompt(HISTORY_PROMPT_ID);
        }
        host.upsert_prompt(&PromptContribution {
            id: HISTORY_PROMPT_ID.into(),
            content: format!(
                "当前项目有 {} 个历史会话片段可检索。用户说“继续”“上次”“之前”或任务可能处理过时，先自行判断是否调用 history_search；命中后用 history_read 核对原文，并以当前代码为准。",
                self.entries.len()
            ),
            priority: 130,
        })?;
        Ok(())
    }

    /// 校验项目作用域是否可用。
    fn ensure_available(&self) -> Result<()> {
        if !self.available {
            return Err(anyhow!(
                "当前宿主未提供项目作用域，历史会话索引不可用；索引按项目隔离，没有项目身份时无处存放"
            ));
        }
        Ok(())
    }

    /// 增量同步全部可见会话。
    ///
    /// 未变化会话只比较修订号，不读取正文。变化会话分页读取并整体替换旧片段；单个会话
    /// 失败会保留它的旧片段并在下次同步重试，不阻塞其他会话。
    fn sync_sessions(&mut self, host: &dyn PluginHostApi) -> Result<SyncReport> {
        self.ensure_available()?;
        let mut sessions = host.session_list().context("列出历史会话失败")?;
        sessions.sort_by(|left, right| {
            left.updated_at_ms
                .cmp(&right.updated_at_ms)
                .then_with(|| left.id.cmp(&right.id))
        });

        let current_ids: HashSet<&str> =
            sessions.iter().map(|session| session.id.as_str()).collect();
        let removed_ids: Vec<String> = self
            .meta
            .indexed
            .keys()
            .filter(|id| !current_ids.contains(id.as_str()))
            .cloned()
            .collect();
        for session_id in &removed_ids {
            self.meta.indexed.remove(session_id);
            self.entries.retain(|entry| entry.session_id != *session_id);
        }

        let mut report = SyncReport {
            removed_sessions: removed_ids.len(),
            ..SyncReport::default()
        };
        let mut errors = Vec::new();

        for summary in sessions {
            if self
                .meta
                .indexed
                .get(&summary.id)
                .is_some_and(|revision| *revision == summary.revision)
            {
                continue;
            }

            match scan_session(host, &summary) {
                Ok(Some(mut replacements)) => {
                    for entry in &mut replacements {
                        self.meta.next_sequence += 1;
                        entry.id = format!("h{}", self.meta.next_sequence);
                    }
                    self.entries.retain(|entry| entry.session_id != summary.id);
                    self.entries.extend(replacements);
                    self.meta.indexed.insert(summary.id, summary.revision);
                    report.updated_sessions += 1;
                }
                Ok(None) => {
                    self.entries.retain(|entry| entry.session_id != summary.id);
                    self.meta.indexed.remove(&summary.id);
                }
                Err(error) => errors.push(format!("会话 {}：{error}", summary.id)),
            }
        }

        self.sort_and_limit_entries();
        self.rebuild_haystacks();
        self.meta.last_error = if errors.is_empty() {
            None
        } else {
            Some(errors.join("；"))
        };
        self.save_entries(host)?;
        self.save_meta(host)?;
        self.refresh_prompt(host)?;
        Ok(report)
    }

    /// 渲染历史面板内容。
    fn panel_lines(&self) -> Vec<UiLine> {
        let session_count = self
            .entries
            .iter()
            .map(|entry| entry.session_id.as_str())
            .collect::<BTreeSet<_>>()
            .len();
        let latest = self
            .entries
            .first()
            .map(|entry| relative_time_at(entry.updated_at_ms, unix_time_ms()));
        let mut lines = vec![UiLine {
            spans: vec![
                styled("历史会话", UiColor::White, true),
                styled(
                    format!(
                        "  片段 {} · 会话 {} · 最近 {}",
                        self.entries.len(),
                        session_count,
                        latest.as_deref().unwrap_or("无")
                    ),
                    UiColor::Gray,
                    false,
                ),
            ],
        }];

        if !self.available {
            lines.push(UiLine {
                spans: vec![styled(
                    "当前宿主未提供项目作用域，历史会话索引不可用。",
                    UiColor::Yellow,
                    false,
                )],
            });
            return lines;
        }

        for entry in self.entries.iter().take(8) {
            lines.push(UiLine {
                spans: vec![
                    styled(format!("{:<7}", entry.id), UiColor::Cyan, false),
                    styled(
                        format!(
                            "{:<10}",
                            truncate_width(
                                &relative_time_at(entry.updated_at_ms, unix_time_ms()),
                                10,
                            )
                        ),
                        UiColor::Gray,
                        false,
                    ),
                    styled(truncate_width(&entry.request, 58), UiColor::White, false),
                ],
            });
        }
        if self.entries.is_empty() {
            lines.push(UiLine {
                spans: vec![styled(
                    "尚未发现可索引的用户消息；插件会在会话持久化后自动同步。",
                    UiColor::Gray,
                    false,
                )],
            });
        }
        if let Some(error) = &self.meta.last_error {
            lines.push(UiLine {
                spans: vec![styled(
                    truncate_width(&format!("最近失败：{error}"), 76),
                    UiColor::Yellow,
                    false,
                )],
            });
        }
        lines.push(UiLine {
            spans: vec![styled("Esc 关闭", UiColor::Gray, false)],
        });
        lines
    }

    /// 在历史片段中检索，省略查询时返回最近记录。
    fn handle_search(&self, call: ToolCall) -> Result<ToolResult> {
        self.ensure_available()?;
        let query = call
            .args
            .get("query")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let limit = call
            .args
            .get("limit")
            .and_then(|value| value.as_u64())
            .map(|limit| (limit as usize).clamp(1, MAX_SEARCH_LIMIT))
            .unwrap_or(DEFAULT_SEARCH_LIMIT);
        let now_ms = unix_time_ms();

        let results: Vec<_> = if let Some(query) = query {
            search(&self.haystacks, query, limit)
                .into_iter()
                .map(|hit| history_result(&self.entries[hit.index], Some(hit.score), now_ms))
                .collect()
        } else {
            self.entries
                .iter()
                .take(limit)
                .map(|entry| history_result(entry, None, now_ms))
                .collect()
        };

        Ok(ToolResult::success(
            call.id,
            call.name,
            json!({
                "count": results.len(),
                "query": query,
                "results": results,
                "time_granularity": "session",
                "note": if self.entries.is_empty() {
                    "历史索引为空；插件会在激活和成功的 TUI 运行结束后自动同步"
                } else if results.is_empty() {
                    "没有词法命中；可缩短关键词、改用标识符，或省略 query 查看最近记录"
                } else {
                    "命中内容只是历史证据；用 history_read 核对原文，并以当前代码和运行状态为准"
                },
            }),
        ))
    }

    /// 按片段消息区间回读会话原文。
    fn handle_read(&self, host: &dyn PluginHostApi, call: ToolCall) -> Result<ToolResult> {
        self.ensure_available()?;
        let entry_id = call
            .args
            .get("entry_id")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| anyhow!("history_read 需要非空的 entry_id"))?;
        let Some(entry) = self.entries.iter().find(|entry| entry.id == entry_id) else {
            return Ok(ToolResult::error(
                call.id,
                call.name,
                format!("没有 ID 为 `{entry_id}` 的历史片段；可先用 history_search 查找。"),
            ));
        };
        let offset = call
            .args
            .get("offset")
            .and_then(|value| value.as_u64())
            .map(|value| value as usize)
            .unwrap_or(entry.range_start);
        if !(entry.range_start..entry.range_end).contains(&offset) {
            return Ok(ToolResult::error(
                call.id,
                call.name,
                format!(
                    "offset {offset} 超出片段 `{entry_id}` 的消息区间 {}..{}",
                    entry.range_start, entry.range_end
                ),
            ));
        }
        let limit = MAX_READ_MESSAGES.min(entry.range_end - offset);
        let Some(page) = host
            .session_read(&entry.session_id, offset, Some(limit))
            .context("读取历史会话原文失败")?
        else {
            return Ok(ToolResult::error(
                call.id,
                call.name,
                format!(
                    "片段 `{entry_id}` 的来源会话 `{}` 已不存在；索引会在下次同步时移除。",
                    entry.session_id
                ),
            ));
        };
        let messages: Vec<_> = page
            .messages
            .into_iter()
            .filter(|message| message.index < entry.range_end)
            .collect();
        let next_offset = messages
            .last()
            .map(|message| message.index + 1)
            .filter(|next| *next < entry.range_end);
        let now_ms = unix_time_ms();

        Ok(ToolResult::success(
            call.id,
            call.name,
            json!({
                "entry_id": entry.id,
                "session_id": entry.session_id,
                "title": entry.title,
                "request": entry.request,
                "outcome": entry.outcome,
                "tools": entry.tools,
                "range_start": entry.range_start,
                "range_end": entry.range_end,
                "transcript": render_transcript(&messages),
                "next_offset": next_offset,
                "created_at_ms": entry.created_at_ms,
                "created_at_utc": format_utc_timestamp(entry.created_at_ms),
                "updated_at_ms": entry.updated_at_ms,
                "updated_at_utc": format_utc_timestamp(entry.updated_at_ms),
                "updated_relative": relative_time_at(entry.updated_at_ms, now_ms),
                "time_granularity": "session",
                "note": "历史内容只作证据；继续修改前核对当前代码、配置和运行状态",
            }),
        ))
    }
}

impl AgentPlugin for KnowledgePlugin {
    /// 恢复索引并自动同步未扫描或修订变化的会话。
    ///
    /// 项目作用域不可用时降级为空索引而不是阻止插件加载；会话同步失败时保留已有缓存，
    /// 等待下一次激活或 TUI 运行结束重试。
    fn activate(&mut self, host: &dyn PluginHostApi, _context: ActivationContext) -> Result<()> {
        match self.load(host) {
            Ok(()) => {
                self.available = true;
                if let Err(error) = self.sync_sessions(host) {
                    self.meta.last_error = Some(format!("同步历史会话失败：{error}"));
                    let _ = self.save_meta(host);
                    let _ = self.refresh_prompt(host);
                }
            }
            Err(_) => {
                self.available = false;
                self.meta = HistoryMeta {
                    schema_version: SCHEMA_VERSION,
                    ..HistoryMeta::default()
                };
                self.entries.clear();
                self.haystacks.clear();
                host.remove_prompt(HISTORY_PROMPT_ID)?;
            }
        }
        Ok(())
    }

    /// 移除临时发现提示；持久化索引继续保留。
    fn deactivate(&mut self, host: &dyn PluginHostApi) -> Result<()> {
        host.remove_prompt(HISTORY_PROMPT_ID)
    }

    /// 声明历史检索与原文回读工具。
    fn list_tools(&self) -> Vec<ToolSpec> {
        vec![history_search_tool(), history_read_tool()]
    }

    /// 分派插件工具调用。
    fn call_tool_with_host(
        &mut self,
        host: &dyn PluginHostApi,
        call: ToolCall,
    ) -> Result<ToolResult> {
        match call.name.as_str() {
            "history_search" => self.handle_search(call),
            "history_read" => self.handle_read(host, call),
            other => Err(anyhow!("历史会话插件不支持工具 `{other}`")),
        }
    }

    /// 在成功且未取消的 TUI 运行持久化后增量同步历史。
    fn on_event(&mut self, host: &dyn PluginHostApi, event: &mut PluginEvent) -> EventOutcome {
        if !self.available || event.name() != TUI_RUN_FINISH_EVENT {
            return EventOutcome::ready();
        }
        let succeeded = event
            .data()
            .get("succeeded")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        let cancelled = event
            .data()
            .get("cancelled")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        if succeeded && !cancelled {
            if let Err(error) = self.sync_sessions(host) {
                self.meta.last_error = Some(format!("同步历史会话失败：{error}"));
                let _ = self.save_meta(host);
            }
        }
        EventOutcome::ready()
    }

    /// 历史工具使用简洁 Search/Read 标题，来源以可读的 History 标签区分文件工具。
    fn describe_tool_renderers(&self) -> Vec<ToolRendererContribution> {
        ["history_search", "history_read"]
            .into_iter()
            .map(|tool| ToolRendererContribution {
                plugin_id: String::new(),
                renderer_id: format!("history-{tool}"),
                tool_name: tool.into(),
            })
            .collect()
    }

    /// 将检索线索或原文回读转换成正文组件，保留历史证据提示与分页信息。
    fn render_tool(&mut self, request: ToolRenderRequest) -> Option<UiFrame> {
        let name = request.context.call.name.as_str();
        let verb = match name {
            "history_search" => "Search",
            "history_read" => "Read",
            _ => return None,
        };
        if request.renderer_id != format!("history-{name}") || request.context.max_height == 0 {
            return None;
        }
        let (color, summary, body) = match &request.context.state {
            ToolRenderState::Running => (UiColor::Yellow, "处理中".into(), Vec::new()),
            ToolRenderState::Skipped { reason } => {
                (UiColor::Gray, format!("skipped · {reason}"), Vec::new())
            }
            ToolRenderState::Finished { result } => {
                if result.is_error {
                    return None;
                }
                let value = &result.content;
                let (summary, mut body) = if name == "history_search" {
                    let entries = value.get("results")?.as_array()?;
                    let mut rows = Vec::new();
                    for entry in entries {
                        rows.push(format!(
                            "[{}] {}",
                            entry.get("entry_id")?.as_str()?,
                            entry.get("title")?.as_str()?
                        ));
                        if let Some(time) = entry.get("updated_at_utc").and_then(|v| v.as_str()) {
                            rows.push(time.into());
                        }
                        rows.push(format!("请求：{}", entry.get("request")?.as_str()?));
                        rows.push(format!("结果：{}", entry.get("outcome")?.as_str()?));
                    }
                    (format!("{} 条历史线索", entries.len()), rows)
                } else {
                    let mut rows = vec![value.get("transcript")?.as_str()?.to_string()];
                    if let Some(offset) = value.get("next_offset").and_then(|v| v.as_u64()) {
                        rows.insert(0, format!("后续内容从消息 {offset} 继续读取"));
                    }
                    (value.get("title")?.as_str()?.to_string(), rows)
                };
                // 历史是证据而非当前指令；提示置于正文之前，折叠时也不丢失边界说明。
                if let Some(note) = value.get("note").and_then(|v| v.as_str()) {
                    body.insert(0, note.into());
                }
                (UiColor::Green, summary, body)
            }
        };
        let mut lines = vec![UiLine {
            spans: vec![
                styled("● ", color, false),
                styled(verb, UiColor::White, true),
                styled("(History)", UiColor::Gray, false),
            ],
        }];
        lines.extend(summary.split('\n').enumerate().map(|(index, row)| UiLine {
            spans: vec![styled(
                format!("{}{row}", if index == 0 { "  └ " } else { "    " }),
                UiColor::Gray,
                false,
            )],
        }));
        for text in body {
            lines.extend(text.split('\n').map(|row| UiLine {
                spans: vec![UiSpan {
                    text: format!("  {row}"),
                    style: UiStyle::default(),
                }],
            }));
        }
        let limit = usize::from(request.context.max_height).min(4096);
        if lines.len() > limit {
            let omitted = lines.len() - limit + 1;
            lines.truncate(limit.saturating_sub(1));
            lines.push(UiLine {
                spans: vec![styled(
                    format!("  … +{omitted} 行（完整内容保留在工具结果）"),
                    UiColor::Gray,
                    false,
                )],
            });
        }
        Some(UiFrame {
            view_id: request.renderer_id,
            visible: true,
            lines,
            cursor: None,
        })
    }

    /// 声明历史会话输入面板。
    fn describe_ui(&self) -> Vec<UiDeclaration> {
        vec![UiDeclaration {
            plugin_id: String::new(),
            view_id: HISTORY_VIEW.into(),
            title: "历史会话".into(),
            placement: UiPlacement::InputPanel,
            size: UiSize {
                width: None,
                height: Some(13),
            },
            focusable: true,
            input_triggers: vec![HISTORY_TRIGGER.into(), LEGACY_HISTORY_TRIGGER.into()],
        }]
    }

    /// 渲染历史会话面板。
    fn render_ui(&mut self, request: UiRenderRequest) -> Option<UiFrame> {
        if request.view_id != HISTORY_VIEW {
            return None;
        }
        Some(UiFrame {
            view_id: HISTORY_VIEW.into(),
            visible: self.panel_visible,
            lines: self.panel_lines(),
            cursor: None,
        })
    }

    /// 跟随 `/history` 或兼容的 `/kb` 输入显示面板，Esc 收起。
    fn on_ui_input(&mut self, input: UiInput) {
        if input.view_id != HISTORY_VIEW {
            return;
        }
        match input.event {
            UiInputEvent::MainInput { text, .. } => {
                let text = text.trim_start();
                self.panel_visible =
                    text.starts_with(HISTORY_TRIGGER) || text.starts_with(LEGACY_HISTORY_TRIGGER);
            }
            UiInputEvent::Key { code, .. } if code == "escape" => self.panel_visible = false,
            _ => {}
        }
    }
}

/// 流式保存片段证据的首尾，避免长会话在 WASM 线性内存中形成完整副本。
struct EvidenceBuffer {
    /// 证据开头，优先保留用户请求与初始操作。
    head: String,
    /// 证据末尾，保留最终回复和最后工具结果。
    tail: VecDeque<char>,
    /// 已写入开头的字符数，避免反复扫描 UTF-8 字符串。
    head_chars: usize,
    /// 已观察的总字符数，用于判断是否需要省略标记。
    total_chars: usize,
    /// 开头字符预算。
    head_limit: usize,
    /// 末尾字符预算。
    tail_limit: usize,
}

impl EvidenceBuffer {
    /// 按总预算创建首尾缓冲区，省略标记计入持久化上限。
    fn new() -> Self {
        let marker_chars = evidence_omission_marker().chars().count();
        let available = MAX_EVIDENCE_CHARS.saturating_sub(marker_chars);
        let head_limit = available * 2 / 3;
        Self {
            head: String::new(),
            tail: VecDeque::new(),
            head_chars: 0,
            total_chars: 0,
            head_limit,
            tail_limit: available - head_limit,
        }
    }

    /// 追加一段文本；超出开头预算后只滚动保留末尾预算。
    fn push_str(&mut self, text: &str) {
        for ch in text.chars() {
            self.total_chars += 1;
            if self.head_chars < self.head_limit {
                self.head.push(ch);
                self.head_chars += 1;
                continue;
            }
            self.tail.push_back(ch);
            if self.tail.len() > self.tail_limit {
                self.tail.pop_front();
            }
        }
    }

    /// 完成有界证据；发生截断时在首尾之间插入明确标记。
    fn finish(self) -> String {
        let tail: String = self.tail.into_iter().collect();
        let available = self.head_limit + self.tail_limit;
        let output = if self.total_chars > available {
            format!(
                "{}{marker}{tail}",
                self.head,
                marker = evidence_omission_marker()
            )
        } else {
            format!("{}{tail}", self.head)
        };
        output.trim().to_string()
    }
}

/// 正在构建的一条用户轮次片段。
struct HistoryEntryBuilder {
    /// 来源会话标识。
    session_id: String,
    /// 片段起始消息序号。
    range_start: usize,
    /// 当前片段末尾消息序号，开区间。
    range_end: usize,
    /// 会话标题或无模型派生标题。
    title: String,
    /// 有界用户请求。
    request: String,
    /// 当前最后一条非空助手回复。
    outcome: String,
    /// 片段内工具名。
    tools: Vec<String>,
    /// 流式首尾证据。
    evidence: EvidenceBuffer,
    /// 来源会话创建时间。
    created_at_ms: u64,
    /// 来源会话最近保存时间。
    updated_at_ms: u64,
}

impl HistoryEntryBuilder {
    /// 从有正文或附件的用户消息开始一个片段；空用户消息不建立索引。
    fn start(summary: &PluginSessionSummary, message: &PluginSessionMessage) -> Option<Self> {
        if message.text.trim().is_empty() && !message.has_attachments {
            return None;
        }
        let request = render_user_request(message);
        let title = summary
            .title
            .as_deref()
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(|title| truncate_chars(title, 120))
            .unwrap_or_else(|| derive_title(&request));
        Some(Self {
            session_id: summary.id.clone(),
            range_start: message.index,
            range_end: message.index + 1,
            title,
            request: truncate_chars(&request, MAX_REQUEST_CHARS),
            outcome: String::new(),
            tools: Vec::new(),
            evidence: EvidenceBuffer::new(),
            created_at_ms: summary.created_at_ms,
            updated_at_ms: summary.updated_at_ms,
        })
    }

    /// 接收片段内的一条消息，并更新结果、工具名、区间与有界证据。
    fn observe(&mut self, message: &PluginSessionMessage) {
        self.range_end = message.index + 1;
        if message.role == "assistant" && !message.text.trim().is_empty() {
            self.outcome = truncate_chars(&message.text, MAX_OUTCOME_CHARS);
        }
        for name in message
            .tool_calls
            .iter()
            .map(|call| call.name.as_str())
            .chain(
                message
                    .tool_results
                    .iter()
                    .map(|result| result.name.as_str()),
            )
        {
            if !self.tools.iter().any(|existing| existing == name) {
                self.tools.push(name.to_string());
            }
        }
        self.evidence.push_str(&render_message(message));
        self.evidence.push_str("\n");
    }

    /// 完成片段并生成待分配 ID 的持久化条目。
    fn finish(self) -> HistoryEntry {
        HistoryEntry {
            id: String::new(),
            session_id: self.session_id,
            range_start: self.range_start,
            range_end: self.range_end,
            title: self.title,
            request: self.request,
            outcome: self.outcome,
            tools: self.tools,
            evidence: self.evidence.finish(),
            created_at_ms: self.created_at_ms,
            updated_at_ms: self.updated_at_ms,
        }
    }
}

/// 分页扫描一份会话，并在读取过程中按用户轮次流式建立片段。
///
/// 宿主同时按消息数和字节数限制单页大小，因此偏移量必须使用最后一条消息的真实序号推进。
/// 空页且尚未到 `message_count` 表示宿主没有推进分页，返回错误以避免无限循环。
fn scan_session(
    host: &dyn PluginHostApi,
    summary: &PluginSessionSummary,
) -> Result<Option<Vec<HistoryEntry>>> {
    if summary.message_count == 0 {
        return Ok(Some(Vec::new()));
    }
    let mut entries = Vec::new();
    let mut current: Option<HistoryEntryBuilder> = None;
    let mut offset = 0;

    while offset < summary.message_count {
        let Some(page) = host
            .session_read(&summary.id, offset, Some(SESSION_PAGE_SIZE))
            .with_context(|| format!("从消息 {offset} 读取失败"))?
        else {
            return Ok(None);
        };
        let Some(last) = page.messages.last() else {
            return Err(anyhow!(
                "会话报告有 {} 条消息，但从 {offset} 开始返回空页",
                summary.message_count
            ));
        };
        let next_offset = last.index.saturating_add(1);
        if next_offset <= offset {
            return Err(anyhow!("会话分页未向前推进"));
        }

        for message in page.messages {
            if message.role == "user" {
                if let Some(builder) = current.take() {
                    entries.push(builder.finish());
                }
                current = HistoryEntryBuilder::start(summary, &message);
            }
            if let Some(builder) = current.as_mut() {
                builder.observe(&message);
            }
        }
        offset = next_offset;
    }

    if let Some(builder) = current {
        entries.push(builder.finish());
    }
    Ok(Some(entries))
}

/// 渲染用户请求，并保留插件不可读取附件正文这一事实。
fn render_user_request(message: &PluginSessionMessage) -> String {
    let mut request = message.text.trim().to_string();
    if message.has_attachments {
        if !request.is_empty() {
            request.push('\n');
        }
        request.push_str("[用户附带了插件不可读取的附件]");
    }
    request
}

/// 从用户请求首个非空行生成无模型标题。
fn derive_title(request: &str) -> String {
    let first_line = request
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("未命名历史会话");
    truncate_chars(first_line, 80)
}

/// 把消息投影渲染为检索证据与回读共用的纯文本。
fn render_transcript(messages: &[PluginSessionMessage]) -> String {
    messages
        .iter()
        .map(render_message)
        .collect::<Vec<_>>()
        .join("\n")
}

/// 把单条消息投影渲染为纯文本。
fn render_message(message: &PluginSessionMessage) -> String {
    let mut output = format!("[{}] ", message.role);
    if !message.text.is_empty() {
        output.push_str(&message.text);
    }
    if message.has_attachments {
        output.push_str(" [含附件]");
    }
    for call in &message.tool_calls {
        output.push_str(&format!("\n  -> 调用 {} {}", call.name, call.arguments));
    }
    for result in &message.tool_results {
        let status = if result.is_error { "失败" } else { "成功" };
        output.push_str(&format!(
            "\n  <- {} {} {}",
            result.name, status, result.content
        ));
    }
    output
}

/// 返回证据首尾之间使用的稳定省略标记。
fn evidence_omission_marker() -> &'static str {
    "\n...（中间内容已省略）...\n"
}

/// 构造一条历史检索结果，并补齐绝对和相对时间。
fn history_result(entry: &HistoryEntry, score: Option<f64>, now_ms: u64) -> serde_json::Value {
    json!({
        "entry_id": entry.id,
        "session_id": entry.session_id,
        "title": entry.title,
        "request": entry.request,
        "outcome": entry.outcome,
        "tools": entry.tools,
        "range_start": entry.range_start,
        "range_end": entry.range_end,
        "score": score.map(|value| (value * 1000.0).round() / 1000.0),
        "created_at_ms": entry.created_at_ms,
        "created_at_utc": format_utc_timestamp(entry.created_at_ms),
        "updated_at_ms": entry.updated_at_ms,
        "updated_at_utc": format_utc_timestamp(entry.updated_at_ms),
        "updated_relative": relative_time_at(entry.updated_at_ms, now_ms),
    })
}

/// 返回当前 UNIX epoch 毫秒；系统时钟早于 epoch 时回落为零。
fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// 生成人类可读的相对时间标签，并处理少量时钟向未来漂移的情况。
fn relative_time_at(timestamp_ms: u64, now_ms: u64) -> String {
    let (future, seconds) = match timestamp_ms.cmp(&now_ms) {
        Ordering::Greater => (true, (timestamp_ms - now_ms) / 1_000),
        _ => (false, (now_ms - timestamp_ms) / 1_000),
    };
    if seconds <= 59 {
        return "刚刚".into();
    }
    let (value, unit) = match seconds {
        60..=3_599 => (seconds / 60, "分钟"),
        3_600..=86_399 => (seconds / 3_600, "小时"),
        86_400..=2_592_000 => (seconds / 86_400, "天"),
        2_592_001..=31_536_000 => (seconds / 2_592_000, "个月"),
        _ => (seconds / 31_536_000, "年"),
    };
    if future {
        format!("{value} {unit}后")
    } else {
        format!("{value} {unit}前")
    }
}

/// 将 UNIX epoch 毫秒格式化为 UTC ISO 8601 时间，不引入额外日期依赖。
fn format_utc_timestamp(timestamp_ms: u64) -> String {
    let total_seconds = timestamp_ms / 1_000;
    let milliseconds = timestamp_ms % 1_000;
    let days = i64::try_from(total_seconds / 86_400).unwrap_or(i64::MAX);
    let seconds_of_day = total_seconds % 86_400;
    let hour = seconds_of_day / 3_600;
    let minute = seconds_of_day % 3_600 / 60;
    let second = seconds_of_day % 60;
    let (year, month, day) = civil_date_from_unix_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{milliseconds:03}Z")
}

/// 把自 1970-01-01 起的天数转换为公历年月日。
///
/// 算法按 400 年公历周期计算，覆盖闰年规则且不依赖本地时区。
fn civil_date_from_unix_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

/// 按字符数截断文本，并移除首尾空白。
fn truncate_chars(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    let mut output: String = trimmed.chars().take(max_chars.saturating_sub(1)).collect();
    output.push('…');
    output
}

/// 按显示宽度截断面板文本。
fn truncate_width(text: &str, max_width: usize) -> String {
    let mut output = String::new();
    let mut width = 0;
    for ch in text.chars() {
        let ch = if ch == '\n' || ch == '\r' { ' ' } else { ch };
        let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_width > max_width {
            output.push('…');
            break;
        }
        output.push(ch);
        width += ch_width;
    }
    output
}

/// 历史检索工具定义；省略 `query` 时返回最近片段，便于处理“继续做某事”。
fn history_search_tool() -> ToolSpec {
    ToolSpec::new(
        "history_search",
        "检索当前项目的历史会话。用户说继续、上次、之前，或任务可能曾处理过时自行判断是否调用；query 可省略，省略时返回最近片段。命中后用 history_read 核对原文。",
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "可选检索词，支持自然语言、标识符、文件路径和报错文本；省略时按时间返回最近片段"
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_SEARCH_LIMIT,
                    "description": "返回片段数上限，默认 8"
                }
            },
            "additionalProperties": false
        }),
    )
    .with_permission_scope(ToolPermissionScope::Control)
}

/// 历史原文回读工具定义。
fn history_read_tool() -> ToolSpec {
    ToolSpec::new(
        "history_read",
        "按 history_search 返回的片段 ID 读取对应消息区间。历史内容只作证据，继续修改前应核对当前代码、配置和运行状态。",
        json!({
            "type": "object",
            "properties": {
                "entry_id": {
                    "type": "string",
                    "description": "history_search 返回的片段 ID"
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "片段区间内的起始消息序号；返回 next_offset 时可继续读取"
                }
            },
            "required": ["entry_id"],
            "additionalProperties": false
        }),
    )
    .with_permission_scope(ToolPermissionScope::Control)
}

/// 构造一段统一风格的声明式 UI 文本。
fn styled(text: impl Into<String>, color: UiColor, bold: bool) -> UiSpan {
    UiSpan {
        text: text.into(),
        style: UiStyle {
            foreground: Some(color),
            bold,
            ..UiStyle::default()
        },
    }
}

export_plugin!(KnowledgePlugin);

#[cfg(test)]
mod tests {
    use super::*;
    use agent_plugin::{
        FileEntry, ModelCompletionRequest, ModelCompletionResponse, PluginEventEnvelope,
        PluginSessionPage, PluginSessionToolCall, PluginSessionToolResult, ServiceDescriptor,
        ServiceSpec,
    };
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// 历史组件保留证据说明、原文和分页提示，不把当前索引状态混入历史结果。
    #[test]
    fn history_tool_components_preserve_evidence_and_pagination() {
        let mut plugin = KnowledgePlugin::default();
        assert_eq!(
            plugin.describe_tool_renderers().len(),
            plugin.list_tools().len()
        );
        let cases = [
            (
                "history_search",
                "Search",
                json!({"results":[{"entry_id":"h1","title":"标题","request":"问题","outcome":"答案"}],"note":"仅作历史证据","future":true}),
                "答案",
            ),
            (
                "history_read",
                "Read",
                json!({"title":"标题","transcript":"第一行\n中文 👩‍💻 第二行","next_offset":20,"note":"仅作历史证据"}),
                "第二行",
            ),
        ];
        for (name, verb, content, expected) in cases {
            let mut request = ToolRenderRequest {
                plugin_id: "knowledge".into(),
                renderer_id: format!("history-{name}"),
                context: agent_plugin::ToolRenderContext {
                    call: ToolCall::new("test", name, json!({})),
                    state: ToolRenderState::Finished {
                        result: ToolResult::success("test", name, content),
                    },
                    width: 20,
                    max_height: 40,
                    frame: 1,
                },
            };
            let frame = plugin.render_tool(request.clone()).unwrap();
            let text = frame
                .lines
                .iter()
                .flat_map(|l| &l.spans)
                .map(|s| s.text.as_str())
                .collect::<String>();
            assert!(
                text.starts_with(&format!("● {verb}(History)")) && text.contains(expected),
                "{text}"
            );
            assert!(frame.lines[2].spans[0].text.contains("历史证据"));
            if name == "history_read" {
                assert!(text.contains("20") && text.contains("👩‍💻"));
            }
            request.context.max_height = 3;
            assert_eq!(plugin.render_tool(request.clone()).unwrap().lines.len(), 3);
            request.context.state = ToolRenderState::Finished {
                result: ToolResult::success("test", name, json!({})),
            };
            assert!(plugin.render_tool(request.clone()).is_none());
            request.context.state = ToolRenderState::Finished {
                result: ToolResult::error("test", name, "读取拒绝"),
            };
            assert!(plugin.render_tool(request).is_none());
        }
    }

    /// 同时模拟持久存储与只读会话的测试替身。
    struct StubHost {
        values: RefCell<HashMap<String, serde_json::Value>>,
        sessions: RefCell<Vec<(PluginSessionSummary, Vec<PluginSessionMessage>)>>,
        prompts: RefCell<HashMap<String, String>>,
        read_offsets: RefCell<Vec<(String, usize)>>,
        workspace_available: bool,
    }

    impl StubHost {
        /// 创建空测试宿主。
        fn new() -> Self {
            Self {
                values: RefCell::new(HashMap::new()),
                sessions: RefCell::new(Vec::new()),
                prompts: RefCell::new(HashMap::new()),
                read_offsets: RefCell::new(Vec::new()),
                workspace_available: true,
            }
        }

        /// 追加一份带完整元数据的会话。
        fn with_session(
            self,
            id: &str,
            revision: u64,
            title: Option<&str>,
            created_at_ms: u64,
            updated_at_ms: u64,
            messages: Vec<PluginSessionMessage>,
        ) -> Self {
            self.sessions.borrow_mut().push((
                PluginSessionSummary {
                    id: id.into(),
                    title: title.map(str::to_string),
                    created_at_ms,
                    updated_at_ms,
                    revision,
                    message_count: messages.len(),
                },
                messages,
            ));
            self
        }

        /// 替换会话消息并推进修订号与更新时间。
        fn replace_session(
            &self,
            id: &str,
            revision: u64,
            updated_at_ms: u64,
            messages: Vec<PluginSessionMessage>,
        ) {
            for (summary, stored_messages) in self.sessions.borrow_mut().iter_mut() {
                if summary.id == id {
                    summary.revision = revision;
                    summary.updated_at_ms = updated_at_ms;
                    summary.message_count = messages.len();
                    *stored_messages = messages;
                    return;
                }
            }
        }

        /// 删除一份会话，模拟用户清理历史记录。
        fn remove_session(&self, id: &str) {
            self.sessions
                .borrow_mut()
                .retain(|(summary, _)| summary.id != id);
        }

        /// 返回当前发现提示正文。
        fn injected_prompt(&self) -> String {
            self.prompts
                .borrow()
                .get(HISTORY_PROMPT_ID)
                .cloned()
                .unwrap_or_default()
        }
    }

    /// 构造一条普通文本消息。
    fn message(index: usize, role: &str, text: &str) -> PluginSessionMessage {
        PluginSessionMessage {
            index,
            role: role.into(),
            text: text.into(),
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
            has_attachments: false,
        }
    }

    /// 构造一条带工具调用和结果的助手消息。
    fn tool_message(index: usize, text: &str, tool: &str) -> PluginSessionMessage {
        PluginSessionMessage {
            index,
            role: "assistant".into(),
            text: text.into(),
            tool_calls: vec![PluginSessionToolCall {
                name: tool.into(),
                arguments: "{}".into(),
            }],
            tool_results: vec![PluginSessionToolResult {
                name: tool.into(),
                is_error: false,
                content: "完成".into(),
            }],
            has_attachments: false,
        }
    }

    /// 构造 TUI 已完成持久化后的运行结束事件。
    fn tui_finish_event(succeeded: bool, cancelled: bool) -> PluginEvent {
        PluginEvent::new(PluginEventEnvelope {
            event: TUI_RUN_FINISH_EVENT.into(),
            cancelled: false,
            canceller: None,
            subscriber: "knowledge".into(),
            data: json!({
                "succeeded": succeeded,
                "cancelled": cancelled,
            }),
        })
    }

    /// 构造插件激活上下文。
    fn activation_context() -> ActivationContext {
        ActivationContext {
            plugin_id: "knowledge".into(),
            metadata: HashMap::new(),
        }
    }

    /// 构造一次工具调用。
    fn call(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall::new("call-1", name, args)
    }

    /// 激活并返回已完成自动同步的插件。
    fn activated(host: &StubHost) -> KnowledgePlugin {
        let mut plugin = KnowledgePlugin::default();
        plugin.activate(host, activation_context()).expect("激活");
        plugin
    }

    impl PluginHostApi for StubHost {
        fn upsert_tool(&self, local_name: &str, _spec: &ToolSpec) -> Result<String> {
            Ok(local_name.to_string())
        }

        fn remove_tool(&self, _public_name: &str) -> Result<()> {
            Ok(())
        }

        fn upsert_prompt(&self, prompt: &PromptContribution) -> Result<String> {
            self.prompts
                .borrow_mut()
                .insert(prompt.id.clone(), prompt.content.clone());
            Ok(prompt.id.clone())
        }

        fn remove_prompt(&self, id: &str) -> Result<()> {
            self.prompts.borrow_mut().remove(id);
            Ok(())
        }

        fn emit_event(&self, _event: &agent_plugin::ExtensionEvent) -> Result<()> {
            Ok(())
        }

        fn get_state(&self, _key: &str) -> Result<Option<serde_json::Value>> {
            Ok(None)
        }

        fn set_state(&self, _key: &str, _value: &serde_json::Value) -> Result<()> {
            Ok(())
        }

        fn remove_state(&self, _key: &str) -> Result<Option<serde_json::Value>> {
            Ok(None)
        }

        fn store_get(
            &self,
            scope: PluginStoreScope,
            key: &str,
        ) -> Result<Option<serde_json::Value>> {
            if scope == PluginStoreScope::Workspace && !self.workspace_available {
                return Err(anyhow!("当前宿主未提供项目作用域"));
            }
            Ok(self.values.borrow().get(key).cloned())
        }

        fn store_set(
            &self,
            scope: PluginStoreScope,
            key: &str,
            value: &serde_json::Value,
        ) -> Result<()> {
            if scope == PluginStoreScope::Workspace && !self.workspace_available {
                return Err(anyhow!("当前宿主未提供项目作用域"));
            }
            self.values
                .borrow_mut()
                .insert(key.to_string(), value.clone());
            Ok(())
        }

        fn store_remove(
            &self,
            _scope: PluginStoreScope,
            key: &str,
        ) -> Result<Option<serde_json::Value>> {
            Ok(self.values.borrow_mut().remove(key))
        }

        fn store_list(&self, _scope: PluginStoreScope) -> Result<Vec<String>> {
            Ok(self.values.borrow().keys().cloned().collect())
        }

        fn session_list(&self) -> Result<Vec<PluginSessionSummary>> {
            Ok(self
                .sessions
                .borrow()
                .iter()
                .map(|(summary, _)| summary.clone())
                .collect())
        }

        fn session_read(
            &self,
            id: &str,
            offset: usize,
            limit: Option<usize>,
        ) -> Result<Option<PluginSessionPage>> {
            self.read_offsets.borrow_mut().push((id.into(), offset));
            let sessions = self.sessions.borrow();
            let Some((summary, messages)) = sessions.iter().find(|(summary, _)| summary.id == id)
            else {
                return Ok(None);
            };
            let limit = limit.unwrap_or(SESSION_PAGE_SIZE).min(SESSION_PAGE_SIZE);
            Ok(Some(PluginSessionPage {
                id: id.into(),
                message_count: summary.message_count,
                messages: messages.iter().skip(offset).take(limit).cloned().collect(),
            }))
        }

        fn complete_model(
            &self,
            _request: &ModelCompletionRequest,
        ) -> Result<ModelCompletionResponse> {
            Err(anyhow!("历史会话插件测试不提供模型调用"))
        }

        fn upsert_service(&self, _service: &ServiceSpec) -> Result<()> {
            Ok(())
        }

        fn remove_service(&self, _name: &str) -> Result<()> {
            Ok(())
        }

        fn list_services(&self, _plugin_id: Option<&str>) -> Result<Vec<ServiceDescriptor>> {
            Ok(Vec::new())
        }

        fn call_service(
            &self,
            _plugin_id: &str,
            _name: &str,
            _payload: &serde_json::Value,
        ) -> Result<serde_json::Value> {
            Err(anyhow!("测试替身不提供插件服务调用"))
        }

        fn read_file(&self, _path: &str) -> Result<String> {
            Err(anyhow!("测试替身不提供文件读取"))
        }

        fn list_dir(&self, _path: &str) -> Result<Vec<FileEntry>> {
            Ok(Vec::new())
        }

        fn spawn_process(&self, _spec: &agent_plugin::ProcessSpec) -> Result<u64> {
            Err(anyhow!("测试替身不提供子进程能力"))
        }

        fn write_process(&self, _handle: u64, _data: &str) -> Result<()> {
            Err(anyhow!("测试替身不提供子进程能力"))
        }

        fn read_process_line(&self, _handle: u64, _timeout_ms: u64) -> Result<Option<String>> {
            Ok(None)
        }

        fn kill_process(&self, _handle: u64) -> Result<()> {
            Ok(())
        }
    }

    /// 激活时按用户轮次自动切片，并保留标题、结果、工具和时间。
    #[test]
    fn activation_indexes_user_turns_without_model() {
        let host = StubHost::new().with_session(
            "s1",
            1,
            Some("修复构建"),
            1_700_000_000_000,
            1_700_000_100_000,
            vec![
                message(0, "system", "系统提示"),
                message(1, "user", "继续修复构建失败"),
                tool_message(2, "已修改配置", "process_exec"),
                message(3, "user", "再运行测试"),
                message(4, "assistant", "测试通过"),
            ],
        );

        let plugin = activated(&host);

        assert_eq!(plugin.entries.len(), 2);
        let first_turn = plugin
            .entries
            .iter()
            .find(|entry| entry.range_start == 1)
            .expect("第一轮");
        assert_eq!(first_turn.request, "继续修复构建失败");
        assert_eq!(first_turn.outcome, "已修改配置");
        assert_eq!(first_turn.tools, ["process_exec"]);
        assert_eq!(first_turn.range_end, 3);
        assert_eq!(first_turn.created_at_ms, 1_700_000_000_000);
        assert!(host.injected_prompt().contains("history_search"));
    }

    /// 长会话必须分页读取，不能把宿主的单页上限误当成完整会话。
    #[test]
    fn activation_reads_long_sessions_by_page() {
        let messages: Vec<_> = (0..130)
            .map(|index| {
                let role = if index % 2 == 0 { "user" } else { "assistant" };
                message(index, role, &format!("消息 {index}"))
            })
            .collect();
        let host = StubHost::new().with_session("long", 1, None, 1, 2, messages);

        let plugin = activated(&host);

        assert_eq!(plugin.entries.len(), 65);
        assert_eq!(
            host.read_offsets.borrow().as_slice(),
            [
                ("long".into(), 0),
                ("long".into(), 64),
                ("long".into(), 128)
            ]
        );
    }

    /// 超长片段只能保留有界首尾，并确保最终助手结果仍可参与检索。
    #[test]
    fn long_evidence_is_bounded_and_keeps_tail() {
        let host = StubHost::new().with_session(
            "long-turn",
            1,
            None,
            1,
            2,
            vec![
                message(0, "user", &format!("开头请求{}", "中".repeat(2_000))),
                message(1, "assistant", "最终结果标识 FINAL_RESULT"),
            ],
        );

        let plugin = activated(&host);
        let evidence = &plugin.entries[0].evidence;

        assert!(evidence.chars().count() <= MAX_EVIDENCE_CHARS);
        assert!(evidence.contains("开头请求"), "{evidence}");
        assert!(evidence.contains("中间内容已省略"), "{evidence}");
        assert!(evidence.contains("FINAL_RESULT"), "{evidence}");
    }

    /// 关键词检索返回可选标题、结果和三种时间表示。
    #[test]
    fn search_returns_metadata_and_time() {
        let host = StubHost::new().with_session(
            "s1",
            1,
            Some("插件构建"),
            0,
            951_782_400_123,
            vec![
                message(0, "user", "构建 knowledge-plugin"),
                message(1, "assistant", "使用 bun run build:plugin:knowledge"),
            ],
        );
        let mut plugin = activated(&host);

        let result = plugin
            .call_tool_with_host(
                &host,
                call("history_search", json!({"query": "knowledge-plugin"})),
            )
            .expect("检索");

        assert_eq!(result.content["count"], 1);
        assert_eq!(result.content["results"][0]["title"], "插件构建");
        assert_eq!(
            result.content["results"][0]["updated_at_utc"],
            "2000-02-29T00:00:00.123Z"
        );
        assert_eq!(result.content["time_granularity"], "session");
    }

    /// 省略查询时按更新时间返回最近片段，支持用户只说“继续做某事”。
    #[test]
    fn search_without_query_returns_recent_entries() {
        let host = StubHost::new()
            .with_session("old", 1, None, 1, 100, vec![message(0, "user", "旧任务")])
            .with_session("new", 1, None, 2, 200, vec![message(0, "user", "新任务")]);
        let mut plugin = activated(&host);

        let result = plugin
            .call_tool_with_host(&host, call("history_search", json!({"limit": 1})))
            .expect("读取最近历史");

        assert_eq!(result.content["results"][0]["session_id"], "new");
        assert!(result.content["results"][0]["score"].is_null());
    }

    /// 同一会话共享更新时间时，较后的用户轮次应优先用于“继续”场景。
    #[test]
    fn recent_search_prefers_later_turn_in_same_session() {
        let host = StubHost::new().with_session(
            "s1",
            1,
            None,
            1,
            2,
            vec![
                message(0, "user", "较早任务"),
                message(1, "assistant", "较早结果"),
                message(2, "user", "最近任务"),
                message(3, "assistant", "最近结果"),
            ],
        );
        let mut plugin = activated(&host);

        let result = plugin
            .call_tool_with_host(&host, call("history_search", json!({"limit": 1})))
            .expect("读取最近轮次");

        assert_eq!(result.content["results"][0]["request"], "最近任务");
        assert_eq!(result.content["results"][0]["range_start"], 2);
    }

    /// 原文回读只能落在命中片段的精确消息区间内。
    #[test]
    fn read_is_bounded_to_entry_range() {
        let host = StubHost::new().with_session(
            "s1",
            1,
            None,
            1,
            2,
            vec![
                message(0, "user", "第一件事"),
                message(1, "assistant", "第一件事完成"),
                message(2, "user", "第二件事"),
                message(3, "assistant", "第二件事完成"),
            ],
        );
        let mut plugin = activated(&host);
        let first_id = plugin
            .entries
            .iter()
            .find(|entry| entry.range_start == 0)
            .expect("第一轮")
            .id
            .clone();

        let result = plugin
            .call_tool_with_host(&host, call("history_read", json!({"entry_id": first_id})))
            .expect("回读");
        let transcript = result.content["transcript"].as_str().expect("原文");

        assert!(transcript.contains("第一件事完成"));
        assert!(!transcript.contains("第二件事"));
        assert_eq!(result.content["range_start"], 0);
        assert_eq!(result.content["range_end"], 2);
    }

    /// 成功事件重建变化会话，失败或取消事件不读取尚未确认持久化的终态。
    #[test]
    fn tui_finish_syncs_only_successful_runs() {
        let host =
            StubHost::new().with_session("s1", 1, None, 1, 2, vec![message(0, "user", "旧请求")]);
        let mut plugin = activated(&host);
        host.replace_session("s1", 2, 3, vec![message(0, "user", "新请求")]);

        plugin.on_event(&host, &mut tui_finish_event(false, false));
        assert_eq!(plugin.entries[0].request, "旧请求");
        plugin.on_event(&host, &mut tui_finish_event(true, false));
        assert_eq!(plugin.entries[0].request, "新请求");
        assert_eq!(plugin.meta.indexed.get("s1"), Some(&2));
    }

    /// 来源会话被删除后，下一次同步会移除片段与修订记录。
    #[test]
    fn deleted_sessions_are_removed_on_sync() {
        let host =
            StubHost::new().with_session("s1", 1, None, 1, 2, vec![message(0, "user", "待删除")]);
        let mut plugin = activated(&host);
        host.remove_session("s1");

        plugin.on_event(&host, &mut tui_finish_event(true, false));

        assert!(plugin.entries.is_empty());
        assert!(!plugin.meta.indexed.contains_key("s1"));
        assert!(host.injected_prompt().is_empty());
    }

    /// v1 模型蒸馏缓存升级时直接从会话原文重建 v2 索引。
    #[test]
    fn legacy_schema_is_rebuilt_from_sessions() {
        let host = StubHost::new().with_session(
            "s1",
            7,
            None,
            1,
            2,
            vec![message(0, "user", "重建后的请求")],
        );
        host.values
            .borrow_mut()
            .insert(META_STORE_KEY.into(), json!({"schema_version": 1}));
        host.values.borrow_mut().insert(
            ENTRIES_STORE_KEY.into(),
            json!([{"id": "k1", "topic": "旧摘要"}]),
        );

        let plugin = activated(&host);

        assert_eq!(plugin.meta.schema_version, 2);
        assert_eq!(plugin.entries.len(), 1);
        assert_eq!(plugin.entries[0].request, "重建后的请求");
        assert!(plugin.entries[0].id.starts_with('h'));
    }

    /// UTC 格式化覆盖 epoch、普通日期和闰日。
    #[test]
    fn utc_timestamp_handles_epoch_and_leap_day() {
        assert_eq!(format_utc_timestamp(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            format_utc_timestamp(951_782_400_123),
            "2000-02-29T00:00:00.123Z"
        );
        assert_eq!(
            format_utc_timestamp(1_788_351_696_789),
            "2026-09-02T12:21:36.789Z"
        );
    }

    /// 相对时间覆盖过去与少量未来时钟漂移。
    #[test]
    fn relative_time_handles_past_and_future() {
        assert_eq!(relative_time_at(0, 30_000), "刚刚");
        assert_eq!(relative_time_at(0, 7_200_000), "2 小时前");
        assert_eq!(relative_time_at(7_200_000, 0), "2 小时后");
    }

    /// 没有项目作用域时插件降级，并返回可读原因。
    #[test]
    fn missing_workspace_scope_degrades_with_reason() {
        let mut host = StubHost::new();
        host.workspace_available = false;
        let mut plugin = activated(&host);

        assert!(!plugin.available);
        let error = plugin
            .call_tool_with_host(&host, call("history_search", json!({})))
            .expect_err("不可用时必须报错");
        assert!(error.to_string().contains("项目作用域"), "{error}");
    }

    /// 面板跟随新旧触发前缀显示，并能用 Esc 收起。
    #[test]
    fn panel_follows_history_and_legacy_triggers() {
        let mut plugin = KnowledgePlugin::default();
        let input = |event| UiInput {
            plugin_id: "knowledge".into(),
            view_id: HISTORY_VIEW.into(),
            instance_id: None,
            event,
        };

        plugin.on_ui_input(input(UiInputEvent::MainInput {
            text: "/history".into(),
            cursor: 8,
        }));
        assert!(plugin.panel_visible);
        plugin.on_ui_input(input(UiInputEvent::Key {
            code: "escape".into(),
            modifiers: Vec::new(),
        }));
        assert!(!plugin.panel_visible);
        plugin.on_ui_input(input(UiInputEvent::MainInput {
            text: "/kb".into(),
            cursor: 3,
        }));
        assert!(plugin.panel_visible);
    }
}

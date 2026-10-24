//! 插件向 Agent 提交的动态贡献注册表。

use agent_core::model::{MessageRole, ModelMessage};
use agent_tool::ToolSpec;
use anyhow::{anyhow, Result};
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::{HashMap, VecDeque},
    sync::{RwLock, RwLockReadGuard, RwLockWriteGuard},
};

/// 限制活跃贡献而非插件一生的调用总数，排空/删除后归还预算。
const MAX_TOOLS: usize = 1024;
const MAX_PROMPTS: usize = 256;
const MAX_EVENTS: usize = 1024;
const MAX_ITEM_BYTES: usize = 1024 * 1024;
const MAX_CONTRIBUTION_BYTES: usize = 8 * 1024 * 1024;

/// 单个插件拥有的动态贡献注册表。
#[derive(Default)]
pub(crate) struct ContributionRegistry {
    inner: RwLock<ContributionState>,
}

#[derive(Default)]
struct ContributionState {
    tools: HashMap<String, RegisteredTool>,
    prompts: HashMap<String, PromptContribution>,
    events: VecDeque<QueuedEvent>,
    tool_bytes: usize,
    prompt_bytes: usize,
    event_bytes: usize,
}

/// 插件事件及其可信运行归属；`None` 表示启动或 UI 控制面事件。
struct QueuedEvent {
    run_id: Option<String>,
    payload: Value,
    bytes: usize,
}

#[derive(Clone)]
struct RegisteredTool {
    local_name: String,
    spec: ToolSpec,
    bytes: usize,
}

/// 插件注册工具时使用的 ABI 请求。
#[derive(Deserialize)]
pub(crate) struct ToolRegistrationRequest {
    /// 插件内部使用的工具 ID，不会暴露给模型。
    pub local_name: String,
    /// 暴露给模型的工具定义。
    pub spec: ToolSpec,
}

/// 插件注册提示消息时使用的 ABI 请求。
#[derive(Clone, Deserialize)]
pub(crate) struct PromptContribution {
    /// 插件内部稳定且唯一的提示 ID。
    pub id: String,
    /// 作为 developer 消息注入的提示内容。
    pub content: String,
    /// 数值较小的提示排在前面。
    #[serde(default)]
    pub priority: i32,
}

impl ContributionRegistry {
    /// 注册或替换一个插件工具，并返回公开工具名。
    pub(crate) fn upsert_tool(&self, request: ToolRegistrationRequest) -> Result<String> {
        if request.local_name.trim().is_empty() {
            return Err(anyhow!("插件本地工具 ID 不能为空"));
        }
        request.spec.validate_name()?;
        let bytes = bounded_json_size(&request.spec)?.saturating_add(request.local_name.len());
        let public_name = request.spec.name.clone();
        let mut state = self.write()?;
        let previous = state.tools.get(&public_name).map_or(0, |tool| tool.bytes);
        let total = state
            .tool_bytes
            .saturating_sub(previous)
            .saturating_add(bytes);
        ensure_budget(
            bytes,
            total,
            state.tools.len(),
            state.tools.contains_key(&public_name),
            MAX_TOOLS,
        )?;
        state.tools.insert(
            public_name.clone(),
            RegisteredTool {
                local_name: request.local_name,
                spec: request.spec,
                bytes,
            },
        );
        state.tool_bytes = total;
        Ok(public_name)
    }

    /// 注册 component 通过 `list-tools` 返回的初始静态工具。
    pub(crate) fn upsert_static_tools(&self, tools: Vec<ToolSpec>) -> Result<()> {
        for spec in tools {
            let local_name = spec.name.clone();
            self.upsert_tool(ToolRegistrationRequest { local_name, spec })?;
        }
        Ok(())
    }

    /// 删除一个公开工具；不存在时保持幂等。
    pub(crate) fn remove_tool(&self, public_name: &str) -> Result<()> {
        let mut state = self.write()?;
        if let Some(tool) = state.tools.remove(public_name) {
            state.tool_bytes -= tool.bytes;
        }
        Ok(())
    }

    /// 返回按公开名称排序的工具定义快照。
    pub(crate) fn tools(&self) -> Result<Vec<ToolSpec>> {
        let mut tools = self
            .read()?
            .tools
            .values()
            .map(|tool| tool.spec.clone())
            .collect::<Vec<_>>();
        tools.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(tools)
    }

    /// 根据公开工具名解析插件内部工具 ID。
    pub(crate) fn local_tool_name(&self, public_name: &str) -> Result<Option<String>> {
        Ok(self
            .read()?
            .tools
            .get(public_name)
            .map(|tool| tool.local_name.clone()))
    }

    /// 注册或替换一条 developer 提示贡献。
    pub(crate) fn upsert_prompt(&self, prompt: PromptContribution) -> Result<String> {
        if prompt.id.trim().is_empty() {
            return Err(anyhow!("插件提示 ID 不能为空"));
        }
        if prompt.content.trim().is_empty() {
            return Err(anyhow!("插件提示内容不能为空"));
        }
        let id = prompt.id.clone();
        let bytes = prompt.id.len().saturating_add(prompt.content.len());
        let mut state = self.write()?;
        let previous = state
            .prompts
            .get(&id)
            .map_or(0, |old| old.id.len() + old.content.len());
        let total = state
            .prompt_bytes
            .saturating_sub(previous)
            .saturating_add(bytes);
        ensure_budget(
            bytes,
            total,
            state.prompts.len(),
            state.prompts.contains_key(&id),
            MAX_PROMPTS,
        )?;
        state.prompts.insert(id.clone(), prompt);
        state.prompt_bytes = total;
        Ok(id)
    }

    /// 删除一条提示贡献；不存在时保持幂等。
    pub(crate) fn remove_prompt(&self, id: &str) -> Result<()> {
        let mut state = self.write()?;
        if let Some(prompt) = state.prompts.remove(id) {
            state.prompt_bytes -= prompt.id.len() + prompt.content.len();
        }
        Ok(())
    }

    /// 返回按优先级和 ID 稳定排序的 developer 消息。
    pub(crate) fn prompt_messages(&self) -> Result<Vec<ModelMessage>> {
        let mut prompts = self.read()?.prompts.values().cloned().collect::<Vec<_>>();
        prompts.sort_by(|left, right| {
            left.priority
                .cmp(&right.priority)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(prompts
            .into_iter()
            .map(|prompt| ModelMessage::text(MessageRole::Developer, prompt.content))
            .collect())
    }

    /// 排队一条等待 Core 发布的结构化事件。
    pub(crate) fn emit_event(&self, run_id: Option<&str>, payload: Value) -> Result<()> {
        let bytes = bounded_json_size(&payload)?.saturating_add(run_id.map_or(0, str::len));
        let mut state = self.write()?;
        let total = state.event_bytes.saturating_add(bytes);
        ensure_budget(bytes, total, state.events.len(), false, MAX_EVENTS)?;
        state.events.push_back(QueuedEvent {
            run_id: run_id.map(str::to_string),
            payload,
            bytes,
        });
        state.event_bytes = total;
        Ok(())
    }

    /// 取出未绑定 Agent run 的启动与 UI 事件，并从注册表中移除。
    pub(crate) fn drain_events(&self) -> Result<Vec<Value>> {
        self.drain_events_matching(None)
    }

    /// 取出指定 Agent run 的事件，其他并发运行和 UI 队列保持不变。
    pub(crate) fn drain_events_for_run(&self, run_id: &str) -> Result<Vec<Value>> {
        self.drain_events_matching(Some(run_id))
    }

    /// 按可信运行归属稳定地拆分事件队列。
    fn drain_events_matching(&self, run_id: Option<&str>) -> Result<Vec<Value>> {
        let mut state = self.write()?;
        let mut matched = Vec::new();
        let mut remaining = VecDeque::new();
        while let Some(event) = state.events.pop_front() {
            if event.run_id.as_deref() == run_id {
                state.event_bytes -= event.bytes;
                matched.push(event.payload);
            } else {
                remaining.push_back(event);
            }
        }
        state.events = remaining;
        Ok(matched)
    }

    fn read(&self) -> Result<RwLockReadGuard<'_, ContributionState>> {
        self.inner
            .read()
            .map_err(|_| anyhow!("插件贡献注册表读锁已中毒"))
    }

    fn write(&self) -> Result<RwLockWriteGuard<'_, ContributionState>> {
        self.inner
            .write()
            .map_err(|_| anyhow!("插件贡献注册表写锁已中毒"))
    }
}

/// 不构造第二份 JSON 字符串；超过单项上限时立即停止序列化。
fn bounded_json_size(value: &impl serde::Serialize) -> Result<usize> {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            if self.0 > MAX_ITEM_BYTES {
                return Err(std::io::Error::other("插件贡献单项超过 1 MiB"));
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Counter(0);
    serde_json::to_writer(&mut count, value)?;
    Ok(count.0)
}

fn ensure_budget(
    bytes: usize,
    total: usize,
    count: usize,
    replacing: bool,
    limit: usize,
) -> Result<()> {
    if bytes > MAX_ITEM_BYTES || total > MAX_CONTRIBUTION_BYTES || (!replacing && count >= limit) {
        return Err(anyhow!("插件活跃贡献超过条目或字节上限"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 队列满时明确拒绝，排空后归还预算，不影响其他 run 的顺序。
    #[test]
    fn event_queue_is_bounded_and_reusable() {
        let registry = ContributionRegistry::default();
        for index in 0..MAX_EVENTS {
            registry.emit_event(Some("run"), json!(index)).unwrap();
        }
        assert!(registry.emit_event(None, Value::Null).is_err());
        assert!(registry.drain_events().unwrap().is_empty());
        let events = registry.drain_events_for_run("run").unwrap();
        assert_eq!(events.len(), MAX_EVENTS);
        assert_eq!(events[0], json!(0));
        registry.emit_event(None, Value::Null).unwrap();
        assert!(registry
            .emit_event(None, json!("x".repeat(MAX_ITEM_BYTES)))
            .is_err());
    }

    /// 替换不消耗额外名额，失败不能覆盖旧提示，删除后可重新注册。
    #[test]
    fn prompt_limits_preserve_existing_contributions() {
        let registry = ContributionRegistry::default();
        for index in 0..MAX_PROMPTS {
            registry
                .upsert_prompt(PromptContribution {
                    id: index.to_string(),
                    content: "ok".into(),
                    priority: 0,
                })
                .unwrap();
        }
        assert!(registry
            .upsert_prompt(PromptContribution {
                id: "extra".into(),
                content: "ok".into(),
                priority: 0
            })
            .is_err());
        registry
            .upsert_prompt(PromptContribution {
                id: "0".into(),
                content: "updated".into(),
                priority: 0,
            })
            .unwrap();
        registry.remove_prompt("0").unwrap();
        registry
            .upsert_prompt(PromptContribution {
                id: "extra".into(),
                content: "ok".into(),
                priority: 0,
            })
            .unwrap();
    }

    /// 验证公开工具名与插件本地工具 ID 分开保存。
    #[test]
    fn tool_registration_preserves_local_name() {
        let registry = ContributionRegistry::default();
        registry
            .upsert_tool(ToolRegistrationRequest {
                local_name: "remote/read_item".into(),
                spec: ToolSpec::new(
                    "extension_read_item",
                    "读取远端项目",
                    json!({"type": "object"}),
                ),
            })
            .expect("工具应注册成功");

        assert_eq!(
            registry
                .local_tool_name("extension_read_item")
                .expect("查询不应失败")
                .as_deref(),
            Some("remote/read_item")
        );
    }

    /// 并发运行事件必须按可信 run_id 排空，启动与其他运行队列不得被提前消费。
    #[test]
    fn events_are_drained_by_trusted_run_id() {
        let registry = ContributionRegistry::default();
        registry
            .emit_event(None, json!({"event": "ui"}))
            .expect("写入 UI 事件");
        registry
            .emit_event(Some("run-a"), json!({"event": "a"}))
            .expect("写入首个运行事件");
        registry
            .emit_event(Some("run-b"), json!({"event": "b"}))
            .expect("写入第二个运行事件");

        assert_eq!(
            registry
                .drain_events_for_run("run-a")
                .expect("排空首个运行"),
            [json!({"event": "a"})]
        );
        assert_eq!(
            registry.drain_events().expect("排空 UI 事件"),
            [json!({"event": "ui"})]
        );
        assert_eq!(
            registry
                .drain_events_for_run("run-b")
                .expect("排空第二个运行"),
            [json!({"event": "b"})]
        );
    }
}

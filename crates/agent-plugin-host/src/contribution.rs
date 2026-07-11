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

/// 单个插件拥有的动态贡献注册表。
#[derive(Default)]
pub(crate) struct ContributionRegistry {
    inner: RwLock<ContributionState>,
}

#[derive(Default)]
struct ContributionState {
    tools: HashMap<String, RegisteredTool>,
    prompts: HashMap<String, PromptContribution>,
    events: VecDeque<Value>,
}

#[derive(Clone)]
struct RegisteredTool {
    local_name: String,
    spec: ToolSpec,
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
        let public_name = request.spec.name.clone();
        self.write()?.tools.insert(
            public_name.clone(),
            RegisteredTool {
                local_name: request.local_name,
                spec: request.spec,
            },
        );
        Ok(public_name)
    }

    /// 注册旧 ABI 通过 `list-tools` 返回的静态工具。
    pub(crate) fn upsert_legacy_tools(&self, tools: Vec<ToolSpec>) -> Result<()> {
        for spec in tools {
            let local_name = spec.name.clone();
            self.upsert_tool(ToolRegistrationRequest { local_name, spec })?;
        }
        Ok(())
    }

    /// 删除一个公开工具；不存在时保持幂等。
    pub(crate) fn remove_tool(&self, public_name: &str) -> Result<()> {
        self.write()?.tools.remove(public_name);
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
        self.write()?.prompts.insert(id.clone(), prompt);
        Ok(id)
    }

    /// 删除一条提示贡献；不存在时保持幂等。
    pub(crate) fn remove_prompt(&self, id: &str) -> Result<()> {
        self.write()?.prompts.remove(id);
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
    pub(crate) fn emit_event(&self, payload: Value) -> Result<()> {
        self.write()?.events.push_back(payload);
        Ok(())
    }

    /// 取出全部待发布事件，并从注册表中移除。
    pub(crate) fn drain_events(&self) -> Result<Vec<Value>> {
        Ok(self.write()?.events.drain(..).collect())
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
}

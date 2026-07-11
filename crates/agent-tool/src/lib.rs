//! Tool abstractions for ascnet-lucia.
//!
//! 工具抽象层：这里不关心 OpenAI / Anthropic，也不关心 WASM 运行时。
//! Tool layer: this crate does not know about OpenAI / Anthropic or the WASM runtime.

#![deny(missing_docs)]

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc};

/// JSON Schema for a tool input.
/// 工具输入参数的 JSON Schema。
pub type JsonSchema = Value;

/// A tool definition visible to the model.
/// 暴露给模型看的工具定义。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolSpec {
    /// Stable tool name. Use snake_case; model providers are usually strict about names.
    /// 稳定工具名。建议使用 snake_case；模型服务商通常对名称较严格。
    pub name: String,

    /// Human-readable description used by the model for tool selection.
    /// 给模型看的描述，用于帮助模型选择是否调用该工具。
    pub description: String,

    /// JSON Schema object that describes the expected input arguments.
    /// 描述工具输入参数的 JSON Schema 对象。
    pub input_schema: JsonSchema,
}

impl ToolSpec {
    /// Build a new tool spec.
    /// 创建一个新的工具定义。
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: JsonSchema,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
        }
    }

    /// A schema for tools that do not require arguments.
    /// 无参数工具可使用的空对象 schema。
    pub fn empty_object_schema() -> JsonSchema {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    /// Validate the subset of naming rules accepted by common providers.
    /// 校验常见模型服务商都能接受的工具命名子集。
    pub fn validate_name(&self) -> Result<()> {
        validate_tool_name(&self.name)
    }
}

/// A tool call requested by the model.
/// 模型请求执行的一次工具调用。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    /// Provider-neutral call id. It must be returned with the tool result.
    /// 与服务商无关的调用 ID，工具结果必须带回这个 ID。
    pub id: String,

    /// Tool name.
    /// 工具名称。
    pub name: String,

    /// Parsed JSON arguments.
    /// 已解析的 JSON 参数。
    pub args: Value,
}

impl ToolCall {
    /// Construct a tool call.
    /// 构造一次工具调用。
    pub fn new(id: impl Into<String>, name: impl Into<String>, args: Value) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            args,
        }
    }

    /// Deserialize arguments into a typed Rust value.
    /// 将工具参数反序列化为强类型 Rust 值。
    pub fn args_as<T: DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_value(self.args.clone())
            .map_err(|err| anyhow!("invalid arguments for tool `{}`: {err}", self.name))
    }

    /// Serialize arguments back to JSON text for provider protocols.
    /// 将工具参数重新序列化为 JSON 文本，供模型协议适配层使用。
    pub fn args_json_string(&self) -> String {
        serde_json::to_string(&self.args).unwrap_or_else(|_| "{}".to_string())
    }
}

/// Result returned by a tool.
/// 工具执行返回的结果。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolResult {
    /// The id of the tool call this result belongs to.
    /// 该结果对应的工具调用 ID。
    pub call_id: String,

    /// Tool name. This is redundant but useful for logs and plugin routing.
    /// 工具名。虽然和 call_id 有冗余，但对日志和插件路由很有用。
    pub name: String,

    /// JSON payload returned to the model.
    /// 返回给模型的 JSON 载荷。
    pub content: Value,

    /// Whether the tool failed. The ReAct loop still returns the error to the model.
    /// 工具是否失败。即使失败，ReAct loop 也会把错误结果回传给模型。
    pub is_error: bool,

    /// UI 专用的结构化细节（diff 预览、执行耗时等），不会发送给模型。
    /// content 与 details 分离：模型只看 content，UI 只看 details。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl ToolResult {
    /// Construct a successful tool result.
    /// 构造成功的工具结果。
    pub fn success(call_id: impl Into<String>, name: impl Into<String>, content: Value) -> Self {
        Self {
            call_id: call_id.into(),
            name: name.into(),
            content,
            is_error: false,
            details: None,
        }
    }

    /// Construct an error tool result.
    /// 构造失败的工具结果。
    pub fn error(
        call_id: impl Into<String>,
        name: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            call_id: call_id.into(),
            name: name.into(),
            content: json!({ "error": message.into() }),
            is_error: true,
            details: None,
        }
    }

    /// 以 builder 风格附加 UI 细节。
    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    /// Convert the result payload to provider-friendly text.
    /// 将工具结果转为模型服务商协议更容易接受的文本。
    pub fn content_text(&self) -> String {
        if let Some(text) = self.content.as_str() {
            text.to_string()
        } else {
            serde_json::to_string(&self.content).unwrap_or_else(|_| "null".to_string())
        }
    }
}

/// Trait implemented by host-native tools.
/// 宿主进程内原生工具需要实现的 trait。
#[async_trait]
pub trait Tool: Send + Sync {
    /// Return the tool definition sent to the model.
    /// 返回发送给模型的工具定义。
    fn spec(&self) -> ToolSpec;

    /// Execute the tool call.
    /// 执行工具调用。
    async fn call(&self, call: ToolCall) -> Result<ToolResult>;
}

/// Async function type used by [`JsonTool`].
/// [`JsonTool`] 使用的异步函数类型。
pub type BoxedToolFuture = Pin<Box<dyn Future<Output = Result<Value>> + Send>>;

/// A convenient tool wrapper around an async JSON function.
/// 一个便捷工具包装器：把异步 JSON 函数包装成 Tool。
pub struct JsonTool {
    spec: ToolSpec,
    handler: Arc<dyn Fn(Value) -> BoxedToolFuture + Send + Sync>,
}

impl JsonTool {
    /// Create a tool from a spec and an async handler.
    /// 用工具定义和异步处理函数创建工具。
    pub fn new<F, Fut>(spec: ToolSpec, handler: F) -> Self
    where
        F: Fn(Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value>> + Send + 'static,
    {
        Self {
            spec,
            handler: Arc::new(move |args| Box::pin(handler(args))),
        }
    }
}

#[async_trait]
impl Tool for JsonTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    async fn call(&self, call: ToolCall) -> Result<ToolResult> {
        let content = (self.handler)(call.args.clone()).await?;
        Ok(ToolResult::success(call.id, call.name, content))
    }
}

/// Registry for host-native tools.
/// 宿主进程内原生工具注册表。
#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// Create an empty registry.
    /// 创建空注册表。
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tool.
    /// 注册一个工具。
    pub fn register<T>(&mut self, tool: T) -> Result<&mut Self>
    where
        T: Tool + 'static,
    {
        self.register_arc(Arc::new(tool))
    }

    /// Register an already shared tool.
    /// 注册一个已经被 Arc 包装的工具。
    pub fn register_arc(&mut self, tool: Arc<dyn Tool>) -> Result<&mut Self> {
        let spec = tool.spec();
        spec.validate_name()?;
        if self.tools.contains_key(&spec.name) {
            return Err(anyhow!("duplicated tool: {}", spec.name));
        }
        self.tools.insert(spec.name, tool);
        Ok(self)
    }

    /// Check if the registry contains a tool.
    /// 检查注册表是否包含某个工具。
    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// 按名称获取共享工具实例。
    ///
    /// 返回的实例与当前注册表共享所有权，可用于构造权限收缩后的独立注册表。
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// 按名称构造当前注册表的子集。
    ///
    /// 子集复用原工具实例，但拥有独立的名称映射。名称不存在或输入包含重复名称时返回错误。
    pub fn subset<I, S>(&self, names: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut subset = Self::new();
        for name in names {
            let name = name.as_ref();
            let tool = self
                .get(name)
                .ok_or_else(|| anyhow!("unknown tool: {name}"))?;
            subset.register_arc(tool)?;
        }
        Ok(subset)
    }

    /// Return all tool specs.
    /// 返回所有工具定义。
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|tool| tool.spec()).collect()
    }

    /// Execute a tool call.
    /// 执行一次工具调用。
    pub async fn call(&self, call: ToolCall) -> Result<ToolResult> {
        let Some(tool) = self.tools.get(&call.name) else {
            return Err(anyhow!("unknown tool: {}", call.name));
        };
        tool.call(call).await
    }

    /// Number of registered tools.
    /// 已注册工具数量。
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether no tools are registered.
    /// 是否没有注册任何工具。
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

#[cfg(feature = "builtins")]
pub mod builtins;

/// Validate a provider-portable tool name.
/// 校验跨服务商可移植的工具名称。
pub fn validate_tool_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(anyhow!("tool name cannot be empty"));
    }
    if name.len() > 64 {
        return Err(anyhow!("tool name cannot be longer than 64 characters"));
    }
    let ok = name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-');
    if !ok {
        return Err(anyhow!(
            "tool name `{name}` may only contain ASCII letters, digits, `_`, or `-`"
        ));
    }
    Ok(())
}

//! Agent 模型请求的上下文加载接口。

use crate::model::ModelMessage;
use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// 兼容同步调用方的上下文变换函数。
pub type ContextTransform = dyn Fn(Vec<ModelMessage>) -> Vec<ModelMessage> + Send + Sync;

/// 单次模型请求传给上下文加载器的数据。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ContextLoadRequest {
    /// 当前 Agent run 的稳定 ID。
    pub run_id: String,
    /// 当前 ReAct step，从零开始。
    pub step: usize,
    /// 当前逻辑 provider 名称。
    pub provider: String,
    /// 当前模型 ID。
    pub model: String,
    /// 会话顶层 system 提示。
    pub system: Option<String>,
    /// 扩展提示与完整会话消息组成的原始上下文。
    pub messages: Vec<ModelMessage>,
    /// 是否由用户显式发起（而非模型请求前的自动加载）。
    /// 加载器可据此跳过内部水位判断，无条件执行完整处理。
    #[serde(default)]
    pub user_initiated: bool,
}

/// 上下文加载器为一次模型请求返回的完整上下文。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LoadedContext {
    /// 实际发送给模型的顶层 system 提示。
    pub system: Option<String>,
    /// 实际发送给模型的全部消息，完整替换请求中的原始消息。
    pub messages: Vec<ModelMessage>,
}

impl LoadedContext {
    /// 创建一份完整替换上下文。
    pub fn new(system: Option<String>, messages: Vec<ModelMessage>) -> Self {
        Self { system, messages }
    }

    /// 从加载请求创建不做修改的上下文。
    pub fn passthrough(request: ContextLoadRequest) -> Self {
        Self {
            system: request.system,
            messages: request.messages,
        }
    }
}

/// 为每次模型请求加载实际上下文的通用接口。
///
/// 返回值具有完整替换语义。实现返回错误时 Agent 会终止当前 run，绝不会静默回退到
/// 完整历史，因此裁剪、摘要或外部上下文管理可以可靠控制模型实际看到的内容。
#[async_trait]
pub trait ContextLoader: Send + Sync {
    /// 加载一次模型请求使用的完整上下文。
    async fn load(&self, request: ContextLoadRequest) -> Result<LoadedContext>;
}

/// 不修改任何上下文的默认加载器。
#[derive(Debug, Clone, Default)]
pub struct PassthroughContextLoader;

#[async_trait]
impl ContextLoader for PassthroughContextLoader {
    async fn load(&self, request: ContextLoadRequest) -> Result<LoadedContext> {
        Ok(LoadedContext::passthrough(request))
    }
}

/// 把同步 [`ContextTransform`] 适配为异步 [`ContextLoader`]。
#[derive(Clone)]
pub struct TransformContextLoader {
    transform: Arc<ContextTransform>,
}

impl TransformContextLoader {
    /// 创建同步变换适配器。
    pub fn new(transform: Arc<ContextTransform>) -> Self {
        Self { transform }
    }
}

#[async_trait]
impl ContextLoader for TransformContextLoader {
    async fn load(&self, request: ContextLoadRequest) -> Result<LoadedContext> {
        Ok(LoadedContext {
            system: request.system,
            messages: (self.transform)(request.messages),
        })
    }
}

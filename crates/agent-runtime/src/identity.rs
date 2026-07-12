//! Runtime principal、Agent 身份与派生谱系。

use crate::{AgentRuntimeError, RuntimeResult};
use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};
use uuid::Uuid;

/// Runtime 内稳定且不可伪造的 Agent 身份。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentId(Uuid);

impl AgentId {
    /// 生成一个随机 Agent 身份。
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// 返回底层 UUID。
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl Default for AgentId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for AgentId {
    type Err = uuid::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(value).map(Self)
    }
}

/// Host 为一次受信任组件生命周期分配的 owner principal。
///
/// Runtime 不解释命名空间或业务含义；Host 可使用插件 ID、租户 ID 或其他稳定主体，
/// 并建议为每次激活附加唯一代次，避免撤销后的 principal 被复用。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RuntimePrincipal(String);

impl RuntimePrincipal {
    /// 创建通用 owner principal。
    ///
    /// 空字符串、首尾空白或超过 256 字节的值会被拒绝。
    pub fn new(value: impl Into<String>) -> RuntimeResult<Self> {
        let value = value.into();
        if value.is_empty() || value.trim() != value || value.len() > 256 {
            return Err(AgentRuntimeError::InvalidPrincipal(value));
        }
        Ok(Self(value))
    }

    /// 返回供仅有一个宿主 owner 的原生集成使用的默认 principal。
    pub fn host() -> Self {
        Self("host".to_string())
    }

    /// 返回 principal 的不透明字符串值。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RuntimePrincipal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Host 注册的命名 Agent 派生 profile 标识。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentProfileId(String);

impl AgentProfileId {
    /// 创建命名 profile 标识。
    ///
    /// 标识只允许 ASCII 字母、数字、点、下划线和连字符，长度为 1 到 128 字节。
    pub fn new(value: impl Into<String>) -> RuntimeResult<Self> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
        if !valid {
            return Err(AgentRuntimeError::InvalidProfileId(value));
        }
        Ok(Self(value))
    }

    /// 返回 profile 的稳定字符串值。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AgentProfileId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Agent 的父子谱系信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentLineage {
    /// 直接父节点；根节点为 `None`。
    pub parent: Option<AgentId>,
    /// 整棵派生树的根节点。
    pub root: AgentId,
    /// 当前节点深度；根节点为零。
    pub depth: usize,
}

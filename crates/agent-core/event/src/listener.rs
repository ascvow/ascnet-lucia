//! 监听者契约、优先级与单次分发上下文。

use crate::{cancel::Cancellation, data::EventData};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 监听者执行优先级。
///
/// 分发按枚举声明顺序从先到后执行，同一优先级内按订阅登记顺序执行。语义参考
/// Bukkit/Nukkit：越早执行的监听者越"外围"，最终决定权留给靠后的监听者；
/// [`ListenerPriority::Monitor`] 只用于观察最终结果，不允许改写或取消事件。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ListenerPriority {
    /// 最先执行，最容易被后续监听者推翻。
    Lowest,
    /// 较早执行。
    Low,
    /// 默认优先级。
    #[default]
    Normal,
    /// 较晚执行。
    High,
    /// 最后一个仍可改写事件的优先级，适合安全拦截。
    Highest,
    /// 在全部改写完成后执行的只读观察级，改写与取消都不会生效。
    Monitor,
}

/// 监听者返回错误时总线的处置方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListenerFailure {
    /// 记录错误后继续分发，事件语义不变。适合观察型监听者。
    #[default]
    Ignore,
    /// 记录错误并立即以该监听者为 `canceller` 取消事件。
    ///
    /// 安全拦截型监听者必须使用该模式：插件崩溃或超时不能被当作放行。
    CancelEvent,
}

/// 单个监听者在一次分发中报告的错误。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListenerError {
    /// 报错的订阅者身份。
    pub subscriber: String,
    /// 错误的完整链式描述。
    pub error: String,
}

/// 一次分发中提供给单个监听者的事件视图。
///
/// 上下文同时暴露强类型入口和 JSON 入口：进程内监听者用
/// [`EventContext::event_mut`] 直接改写字段，跨进程监听者用
/// [`EventContext::snapshot`] 与 [`EventContext::merge`] 走白名单通道。
pub struct EventContext<'a> {
    name: &'a str,
    subscriber: &'a str,
    read_only: bool,
    data: &'a mut dyn EventData,
    cancellation: &'a mut Cancellation,
}

impl<'a> EventContext<'a> {
    /// 构造一次分发上下文；仅由总线内部调用。
    pub(crate) fn new(
        name: &'a str,
        subscriber: &'a str,
        read_only: bool,
        data: &'a mut dyn EventData,
        cancellation: &'a mut Cancellation,
    ) -> Self {
        Self {
            name,
            subscriber,
            read_only,
            data,
            cancellation,
        }
    }

    /// 当前事件名。
    pub fn name(&self) -> &str {
        self.name
    }

    /// 当前监听者的订阅身份，也是它取消事件时写入的 `canceller`。
    pub fn subscriber(&self) -> &str {
        self.subscriber
    }

    /// 当前监听者是否为只读（[`ListenerPriority::Monitor`]）。
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// 按具体类型只读访问事件；类型不匹配时返回 `None`。
    pub fn event<E: EventData>(&self) -> Option<&E> {
        self.data.as_any().downcast_ref::<E>()
    }

    /// 按具体类型可变访问事件；只读监听者或类型不匹配时返回 `None`。
    pub fn event_mut<E: EventData>(&mut self) -> Option<&mut E> {
        if self.read_only {
            return None;
        }
        self.data.as_any_mut().downcast_mut::<E>()
    }

    /// 生成事件的完整 JSON 快照。
    ///
    /// # Errors
    ///
    /// 事件负载无法序列化时返回错误。
    pub fn snapshot(&self) -> Result<Value> {
        self.data.snapshot()
    }

    /// 通过事件声明的白名单合并一份 JSON 补丁。
    ///
    /// # Errors
    ///
    /// 只读监听者调用，或补丁中白名单字段取值不合法时返回错误。
    pub fn merge(&mut self, patch: &Value) -> Result<()> {
        if self.read_only {
            return Err(anyhow!(
                "monitor 优先级监听者 `{}` 不得改写事件 `{}`",
                self.subscriber,
                self.name
            ));
        }
        self.data.merge(patch)
    }

    /// 取消事件，并在首次取消时把当前订阅者记为 `canceller`。
    ///
    /// 返回 `true` 表示本次调用真正取消了事件。只读监听者调用无效果并返回 `false`。
    pub fn cancel(&mut self) -> bool {
        if self.read_only {
            return false;
        }
        self.cancellation.cancel(self.subscriber)
    }

    /// 事件当前是否已被取消。
    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    /// 返回首个取消者身份。
    pub fn canceller(&self) -> Option<&str> {
        self.cancellation.canceller()
    }
}

/// 事件监听者。
///
/// 实现方应保持短小且不阻塞：一次分发会串行调用全部监听者，慢监听者会直接拖慢
/// ReAct 循环或界面响应。
#[async_trait]
pub trait EventListener: Send + Sync {
    /// 处理一次事件分发。
    ///
    /// # Errors
    ///
    /// 返回错误的后果由订阅时声明的 [`ListenerFailure`] 决定，不会中断其余监听者。
    async fn handle(&self, ctx: &mut EventContext<'_>) -> Result<()>;
}

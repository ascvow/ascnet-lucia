//! 可取消事件总线原语。
//!
//! 本 crate 只提供与业务无关的分发机制，不认识 Agent、工具或界面：
//!
//! - [`Cancellation`]：`is_cancel` + `canceller` 的单调取消状态，首个取消者不可被覆盖；
//! - [`EventData`] / [`Event`]：事件负载，同时暴露强类型入口与受白名单约束的 JSON 入口；
//! - [`ListenerPriority`] / [`ListenerFailure`]：监听者的执行顺序与故障处置方式；
//! - [`EventBus`]：按事件名路由的订阅表与一次分发。
//!
//! 各层各自定义事件类型：`agent-core` 定义 `core.*`，TUI 定义 `tui.*`，插件宿主把
//! 不可信插件桥接为普通监听者。层与层之间只共享事件名和 JSON 形状，不共享实现。
//!
//! # 信任边界
//!
//! 不可信监听者只能通过 [`EventData::merge`] 改写事件，而 `merge` 由事件类型自己实现，
//! 默认拒绝一切改写。调用 ID、运行 ID、权限 scope 等宿主绑定字段不应出现在任何事件的
//! 白名单里；即使出现，调用方仍应在分发返回后重新绑定可信值。

mod bus;
mod cancel;
mod data;
mod listener;

pub use bus::{DispatchOutcome, Dispatched, EventBus, Subscribe, SubscriptionId};
pub use cancel::Cancellation;
pub use data::{Event, EventData};
pub use listener::{EventContext, EventListener, ListenerError, ListenerFailure, ListenerPriority};

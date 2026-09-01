//! 总线事件负载的动态表示。

use anyhow::Result;
use serde_json::Value;
use std::any::Any;

/// 可在总线上传递的事件负载。
///
/// 同一个事件需要面向两类监听者：
///
/// - **进程内可信监听者**通过 [`Any`] 向下转型拿到具体类型，直接读写字段；
/// - **不可信监听者**（WASM 插件等）只能看到 [`EventData::snapshot`] 产生的 JSON，
///   并通过 [`EventData::merge`] 提交补丁。
///
/// `merge` 是本项目的信任边界所在：实现方必须只接受本事件显式声明为可改写的字段，
/// 其余字段一律忽略。默认实现拒绝一切改写，因此新增事件在没有主动开放字段前是安全的。
pub trait EventData: Any + Send + Sync {
    /// 返回事件名，决定订阅路由。
    fn event_name(&self) -> &'static str;

    /// 生成面向不可信监听者的完整 JSON 快照。
    ///
    /// # Errors
    ///
    /// 事件负载无法序列化为 JSON 时返回错误。
    fn snapshot(&self) -> Result<Value>;

    /// 合并不可信监听者返回的补丁。
    ///
    /// 实现方只应采纳白名单字段；补丁中的其他字段必须静默忽略，不得据此改写调用身份、
    /// 运行 ID、权限 scope 等由宿主绑定的可信字段。
    ///
    /// # Errors
    ///
    /// 白名单字段存在但取值不合法时返回错误；未知字段不构成错误。
    fn merge(&mut self, patch: &Value) -> Result<()> {
        let _ = patch;
        Ok(())
    }

    /// 只读向下转型入口。
    fn as_any(&self) -> &dyn Any;

    /// 可变向下转型入口。
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

/// 拥有静态事件名的事件类型。
///
/// 宿主侧以该常量订阅和分发，避免事件名散落为字符串字面量。
pub trait Event: EventData + Sized {
    /// 事件名，约定使用 `<层>.<域>.<动作>` 形式，例如 `core.tool.call`。
    const NAME: &'static str;
}

/// 为实现了 [`serde::Serialize`] 的类型生成 [`Event`] 与 [`EventData`] 实现。
///
/// 不带 `merge` 时事件对不可信监听者只读，只能被观察或取消。需要开放可改写字段时用
/// `merge = <表达式>` 提供签名为 `fn(&mut $ty, &serde_json::Value) -> anyhow::Result<()>`
/// 的白名单合并函数，该函数是该事件面向不可信监听者的唯一写入口。
///
/// ```ignore
/// declare_event!(ToolCallEvent, "core.tool.call", merge = ToolCallEvent::merge_patch);
/// ```
#[macro_export]
macro_rules! declare_event {
    ($ty:ty, $name:literal) => {
        $crate::declare_event!(
            @impl $ty,
            $name,
            |_event: &mut $ty, _patch: &::serde_json::Value| ::anyhow::Ok(())
        );
    };
    ($ty:ty, $name:literal, merge = $merge:expr) => {
        $crate::declare_event!(@impl $ty, $name, $merge);
    };
    (@impl $ty:ty, $name:literal, $merge:expr) => {
        impl $crate::Event for $ty {
            const NAME: &'static str = $name;
        }

        impl $crate::EventData for $ty {
            fn event_name(&self) -> &'static str {
                $name
            }

            fn snapshot(&self) -> ::anyhow::Result<::serde_json::Value> {
                ::serde_json::to_value(self).map_err(::std::convert::Into::into)
            }

            fn merge(&mut self, patch: &::serde_json::Value) -> ::anyhow::Result<()> {
                let merge: fn(&mut $ty, &::serde_json::Value) -> ::anyhow::Result<()> = $merge;
                merge(self, patch)
            }

            fn as_any(&self) -> &dyn ::std::any::Any {
                self
            }

            fn as_any_mut(&mut self) -> &mut dyn ::std::any::Any {
                self
            }
        }
    };
}

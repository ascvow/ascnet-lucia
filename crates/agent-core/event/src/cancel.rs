//! 事件取消状态。

use serde::{Deserialize, Serialize};

/// 一个事件的取消状态。
///
/// 取消是**单调**的：一旦被取消就无法恢复，`canceller` 只记录首个取消者且此后不可修改。
/// 这条规则是安全前提——如果允许撤销，低优先级监听者就能推翻高优先级安全监听者做出的
/// 拦截决定，取消语义也不再能作为权限边界使用。
///
/// 线上表示与插件 ABI 一致：`cancelled` 为布尔标志，`canceller` 为取消者身份。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cancellation {
    /// 事件是否已被取消。
    #[serde(default)]
    cancelled: bool,

    /// 首个取消该事件的监听者身份；取消后不再变化。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    canceller: Option<String>,
}

impl Cancellation {
    /// 创建未取消状态。
    pub fn new() -> Self {
        Self::default()
    }

    /// 创建由指定监听者取消的状态，供测试与跨层还原使用。
    pub fn cancelled_by(canceller: impl Into<String>) -> Self {
        Self {
            cancelled: true,
            canceller: Some(canceller.into()),
        }
    }

    /// 取消事件并在首次取消时记录 `canceller`。
    ///
    /// 返回 `true` 表示本次调用真正把事件从未取消变为取消；重复取消返回 `false`
    /// 且不会覆盖已记录的取消者。
    pub fn cancel(&mut self, canceller: impl Into<String>) -> bool {
        if self.cancelled {
            return false;
        }
        self.cancelled = true;
        self.canceller = Some(canceller.into());
        true
    }

    /// 事件是否已被取消。
    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    /// 返回首个取消者身份；未取消时为 `None`。
    pub fn canceller(&self) -> Option<&str> {
        self.canceller.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::Cancellation;

    /// 首个取消者写入 canceller，后续取消不得改写取消归属。
    #[test]
    fn first_canceller_wins_and_is_immutable() {
        let mut cancellation = Cancellation::new();

        assert!(cancellation.cancel("permission-plugin"));
        assert!(!cancellation.cancel("audit-plugin"));

        assert!(cancellation.is_cancelled());
        assert_eq!(cancellation.canceller(), Some("permission-plugin"));
    }

    /// 线上字段名必须与插件 ABI 约定一致，未取消时省略 canceller。
    #[test]
    fn wire_format_exposes_cancelled_and_canceller() {
        let pristine = serde_json::to_value(Cancellation::new()).expect("取消状态应可序列化");
        assert_eq!(pristine, serde_json::json!({ "cancelled": false }));

        let cancelled =
            serde_json::to_value(Cancellation::cancelled_by("plugin")).expect("取消状态应可序列化");
        assert_eq!(
            cancelled,
            serde_json::json!({ "cancelled": true, "canceller": "plugin" })
        );
    }
}

use super::*;
use crate::declare_event;
use anyhow::Result;
use async_trait::async_trait;
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::Mutex;

/// 测试事件：`note` 对不可信监听者开放，`bound_id` 模拟宿主绑定字段。
#[derive(Debug, Clone, Serialize)]
struct SampleEvent {
    note: String,
    bound_id: String,
}

impl SampleEvent {
    /// 只接受 `note` 字段；宿主绑定字段永远不进入白名单。
    fn merge_patch(&mut self, patch: &Value) -> Result<()> {
        if let Some(note) = patch.get("note") {
            self.note = note
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("note 必须是字符串"))?
                .to_string();
        }
        Ok(())
    }
}

declare_event!(SampleEvent, "test.sample", merge = SampleEvent::merge_patch);

/// 只读事件：不声明 merge，任何补丁都不生效。
#[derive(Debug, Clone, Serialize)]
struct ReadOnlyEvent {
    note: String,
}

declare_event!(ReadOnlyEvent, "test.readonly");

/// 按脚本执行取消、改写与记录的测试监听者。
struct ScriptedListener {
    cancel: bool,
    rename: Option<&'static str>,
    patch: Option<Value>,
    fail: bool,
    order: Arc<Mutex<Vec<String>>>,
}

impl ScriptedListener {
    fn new(order: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            cancel: false,
            rename: None,
            patch: None,
            fail: false,
            order,
        }
    }

    fn cancelling(mut self) -> Self {
        self.cancel = true;
        self
    }

    fn renaming(mut self, note: &'static str) -> Self {
        self.rename = Some(note);
        self
    }

    fn patching(mut self, patch: Value) -> Self {
        self.patch = Some(patch);
        self
    }

    fn failing(mut self) -> Self {
        self.fail = true;
        self
    }
}

#[async_trait]
impl EventListener for ScriptedListener {
    async fn handle(&self, ctx: &mut EventContext<'_>) -> Result<()> {
        self.order
            .lock()
            .expect("顺序记录锁不应中毒")
            .push(ctx.subscriber().to_string());
        if let Some(note) = self.rename {
            if let Some(event) = ctx.event_mut::<SampleEvent>() {
                event.note = note.to_string();
            }
        }
        if let Some(patch) = &self.patch {
            ctx.merge(patch)?;
        }
        if self.cancel {
            ctx.cancel();
        }
        if self.fail {
            return Err(anyhow::anyhow!("监听者内部失败"));
        }
        Ok(())
    }
}

fn order_log() -> Arc<Mutex<Vec<String>>> {
    Arc::new(Mutex::new(Vec::new()))
}

fn sample() -> SampleEvent {
    SampleEvent {
        note: "原始".into(),
        bound_id: "call-1".into(),
    }
}

/// 无订阅者时分发不产生取消状态，也不报错。
#[tokio::test]
async fn dispatch_without_listeners_is_inert() {
    let bus = EventBus::new();

    let dispatched = bus.dispatch(sample()).await;

    assert!(!dispatched.is_cancelled());
    assert!(dispatched.errors.is_empty());
    assert_eq!(dispatched.event.note, "原始");
}

/// 监听者按优先级从低到高执行，同优先级保持登记顺序。
#[tokio::test]
async fn listeners_run_in_priority_then_registration_order() {
    let bus = EventBus::new();
    let order = order_log();

    for (subscriber, priority) in [
        ("normal-a", ListenerPriority::Normal),
        ("highest", ListenerPriority::Highest),
        ("lowest", ListenerPriority::Lowest),
        ("normal-b", ListenerPriority::Normal),
        ("monitor", ListenerPriority::Monitor),
    ] {
        bus.subscribe(
            Subscribe::typed::<SampleEvent>(subscriber).with_priority(priority),
            Arc::new(ScriptedListener::new(order.clone())),
        );
    }

    bus.dispatch(sample()).await;

    assert_eq!(
        order.lock().expect("顺序记录锁不应中毒").as_slice(),
        ["lowest", "normal-a", "normal-b", "highest", "monitor"]
    );
}

/// 首个取消者写入 canceller，后续取消者无法改写取消归属。
#[tokio::test]
async fn first_canceller_owns_the_cancellation() {
    let bus = EventBus::new();
    let order = order_log();

    bus.subscribe(
        Subscribe::typed::<SampleEvent>("first").with_priority(ListenerPriority::Low),
        Arc::new(ScriptedListener::new(order.clone()).cancelling()),
    );
    bus.subscribe(
        Subscribe::typed::<SampleEvent>("second").with_priority(ListenerPriority::High),
        Arc::new(ScriptedListener::new(order.clone()).cancelling()),
    );

    let dispatched = bus.dispatch(sample()).await;

    assert!(dispatched.is_cancelled());
    assert_eq!(dispatched.canceller(), Some("first"));
}

/// `ignore_cancelled` 的监听者在事件已取消后被跳过。
#[tokio::test]
async fn ignore_cancelled_listeners_are_skipped() {
    let bus = EventBus::new();
    let order = order_log();

    bus.subscribe(
        Subscribe::typed::<SampleEvent>("canceller").with_priority(ListenerPriority::Lowest),
        Arc::new(ScriptedListener::new(order.clone()).cancelling()),
    );
    bus.subscribe(
        Subscribe::typed::<SampleEvent>("skipped")
            .with_priority(ListenerPriority::Normal)
            .ignore_cancelled(true),
        Arc::new(ScriptedListener::new(order.clone())),
    );
    bus.subscribe(
        Subscribe::typed::<SampleEvent>("observer").with_priority(ListenerPriority::Monitor),
        Arc::new(ScriptedListener::new(order.clone())),
    );

    bus.dispatch(sample()).await;

    assert_eq!(
        order.lock().expect("顺序记录锁不应中毒").as_slice(),
        ["canceller", "observer"]
    );
}

/// Monitor 级监听者的改写和取消都不生效。
#[tokio::test]
async fn monitor_listeners_cannot_mutate_or_cancel() {
    let bus = EventBus::new();
    let order = order_log();

    bus.subscribe(
        Subscribe::typed::<SampleEvent>("monitor").with_priority(ListenerPriority::Monitor),
        Arc::new(
            ScriptedListener::new(order)
                .renaming("被监视者改写")
                .cancelling(),
        ),
    );

    let dispatched = bus.dispatch(sample()).await;

    assert!(!dispatched.is_cancelled());
    assert_eq!(dispatched.event.note, "原始");
}

/// JSON 补丁只能改写事件白名单字段，宿主绑定字段保持不变。
#[tokio::test]
async fn merge_only_applies_whitelisted_fields() {
    let bus = EventBus::new();

    bus.subscribe(
        Subscribe::typed::<SampleEvent>("guest"),
        Arc::new(
            ScriptedListener::new(order_log())
                .patching(json!({ "note": "插件改写", "bound_id": "伪造调用" })),
        ),
    );

    let dispatched = bus.dispatch(sample()).await;

    assert_eq!(dispatched.event.note, "插件改写");
    assert_eq!(dispatched.event.bound_id, "call-1");
}

/// 未声明 merge 的事件对不可信监听者完全只读。
#[tokio::test]
async fn events_without_merge_reject_every_patch() {
    let bus = EventBus::new();

    bus.subscribe(
        Subscribe::new(ReadOnlyEvent::NAME, "guest"),
        Arc::new(ScriptedListener::new(order_log()).patching(json!({ "note": "插件改写" }))),
    );

    let dispatched = bus
        .dispatch(ReadOnlyEvent {
            note: "原始".into(),
        })
        .await;

    assert_eq!(dispatched.event.note, "原始");
}

/// 默认故障模式只记录错误并继续分发，不改变事件语义。
#[tokio::test]
async fn ignored_failures_do_not_stop_dispatch() {
    let bus = EventBus::new();
    let order = order_log();

    bus.subscribe(
        Subscribe::typed::<SampleEvent>("broken").with_priority(ListenerPriority::Lowest),
        Arc::new(ScriptedListener::new(order.clone()).failing()),
    );
    bus.subscribe(
        Subscribe::typed::<SampleEvent>("healthy"),
        Arc::new(ScriptedListener::new(order.clone()).renaming("后续仍执行")),
    );

    let dispatched = bus.dispatch(sample()).await;

    assert!(!dispatched.is_cancelled());
    assert_eq!(dispatched.event.note, "后续仍执行");
    assert_eq!(dispatched.errors.len(), 1);
    assert_eq!(dispatched.errors[0].subscriber, "broken");
}

/// fail-closed 监听者报错等价于取消，插件崩溃不得被当作放行。
#[tokio::test]
async fn fail_closed_listener_errors_cancel_the_event() {
    let bus = EventBus::new();

    bus.subscribe(
        Subscribe::typed::<SampleEvent>("permission").on_failure(ListenerFailure::CancelEvent),
        Arc::new(ScriptedListener::new(order_log()).failing()),
    );

    let dispatched = bus.dispatch(sample()).await;

    assert!(dispatched.is_cancelled());
    assert_eq!(dispatched.canceller(), Some("permission"));
}

/// 注销订阅者后其监听不再参与分发。
#[tokio::test]
async fn unsubscribe_all_removes_every_registration() {
    let bus = EventBus::new();
    let order = order_log();

    bus.subscribe(
        Subscribe::typed::<SampleEvent>("plugin"),
        Arc::new(ScriptedListener::new(order.clone()).cancelling()),
    );
    bus.subscribe(
        Subscribe::new(ReadOnlyEvent::NAME, "plugin"),
        Arc::new(ScriptedListener::new(order.clone())),
    );

    assert_eq!(bus.unsubscribe_all("plugin"), 2);
    assert!(!bus.has_listeners(SampleEvent::NAME));

    let dispatched = bus.dispatch(sample()).await;
    assert!(!dispatched.is_cancelled());
}

/// 单个订阅句柄可精确注销，不影响同事件的其他监听者。
#[tokio::test]
async fn unsubscribe_removes_only_the_target_handle() {
    let bus = EventBus::new();
    let order = order_log();

    let handle = bus.subscribe(
        Subscribe::typed::<SampleEvent>("temporary"),
        Arc::new(ScriptedListener::new(order.clone())),
    );
    bus.subscribe(
        Subscribe::typed::<SampleEvent>("permanent"),
        Arc::new(ScriptedListener::new(order.clone())),
    );

    assert!(bus.unsubscribe(handle));
    assert!(!bus.unsubscribe(handle));
    bus.dispatch(sample()).await;

    assert_eq!(
        order.lock().expect("顺序记录锁不应中毒").as_slice(),
        ["permanent"]
    );
    assert_eq!(bus.listener_count(SampleEvent::NAME), 1);
}

/// 子总线继承父总线监听者，两侧按统一优先级排序参与同一次分发。
#[tokio::test]
async fn child_bus_merges_parent_listeners_by_priority() {
    let parent = EventBus::new();
    let order = order_log();

    parent.subscribe(
        Subscribe::typed::<SampleEvent>("parent-normal"),
        Arc::new(ScriptedListener::new(order.clone())),
    );
    parent.subscribe(
        Subscribe::typed::<SampleEvent>("parent-highest").with_priority(ListenerPriority::Highest),
        Arc::new(ScriptedListener::new(order.clone())),
    );

    let child = parent.child();
    child.subscribe(
        Subscribe::typed::<SampleEvent>("child-lowest").with_priority(ListenerPriority::Lowest),
        Arc::new(ScriptedListener::new(order.clone())),
    );

    child.dispatch(sample()).await;

    assert_eq!(
        order.lock().expect("顺序记录锁不应中毒").as_slice(),
        ["child-lowest", "parent-normal", "parent-highest"]
    );
}

/// 子总线新增的订阅不得回流到父总线，派生 Agent 的门禁不能影响父 Agent。
#[tokio::test]
async fn child_bus_subscriptions_do_not_leak_to_parent() {
    let parent = EventBus::new();
    let order = order_log();
    let child = parent.child();

    child.subscribe(
        Subscribe::typed::<SampleEvent>("child-only"),
        Arc::new(ScriptedListener::new(order.clone()).cancelling()),
    );

    assert!(!parent.has_listeners(SampleEvent::NAME));
    let on_parent = parent.dispatch(sample()).await;
    assert!(!on_parent.is_cancelled());

    let on_child = child.dispatch(sample()).await;
    assert_eq!(on_child.canceller(), Some("child-only"));
}

//! 事件总线：订阅登记、优先级排序与一次分发。

use crate::{
    cancel::Cancellation,
    data::{Event, EventData},
    listener::{EventContext, EventListener, ListenerError, ListenerFailure, ListenerPriority},
};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, RwLock,
    },
};

/// 一次订阅的句柄，用于精确注销。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SubscriptionId(u64);

impl SubscriptionId {
    /// 返回句柄的原始数值，便于跨层记录。
    pub fn get(self) -> u64 {
        self.0
    }
}

/// 一次订阅的声明。
#[derive(Debug, Clone)]
pub struct Subscribe {
    /// 订阅的事件名。
    pub event: String,
    /// 订阅者身份；取消事件时写入 `canceller`，因此必须是可信来源（如宿主注入的插件 ID）。
    pub subscriber: String,
    /// 执行优先级。
    pub priority: ListenerPriority,
    /// 事件已被取消时是否跳过该监听者。
    pub ignore_cancelled: bool,
    /// 监听者报错时的处置方式。
    pub failure: ListenerFailure,
}

impl Subscribe {
    /// 以默认优先级声明一次订阅：`Normal`、不跳过已取消事件、报错仅记录。
    pub fn new(event: impl Into<String>, subscriber: impl Into<String>) -> Self {
        Self {
            event: event.into(),
            subscriber: subscriber.into(),
            priority: ListenerPriority::Normal,
            ignore_cancelled: false,
            failure: ListenerFailure::Ignore,
        }
    }

    /// 以事件类型的静态名称声明一次订阅。
    pub fn typed<E: Event>(subscriber: impl Into<String>) -> Self {
        Self::new(E::NAME, subscriber)
    }

    /// 设置执行优先级。
    pub fn with_priority(mut self, priority: ListenerPriority) -> Self {
        self.priority = priority;
        self
    }

    /// 设置事件已取消时是否跳过。
    pub fn ignore_cancelled(mut self, ignore: bool) -> Self {
        self.ignore_cancelled = ignore;
        self
    }

    /// 设置监听者报错时的处置方式。
    pub fn on_failure(mut self, failure: ListenerFailure) -> Self {
        self.failure = failure;
        self
    }
}

/// 已登记的订阅。
#[derive(Clone)]
struct Registration {
    id: SubscriptionId,
    /// 全局单调登记序号；同优先级内按它排序，使父子总线的监听者也能稳定合并。
    sequence: u64,
    subscriber: String,
    priority: ListenerPriority,
    ignore_cancelled: bool,
    failure: ListenerFailure,
    listener: Arc<dyn EventListener>,
}

/// 一次分发的结果。
#[derive(Debug, Clone)]
pub struct Dispatched<E> {
    /// 可能已被监听者改写的事件。
    pub event: E,
    /// 最终取消状态。
    pub cancellation: Cancellation,
    /// 监听者报告的错误，按发生顺序排列。
    pub errors: Vec<ListenerError>,
}

impl<E> Dispatched<E> {
    /// 事件是否被取消。
    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    /// 返回首个取消者身份。
    pub fn canceller(&self) -> Option<&str> {
        self.cancellation.canceller()
    }

    /// 未被取消时取出事件，被取消时返回 `None`。
    pub fn into_event_unless_cancelled(self) -> Option<E> {
        if self.cancellation.is_cancelled() {
            None
        } else {
            Some(self.event)
        }
    }
}

/// 一次分发中除事件本身以外的结果。
#[derive(Debug, Clone, Default)]
pub struct DispatchOutcome {
    /// 最终取消状态。
    pub cancellation: Cancellation,
    /// 监听者报告的错误，按发生顺序排列。
    pub errors: Vec<ListenerError>,
}

impl DispatchOutcome {
    /// 事件是否被取消。
    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    /// 返回首个取消者身份。
    pub fn canceller(&self) -> Option<&str> {
        self.cancellation.canceller()
    }
}

/// 可取消事件总线。
///
/// 总线本身不含任何业务语义：它只负责按优先级把事件送到监听者、收敛取消状态，
/// 并把不可信监听者的改写限制在事件自身声明的白名单内。各层（core、tui、插件宿主）
/// 定义各自的事件类型并共享同一套分发规则。
///
/// 克隆通过 [`Arc`] 共享同一份订阅表，可直接跨任务持有。注册接口是同步的，分发接口
/// 是异步的：订阅表只在收集监听者时短暂加锁，绝不跨 `await` 持有。
#[derive(Clone, Default)]
pub struct EventBus {
    inner: Arc<BusInner>,
}

#[derive(Default)]
struct BusInner {
    /// 父总线；子总线继承父总线的全部监听者。
    parent: Option<Arc<BusInner>>,
    registrations: RwLock<HashMap<String, Vec<Registration>>>,
    /// 登记序号计数器；子总线共享根总线的计数器以保证全局稳定排序。
    sequence: Arc<AtomicU64>,
}

impl BusInner {
    /// 自底向上收集指定事件的全部监听者。
    fn collect(&self, event: &str, out: &mut Vec<Registration>) {
        if let Some(slot) = self
            .registrations
            .read()
            .expect("事件总线订阅表锁不应中毒")
            .get(event)
        {
            out.extend(slot.iter().cloned());
        }
        if let Some(parent) = &self.parent {
            parent.collect(event, out);
        }
    }

    /// 判断自身或任一祖先是否登记了该事件的监听者。
    fn has_listeners(&self, event: &str) -> bool {
        let own = self
            .registrations
            .read()
            .expect("事件总线订阅表锁不应中毒")
            .get(event)
            .is_some_and(|slot| !slot.is_empty());
        own || self
            .parent
            .as_ref()
            .is_some_and(|parent| parent.has_listeners(event))
    }
}

impl EventBus {
    /// 创建空总线。
    pub fn new() -> Self {
        Self::default()
    }

    /// 创建继承当前总线全部监听者的子总线。
    ///
    /// 子总线上新增或注销的订阅只影响自身，父总线的监听者继续参与子总线的分发，
    /// 且两侧监听者在同一次分发中按统一优先级排序。派生 Agent 用它在共享宿主与插件
    /// 监听的同时叠加自己的权限门禁，而不会把门禁泄漏回父 Agent。
    pub fn child(&self) -> Self {
        Self {
            inner: Arc::new(BusInner {
                parent: Some(Arc::clone(&self.inner)),
                registrations: RwLock::new(HashMap::new()),
                sequence: Arc::clone(&self.inner.sequence),
            }),
        }
    }

    /// 登记一个监听者，返回可用于注销的句柄。
    ///
    /// 同一优先级内按登记顺序执行，因此重复登记会稳定地保持先后关系。
    pub fn subscribe(
        &self,
        subscribe: Subscribe,
        listener: Arc<dyn EventListener>,
    ) -> SubscriptionId {
        let sequence = self.inner.sequence.fetch_add(1, Ordering::Relaxed);
        let id = SubscriptionId(sequence);
        let registration = Registration {
            id,
            sequence,
            subscriber: subscribe.subscriber,
            priority: subscribe.priority,
            ignore_cancelled: subscribe.ignore_cancelled,
            failure: subscribe.failure,
            listener,
        };

        self.inner
            .registrations
            .write()
            .expect("事件总线订阅表锁不应中毒")
            .entry(subscribe.event)
            .or_default()
            .push(registration);
        id
    }

    /// 注销一个订阅，返回是否命中。
    ///
    /// 只在当前总线查找；父总线的订阅必须由其所有者注销。
    pub fn unsubscribe(&self, id: SubscriptionId) -> bool {
        let mut registrations = self
            .inner
            .registrations
            .write()
            .expect("事件总线订阅表锁不应中毒");
        for slot in registrations.values_mut() {
            if let Some(position) = slot.iter().position(|entry| entry.id == id) {
                slot.remove(position);
                return true;
            }
        }
        false
    }

    /// 注销某个订阅者在当前总线所有事件上的全部监听，返回移除数量。
    ///
    /// 插件卸载或重载时必须调用，否则失效的监听者仍会参与取消决策。
    pub fn unsubscribe_all(&self, subscriber: &str) -> usize {
        let mut registrations = self
            .inner
            .registrations
            .write()
            .expect("事件总线订阅表锁不应中毒");
        let mut removed = 0;
        for slot in registrations.values_mut() {
            let before = slot.len();
            slot.retain(|entry| entry.subscriber != subscriber);
            removed += before - slot.len();
        }
        registrations.retain(|_, slot| !slot.is_empty());
        removed
    }

    /// 当前总线或其祖先是否存在该事件的监听者。
    ///
    /// 高频事件（如模型文本增量）应先判断再构造事件，避免无人订阅时白白付出开销。
    pub fn has_listeners(&self, event: &str) -> bool {
        self.inner.has_listeners(event)
    }

    /// 返回指定事件在当前总线及其祖先上的监听者总数。
    pub fn listener_count(&self, event: &str) -> usize {
        let mut listeners = Vec::new();
        self.inner.collect(event, &mut listeners);
        listeners.len()
    }

    /// 分发一个静态类型事件，返回可能被改写的事件与取消状态。
    ///
    /// 监听者报错不会中断分发，也不会返回 `Err`：错误按订阅声明的
    /// [`ListenerFailure`] 处理并收集到 [`Dispatched::errors`]，由调用方决定如何记录。
    /// 这是"单个插件故障不应打断 Agent 主循环"的实现点。
    pub async fn dispatch<E: Event>(&self, mut event: E) -> Dispatched<E> {
        let outcome = self.dispatch_data(&mut event).await;
        Dispatched {
            event,
            cancellation: outcome.cancellation,
            errors: outcome.errors,
        }
    }

    /// 分发一个动态事件负载，就地改写传入的事件。
    ///
    /// 供已经持有 `&mut dyn EventData` 或需要复用事件对象的调用方使用。
    pub async fn dispatch_data(&self, event: &mut dyn EventData) -> DispatchOutcome {
        let name = event.event_name();
        // 先收集再分发：订阅表锁不跨 await 持有，监听者内部可以再次分发或增删订阅。
        let mut listeners = Vec::new();
        self.inner.collect(name, &mut listeners);
        if listeners.is_empty() {
            return DispatchOutcome::default();
        }
        listeners.sort_by_key(|entry| (entry.priority, entry.sequence));

        let mut cancellation = Cancellation::new();
        let mut errors = Vec::new();
        for registration in listeners {
            if registration.ignore_cancelled && cancellation.is_cancelled() {
                continue;
            }
            let read_only = registration.priority == ListenerPriority::Monitor;
            let mut ctx = EventContext::new(
                name,
                &registration.subscriber,
                read_only,
                event,
                &mut cancellation,
            );
            if let Err(error) = registration.listener.handle(&mut ctx).await {
                errors.push(ListenerError {
                    subscriber: registration.subscriber.clone(),
                    error: format!("{error:#}"),
                });
                // fail-closed 监听者报错等同于拒绝：只读的 Monitor 级除外。
                if registration.failure == ListenerFailure::CancelEvent && !read_only {
                    cancellation.cancel(&registration.subscriber);
                }
            }
        }

        DispatchOutcome {
            cancellation,
            errors,
        }
    }
}

#[cfg(test)]
mod tests;

//! 基于 Agent Runtime 控制面的 Teammate 协作插件。
//!
//! 插件拥有成员角色、短期邮箱、确认和消息注入规则；Host 只提供可信 controller
//! 身份、受限 Agent 派生及生命周期操作。

use agent_plugin::{
    export_plugin, ActivationContext, AgentEvent, AgentEventKind, AgentHandle, AgentId,
    AgentOutcome, AgentPlugin, AgentSpawnRequest, AgentStatus, ExtensionEvent, PluginHostApi,
    PromptContribution, Result, ServiceCall, ServiceSpec, ToolCall, ToolResult, ToolSpec, UiColor,
    UiDeclaration, UiFrame, UiInput, UiInputEvent, UiLine, UiNavigationAction, UiNavigationRequest,
    UiPlacement, UiRenderRequest, UiSize, UiSpan, UiStyle, UiViewInstance,
};
use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};

/// Host 注册并由 manifest 授权的唯一派生策略。
const WORKER_PROFILE: &str = "worker";
/// 对其他插件公开的版本化邮箱服务。
const TEAMMATE_SERVICE: &str = "teammate.mailbox";
/// 当前邮箱服务协议版本。
const TEAMMATE_SERVICE_VERSION: &str = "1.0.0";
/// 引导主 Agent 合理拆分协作任务的 developer 提示 ID。
const TEAMMATE_ORCHESTRATION_PROMPT_ID: &str = "teammate-orchestration";
/// 引导主 Agent 选择 teammate 工具的协作规则。
const TEAMMATE_ORCHESTRATION_PROMPT: &str = "当任务可拆分为两个或更多相互独立的子任务、需要并行调研，或需要独立审查时，优先使用 teammate_spawn 创建角色明确的 teammate。向成员提供完整且可执行的任务输入；完成后使用 teammate_result 获取结果并整合。简单、顺序依赖强或拆分成本高的任务不要创建 teammate。";
/// 单个 owner 可创建的最大成员数。
const MAX_MEMBERS_PER_OWNER: usize = 16;
/// 单个成员邮箱可保留的最大未确认消息数。
const MAILBOX_CAPACITY: usize = 64;
/// 单条消息 JSON payload 的最大编码字节数。
const MAX_PAYLOAD_BYTES: usize = 64 * 1024;
/// 单条消息转换为续跑输入的最大尝试次数。
const MAX_DISPATCH_ATTEMPTS: u32 = 5;
/// 主界面右侧的团队摘要入口。
const TEAM_DOCK_VIEW: &str = "teammate-team-dock";
/// 替换主界面的团队工作台子视图。
const TEAM_WORKSPACE_VIEW: &str = "teammate-team-workspace";
/// 展示单个成员实时执行过程并接收交互消息的子视图。
const TEAM_SESSION_VIEW: &str = "teammate-member-session";
/// 单个成员会话视图在 Guest 内保留的最大事件数。
const MEMBER_SESSION_EVENT_LIMIT: usize = 512;

/// Teammate 消息的可信发送方。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum MessageSender {
    /// 当前插件激活实例绑定的 controller Agent。
    Controller { agent_id: AgentId },
    /// Host 注入的版本化服务调用方。
    Plugin { plugin_id: String },
}

impl MessageSender {
    /// 返回适合注入模型上下文的稳定发送方描述。
    fn display(&self) -> String {
        match self {
            Self::Controller { agent_id } => format!("controller:{}", agent_id.as_str()),
            Self::Plugin { plugin_id } => format!("plugin:{plugin_id}"),
        }
    }
}

/// 保存在插件实例内存中的一条未确认消息。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct TeammateMessage {
    /// 插件实例内单调递增的消息 ID。
    id: u64,
    /// Host 确认的发送方，不接受请求覆盖。
    sender: MessageSender,
    /// 首次派生得到的稳定成员地址。
    recipient: AgentId,
    /// 消费方自行解释的消息主题。
    topic: String,
    /// 协议无关 JSON 载荷。
    payload: Value,
    /// 已尝试转换为续跑输入的次数。
    dispatch_attempts: u32,
}

/// 对工具和服务调用方公开的成员快照。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct MemberSnapshot {
    /// 首次派生得到且不会随续跑变化的成员地址。
    id: AgentId,
    /// 当前用于状态、结果、取消和下一次续跑的 Agent ID。
    current_agent_id: AgentId,
    /// owner 定义的成员角色。
    role: String,
    /// 插件最近一次观察到的当前执行状态。
    status: AgentStatus,
    /// 当前成员尚未确认的消息数量。
    unread_messages: usize,
}

/// 插件维护的成员状态。
#[derive(Debug, Clone)]
struct Member {
    owner: String,
    role: String,
    current: AgentHandle,
    status: AgentStatus,
}

impl Member {
    /// 生成不暴露 owner 内部索引的协议快照。
    fn snapshot(&self, address: &AgentId, unread_messages: usize) -> MemberSnapshot {
        MemberSnapshot {
            id: address.clone(),
            current_agent_id: self.current.id.clone(),
            role: self.role.clone(),
            status: self.status,
            unread_messages,
        }
    }
}

/// 单个 WASM 实例内的成员目录和短期邮箱。
#[derive(Default)]
struct TeamState {
    members: BTreeMap<AgentId, Member>,
    mailboxes: BTreeMap<AgentId, VecDeque<TeammateMessage>>,
    next_message_id: u64,
}

impl TeamState {
    /// 校验 owner 配额并登记一个已经成功派生的成员。
    fn add_member(
        &mut self,
        owner: &str,
        role: String,
        handle: AgentHandle,
    ) -> Result<MemberSnapshot> {
        if self
            .members
            .values()
            .filter(|member| member.owner == owner)
            .count()
            >= MAX_MEMBERS_PER_OWNER
        {
            return Err(anyhow!(
                "owner `{owner}` 的成员数已达到上限 {MAX_MEMBERS_PER_OWNER}"
            ));
        }
        let address = handle.id.clone();
        let member = Member {
            owner: owner.to_string(),
            role,
            current: handle,
            status: AgentStatus::Queued,
        };
        let snapshot = member.snapshot(&address, 0);
        self.members.insert(address.clone(), member);
        self.mailboxes.entry(address).or_default();
        Ok(snapshot)
    }

    /// 返回 owner 可见的成员列表，顺序按稳定成员地址排列。
    fn list_members(&self, owner: &str) -> Vec<MemberSnapshot> {
        self.members
            .iter()
            .filter(|(_, member)| member.owner == owner)
            .map(|(address, member)| {
                let unread = self.mailboxes.get(address).map_or(0, VecDeque::len);
                member.snapshot(address, unread)
            })
            .collect()
    }

    /// 删除 owner 名下成员及其全部未确认消息，并返回删除前快照。
    fn remove_member(&mut self, owner: &str, address: &AgentId) -> Result<MemberSnapshot> {
        let unread = self.mailboxes.get(address).map_or(0, VecDeque::len);
        let snapshot = self.member(owner, address)?.snapshot(address, unread);
        self.members.remove(address);
        self.mailboxes.remove(address);
        Ok(snapshot)
    }

    /// 查找并校验 owner 对成员的访问权。
    fn member(&self, owner: &str, address: &AgentId) -> Result<&Member> {
        let member = self
            .members
            .get(address)
            .ok_or_else(|| anyhow!("Teammate 成员不存在：{}", address.as_str()))?;
        if member.owner != owner {
            return Err(anyhow!(
                "owner `{owner}` 无权访问成员 `{}`",
                address.as_str()
            ));
        }
        Ok(member)
    }

    /// 可变查找并校验 owner 对成员的访问权。
    fn member_mut(&mut self, owner: &str, address: &AgentId) -> Result<&mut Member> {
        let member = self
            .members
            .get_mut(address)
            .ok_or_else(|| anyhow!("Teammate 成员不存在：{}", address.as_str()))?;
        if member.owner != owner {
            return Err(anyhow!(
                "owner `{owner}` 无权访问成员 `{}`",
                address.as_str()
            ));
        }
        Ok(member)
    }

    /// 向 owner 名下成员的有界邮箱追加消息。
    fn send(
        &mut self,
        owner: &str,
        sender: MessageSender,
        request: SendRequest,
    ) -> Result<TeammateMessage> {
        self.member(owner, &request.recipient)?;
        validate_topic(&request.topic)?;
        let payload_size = serde_json::to_vec(&request.payload)
            .context("序列化 teammate 消息 payload 失败")?
            .len();
        if payload_size > MAX_PAYLOAD_BYTES {
            return Err(anyhow!(
                "消息 payload 为 {payload_size} 字节，超过上限 {MAX_PAYLOAD_BYTES}"
            ));
        }
        let mailbox = self.mailboxes.entry(request.recipient.clone()).or_default();
        if mailbox.len() >= MAILBOX_CAPACITY {
            return Err(anyhow!(
                "成员 `{}` 的邮箱已满，上限为 {MAILBOX_CAPACITY}",
                request.recipient.as_str()
            ));
        }
        self.next_message_id = self.next_message_id.saturating_add(1);
        let message = TeammateMessage {
            id: self.next_message_id,
            sender,
            recipient: request.recipient,
            topic: request.topic,
            payload: request.payload,
            dispatch_attempts: 0,
        };
        mailbox.push_back(message.clone());
        Ok(message)
    }

    /// 返回指定成员尚未确认的消息，不改变投递次数或队列顺序。
    fn inbox(&self, owner: &str, address: &AgentId, limit: usize) -> Result<Vec<TeammateMessage>> {
        self.member(owner, address)?;
        let limit = limit.clamp(1, MAILBOX_CAPACITY);
        Ok(self
            .mailboxes
            .get(address)
            .into_iter()
            .flat_map(|mailbox| mailbox.iter())
            .take(limit)
            .cloned()
            .collect())
    }

    /// 确认并删除一条属于指定成员的消息。
    fn ack(&mut self, owner: &str, address: &AgentId, message_id: u64) -> Result<TeammateMessage> {
        self.member(owner, address)?;
        let mailbox = self
            .mailboxes
            .get_mut(address)
            .ok_or_else(|| anyhow!("成员邮箱不存在：{}", address.as_str()))?;
        let index = mailbox
            .iter()
            .position(|message| message.id == message_id)
            .ok_or_else(|| anyhow!("消息不存在或已确认：{message_id}"))?;
        mailbox
            .remove(index)
            .ok_or_else(|| anyhow!("删除消息 `{message_id}` 失败"))
    }

    /// 返回 dispatch 所需的消息和当前会话句柄，并校验重试预算。
    fn dispatch_input(
        &self,
        owner: &str,
        address: &AgentId,
        message_id: u64,
    ) -> Result<(TeammateMessage, AgentId)> {
        let member = self.member(owner, address)?;
        let message = self
            .mailboxes
            .get(address)
            .and_then(|mailbox| mailbox.iter().find(|message| message.id == message_id))
            .cloned()
            .ok_or_else(|| anyhow!("消息不存在或已确认：{message_id}"))?;
        if message.dispatch_attempts >= MAX_DISPATCH_ATTEMPTS {
            return Err(anyhow!(
                "消息 `{message_id}` 已达到 dispatch 重试上限 {MAX_DISPATCH_ATTEMPTS}"
            ));
        }
        Ok((message, member.current.id.clone()))
    }

    /// 记录一次失败的 dispatch；消息保持未确认状态。
    fn record_dispatch_failure(&mut self, address: &AgentId, message_id: u64) {
        if let Some(message) = self
            .mailboxes
            .get_mut(address)
            .and_then(|mailbox| mailbox.iter_mut().find(|message| message.id == message_id))
        {
            message.dispatch_attempts = message.dispatch_attempts.saturating_add(1);
        }
    }

    /// 在续跑成功入队后更新当前句柄并自动确认消息。
    fn finish_dispatch(
        &mut self,
        owner: &str,
        address: &AgentId,
        message_id: u64,
        handle: AgentHandle,
    ) -> Result<()> {
        self.ack(owner, address, message_id)?;
        let member = self
            .members
            .get_mut(address)
            .ok_or_else(|| anyhow!("Teammate 成员不存在：{}", address.as_str()))?;
        member.current = handle;
        member.status = AgentStatus::Queued;
        Ok(())
    }
}

/// 创建成员的公共请求。
#[derive(Debug, Deserialize)]
struct SpawnRequest {
    role: String,
    input: String,
}

/// 指向一个稳定成员地址的公共请求。
#[derive(Debug, Deserialize)]
struct MemberRequest {
    member_id: AgentId,
}

/// 向成员邮箱投递消息的公共请求。
#[derive(Debug, Deserialize)]
struct SendRequest {
    recipient: AgentId,
    topic: String,
    #[serde(default)]
    payload: Value,
}

/// 拉取成员邮箱的公共请求。
#[derive(Debug, Deserialize)]
struct InboxRequest {
    member_id: AgentId,
    #[serde(default = "default_inbox_limit")]
    limit: usize,
}

/// 确认或 dispatch 指定消息的公共请求。
#[derive(Debug, Deserialize)]
struct MessageRequest {
    member_id: AgentId,
    message_id: u64,
}

/// 版本化服务支持的操作集合。
#[derive(Debug, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum ServiceRequest {
    /// 创建一个 teammate 成员。
    Spawn { role: String, input: String },
    /// 列出当前 service caller 拥有的成员。
    List,
    /// 查询成员当前执行状态。
    Status { member_id: AgentId },
    /// 查询成员当前执行结果。
    Result { member_id: AgentId },
    /// 取消成员当前执行及其后代。
    Cancel { member_id: AgentId },
    /// 取消并删除成员及其邮箱。
    Remove { member_id: AgentId },
    /// 投递一条可信来源消息。
    Send {
        recipient: AgentId,
        topic: String,
        #[serde(default)]
        payload: Value,
    },
    /// 列出成员未确认消息。
    Inbox {
        member_id: AgentId,
        #[serde(default = "default_inbox_limit")]
        limit: usize,
    },
    /// 确认一条消息。
    Ack { member_id: AgentId, message_id: u64 },
    /// 将消息注入成员的成功会话并启动续跑。
    Dispatch { member_id: AgentId, message_id: u64 },
}

/// 返回默认单次邮箱读取数量。
fn default_inbox_limit() -> usize {
    20
}

/// 提供成员目录、有界邮箱和显式续跑注入的插件。
#[derive(Default)]
struct TeammatePlugin {
    plugin_id: Option<String>,
    state: TeamState,
    selected_member: usize,
    navigation_sequence: u64,
    member_sessions: BTreeMap<AgentId, MemberSessionView>,
    controller_activity: ControllerActivity,
}

/// 主 Agent 在团队栏中展示的紧凑活动状态。
#[derive(Default)]
enum ControllerActivity {
    /// 当前没有运行任务。
    #[default]
    Waiting,
    /// 模型正在分析或生成响应。
    Analyzing,
    /// 当前轮次正在执行工具。
    Working,
}

impl ControllerActivity {
    /// 返回团队栏使用的简体中文活动标签。
    fn label(&self) -> &'static str {
        match self {
            Self::Waiting => "等待",
            Self::Analyzing => "分析中",
            Self::Working => "执行工具",
        }
    }

    /// 返回与活动语义一致的终端颜色。
    fn color(&self) -> UiColor {
        match self {
            Self::Waiting => UiColor::Gray,
            Self::Analyzing => UiColor::Cyan,
            Self::Working => UiColor::Yellow,
        }
    }
}

/// 单个成员会话视图的实时事件、输入和最近交互状态。
struct MemberSessionView {
    target: AgentId,
    timeline: VecDeque<MemberTimelineItem>,
    input: String,
    feedback: Option<std::result::Result<String, String>>,
}

/// 成员会话中按到达顺序保存的 Runtime 事件或本地用户消息。
enum MemberTimelineItem {
    /// Runtime 回放或实时推送的 Agent 事件。
    Event(AgentEvent),
    /// 用户从成员会话视图发送的消息。
    User(String),
}

impl MemberSessionView {
    /// 为成员当前运行句柄创建空的实时视图状态。
    fn new(target: AgentId) -> Self {
        Self {
            target,
            timeline: VecDeque::new(),
            input: String::new(),
            feedback: None,
        }
    }
}

impl TeammatePlugin {
    /// 返回当前插件自身作为工具调用 owner 的可信 ID。
    fn owner(&self) -> Result<&str> {
        self.plugin_id
            .as_deref()
            .ok_or_else(|| anyhow!("Teammate 插件尚未激活"))
    }

    /// 创建成员并登记首次派生句柄。
    fn spawn(
        &mut self,
        host: &dyn PluginHostApi,
        owner: &str,
        request: SpawnRequest,
    ) -> Result<Value> {
        let role = validated_text(request.role, "role", 128)?;
        let input = validated_text(request.input, "input", 32 * 1024)?;
        if self.state.list_members(owner).len() >= MAX_MEMBERS_PER_OWNER {
            return Err(anyhow!(
                "owner `{owner}` 的成员数已达到上限 {MAX_MEMBERS_PER_OWNER}"
            ));
        }
        let agent_input =
            format!("你是 teammate 成员，角色为 `{role}`。请完成以下任务。\n\n{input}");
        let handle = host.spawn_agent(&AgentSpawnRequest::new(WORKER_PROFILE, agent_input))?;
        let member = self.state.add_member(owner, role, handle)?;
        emit(
            host,
            "teammate.member.spawned",
            json!({"owner": owner, "member": member}),
        );
        Ok(json!({"member": member, "completed": false}))
    }

    /// 返回 owner 的成员目录。
    fn list(&self, owner: &str) -> Value {
        json!({"members": self.state.list_members(owner)})
    }

    /// 查询成员当前 Agent 句柄的 Runtime 状态。
    fn status(
        &mut self,
        host: &dyn PluginHostApi,
        owner: &str,
        request: MemberRequest,
    ) -> Result<Value> {
        let target = self
            .state
            .member(owner, &request.member_id)?
            .current
            .id
            .clone();
        let snapshot = host.agent_status(&target)?;
        self.state.member_mut(owner, &request.member_id)?.status = snapshot.status;
        Ok(json!({"snapshot": snapshot}))
    }

    /// 查询成员当前 Agent 句柄的幂等终态结果。
    fn result(
        &mut self,
        host: &dyn PluginHostApi,
        owner: &str,
        request: MemberRequest,
    ) -> Result<Value> {
        let target = self
            .state
            .member(owner, &request.member_id)?
            .current
            .id
            .clone();
        let outcome = host.agent_result(&target)?;
        if let Some(outcome) = &outcome {
            self.state.member_mut(owner, &request.member_id)?.status = match outcome {
                AgentOutcome::Succeeded { .. } => AgentStatus::Succeeded,
                AgentOutcome::Failed { .. } => AgentStatus::Failed,
                AgentOutcome::Cancelled => AgentStatus::Cancelled,
            };
        }
        Ok(json!({"completed": outcome.is_some(), "outcome": outcome}))
    }

    /// 级联取消成员当前 Agent 句柄。
    fn cancel(
        &mut self,
        host: &dyn PluginHostApi,
        owner: &str,
        request: MemberRequest,
    ) -> Result<Value> {
        let target = self
            .state
            .member(owner, &request.member_id)?
            .current
            .id
            .clone();
        let cancelled = host.cancel_agent(&target)?;
        if cancelled {
            self.state.member_mut(owner, &request.member_id)?.status = AgentStatus::Cancelled;
        }
        Ok(json!({"cancelled": cancelled}))
    }

    /// 先取消成员当前执行，再删除成员目录项和全部未确认消息。
    fn remove(
        &mut self,
        host: &dyn PluginHostApi,
        owner: &str,
        request: MemberRequest,
    ) -> Result<Value> {
        let target = self
            .state
            .member(owner, &request.member_id)?
            .current
            .id
            .clone();
        let cancelled = host.cancel_agent(&target)?;
        let member = self.state.remove_member(owner, &request.member_id)?;
        emit(
            host,
            "teammate.member.removed",
            json!({"owner": owner, "member": member, "cancelled": cancelled}),
        );
        Ok(json!({"removed": true, "cancelled": cancelled, "member": member}))
    }

    /// 以 Host 已确认的发送者向 owner 的成员邮箱投递消息。
    fn send(
        &mut self,
        host: &dyn PluginHostApi,
        owner: &str,
        sender: MessageSender,
        request: SendRequest,
    ) -> Result<Value> {
        let message = self.state.send(owner, sender, request)?;
        emit(
            host,
            "teammate.message.sent",
            serde_json::to_value(&message)?,
        );
        Ok(json!({"message": message}))
    }

    /// 列出成员未确认的消息。
    fn inbox(&self, owner: &str, request: InboxRequest) -> Result<Value> {
        let messages = self.state.inbox(owner, &request.member_id, request.limit)?;
        Ok(json!({"messages": messages}))
    }

    /// 显式确认并删除一条消息。
    fn ack(
        &mut self,
        host: &dyn PluginHostApi,
        owner: &str,
        request: MessageRequest,
    ) -> Result<Value> {
        let message = self
            .state
            .ack(owner, &request.member_id, request.message_id)?;
        emit(host, "teammate.message.acked", json!({"message": message}));
        Ok(json!({"acked": true, "message_id": request.message_id}))
    }

    /// 把一条未确认消息转换为成员成功会话的新增输入。
    fn dispatch(
        &mut self,
        host: &dyn PluginHostApi,
        owner: &str,
        request: MessageRequest,
    ) -> Result<Value> {
        let (message, target) =
            self.state
                .dispatch_input(owner, &request.member_id, request.message_id)?;
        let payload =
            serde_json::to_string(&message.payload).context("序列化待注入的 teammate 消息失败")?;
        let input = format!(
            "你收到一条可信 teammate 消息。\n发送者：{}\n主题：{}\nPayload(JSON)：{}",
            message.sender.display(),
            message.topic,
            payload
        );
        let handle =
            match host.continue_agent(&agent_plugin::AgentContinueRequest::new(target, input)) {
                Ok(handle) => handle,
                Err(error) => {
                    self.state
                        .record_dispatch_failure(&request.member_id, request.message_id);
                    return Err(error.context(format!(
                        "dispatch 消息 `{}` 失败，消息仍保留在邮箱中",
                        request.message_id
                    )));
                }
            };
        self.state.finish_dispatch(
            owner,
            &request.member_id,
            request.message_id,
            handle.clone(),
        )?;
        emit(
            host,
            "teammate.message.dispatched",
            json!({"member_id": request.member_id, "message_id": request.message_id, "handle": handle}),
        );
        Ok(json!({"handle": handle, "completed": false, "acked": true}))
    }

    /// 解析并执行一项版本化 service 操作。
    fn handle_request(
        &mut self,
        host: &dyn PluginHostApi,
        owner: &str,
        sender: MessageSender,
        request: ServiceRequest,
    ) -> Result<Value> {
        match request {
            ServiceRequest::Spawn { role, input } => {
                self.spawn(host, owner, SpawnRequest { role, input })
            }
            ServiceRequest::List => Ok(self.list(owner)),
            ServiceRequest::Status { member_id } => {
                self.status(host, owner, MemberRequest { member_id })
            }
            ServiceRequest::Result { member_id } => {
                self.result(host, owner, MemberRequest { member_id })
            }
            ServiceRequest::Cancel { member_id } => {
                self.cancel(host, owner, MemberRequest { member_id })
            }
            ServiceRequest::Remove { member_id } => {
                self.remove(host, owner, MemberRequest { member_id })
            }
            ServiceRequest::Send {
                recipient,
                topic,
                payload,
            } => self.send(
                host,
                owner,
                sender,
                SendRequest {
                    recipient,
                    topic,
                    payload,
                },
            ),
            ServiceRequest::Inbox { member_id, limit } => {
                self.inbox(owner, InboxRequest { member_id, limit })
            }
            ServiceRequest::Ack {
                member_id,
                message_id,
            } => self.ack(
                host,
                owner,
                MessageRequest {
                    member_id,
                    message_id,
                },
            ),
            ServiceRequest::Dispatch {
                member_id,
                message_id,
            } => self.dispatch(
                host,
                owner,
                MessageRequest {
                    member_id,
                    message_id,
                },
            ),
        }
    }

    /// 返回工具界面所属 owner 的成员切片，并收敛越界选择。
    fn ui_members(&mut self) -> Vec<MemberSnapshot> {
        let owner = self.plugin_id.clone().unwrap_or_default();
        let members = self.state.list_members(&owner);
        self.selected_member = self.selected_member.min(members.len().saturating_sub(1));
        members
    }

    /// 渲染主界面右侧的紧凑团队入口。
    fn render_team_dock(&mut self, request: &UiRenderRequest) -> Vec<UiLine> {
        let members = self.ui_members();
        let unread = members
            .iter()
            .map(|member| member.unread_messages)
            .sum::<usize>();
        let active = members
            .iter()
            .filter(|member| matches!(member.status, AgentStatus::Queued | AgentStatus::Running))
            .count();
        let mut lines = vec![
            ui_line(vec![ui_span("团队", Some(UiColor::Cyan), true)]),
            ui_line(vec![ui_span(
                format!("{} 成员  {} 运行  {} 消息", members.len(), active, unread),
                None,
                false,
            )]),
            ui_line(vec![
                ui_span("● 队长  ", Some(self.controller_activity.color()), true),
                ui_span(
                    self.controller_activity.label(),
                    Some(self.controller_activity.color()),
                    false,
                ),
            ]),
            ui_line(Vec::new()),
        ];
        let available = usize::from(request.height).saturating_sub(6);
        for member in members.iter().take(available) {
            lines.push(ui_line(vec![
                ui_span(
                    status_marker(member.status),
                    Some(status_color(member.status)),
                    true,
                ),
                ui_span(format!(" {}", clipped(&member.role, 18)), None, false),
                ui_span(
                    format!("  {}", status_label(member.status)),
                    Some(UiColor::Gray),
                    false,
                ),
                ui_span(
                    if member.unread_messages > 0 {
                        format!("  {}", member.unread_messages)
                    } else {
                        String::new()
                    },
                    Some(UiColor::Yellow),
                    false,
                ),
            ]));
        }
        if members.is_empty() {
            lines.push(ui_line(vec![ui_span(
                "暂无成员",
                Some(UiColor::Gray),
                false,
            )]));
        }
        lines.push(ui_line(Vec::new()));
        lines.push(ui_line(vec![ui_span(
            "打开团队工作台",
            Some(UiColor::Green),
            true,
        )]));
        lines
    }

    /// 渲染全屏团队工作台中的成员目录和当前成员详情。
    fn render_team_workspace(&mut self, request: &UiRenderRequest) -> Vec<UiLine> {
        let members = self.ui_members();
        let mut lines = vec![
            ui_line(vec![ui_span("团队工作台", Some(UiColor::Cyan), true)]),
            ui_line(vec![ui_span(
                format!("成员 {}", members.len()),
                Some(UiColor::Gray),
                false,
            )]),
            ui_line(Vec::new()),
        ];
        let list_limit = usize::from(request.height).saturating_sub(11).max(1);
        for (index, member) in members.iter().enumerate().take(list_limit) {
            let selected = index == self.selected_member;
            lines.push(ui_line(vec![
                ui_span(
                    if selected { ">" } else { " " },
                    Some(UiColor::Cyan),
                    selected,
                ),
                ui_span(
                    format!(" {} ", status_marker(member.status)),
                    Some(status_color(member.status)),
                    true,
                ),
                ui_span(clipped(&member.role, 28), None, selected),
                ui_span(
                    format!("  {}", status_label(member.status)),
                    Some(UiColor::Gray),
                    false,
                ),
                ui_span(
                    if member.unread_messages > 0 {
                        format!("  {} 条消息", member.unread_messages)
                    } else {
                        String::new()
                    },
                    Some(UiColor::Yellow),
                    false,
                ),
            ]));
        }
        if members.is_empty() {
            lines.push(ui_line(vec![ui_span(
                "暂无成员",
                Some(UiColor::Gray),
                false,
            )]));
            return lines;
        }
        if let Some(member) = members.get(self.selected_member) {
            lines.push(ui_line(Vec::new()));
            lines.push(ui_line(vec![ui_span(
                "成员详情",
                Some(UiColor::Blue),
                true,
            )]));
            lines.push(ui_line(vec![ui_span(
                format!("角色  {}", member.role),
                None,
                false,
            )]));
            lines.push(ui_line(vec![ui_span(
                format!("状态  {}", status_label(member.status)),
                Some(status_color(member.status)),
                false,
            )]));
            lines.push(ui_line(vec![ui_span(
                format!("地址  {}", member.id.as_str()),
                Some(UiColor::Gray),
                false,
            )]));
            lines.push(ui_line(vec![ui_span(
                format!("当前  {}", member.current_agent_id.as_str()),
                Some(UiColor::Gray),
                false,
            )]));
            lines.push(ui_line(vec![ui_span(
                "查看成员会话",
                Some(UiColor::Green),
                true,
            )]));
        }
        lines
    }

    /// 渲染成员实时事件和可编辑消息输入行。
    fn render_member_session(&self, request: &UiRenderRequest) -> Vec<UiLine> {
        let Some(instance_id) = request.instance_id.as_deref() else {
            return vec![ui_line(vec![ui_span(
                "成员会话缺少实例 ID",
                Some(UiColor::Red),
                false,
            )])];
        };
        let owner = self.plugin_id.as_deref().unwrap_or_default();
        let Some(member) = self
            .state
            .list_members(owner)
            .into_iter()
            .find(|member| member.id.as_str() == instance_id)
        else {
            return vec![ui_line(vec![ui_span(
                "成员不存在",
                Some(UiColor::Red),
                false,
            )])];
        };
        let Some(session) = self.member_sessions.get(&member.id) else {
            return vec![ui_line(vec![ui_span(
                "正在连接成员会话",
                Some(UiColor::Gray),
                false,
            )])];
        };
        let mut lines = vec![
            ui_line(vec![
                ui_span(member.role, Some(UiColor::Cyan), true),
                ui_span(
                    format!("  {}", status_label(member.status)),
                    Some(status_color(member.status)),
                    false,
                ),
            ]),
            ui_line(vec![ui_span(
                format!("Agent  {}", session.target.as_str()),
                Some(UiColor::Gray),
                false,
            )]),
            ui_line(Vec::new()),
        ];
        let available = usize::from(request.height).saturating_sub(7);
        let event_lines = member_event_lines(&session.timeline, usize::from(request.width));
        lines.extend(event_lines.into_iter().rev().take(available).rev());
        if lines.len() == 3 {
            lines.push(ui_line(vec![ui_span(
                if member.status == AgentStatus::Queued {
                    "等待运行资源"
                } else {
                    "等待成员事件"
                },
                Some(UiColor::Gray),
                false,
            )]));
        }
        lines.push(ui_line(Vec::new()));
        if let Some(feedback) = &session.feedback {
            let (text, color) = match feedback {
                Ok(text) => (text.as_str(), UiColor::Green),
                Err(text) => (text.as_str(), UiColor::Red),
            };
            lines.push(ui_line(vec![ui_span(text, Some(color), false)]));
        }
        lines.push(ui_line(vec![
            ui_span("> ", Some(UiColor::Green), true),
            ui_span(session.input.clone(), None, false),
        ]));
        lines
    }

    /// 刷新当前工具 owner 名下全部成员的 Runtime 状态缓存。
    fn refresh_ui_statuses(&mut self, host: &dyn PluginHostApi) {
        let owner = self.plugin_id.clone().unwrap_or_default();
        let targets = self
            .state
            .list_members(&owner)
            .into_iter()
            .map(|member| (member.id, member.current_agent_id))
            .collect::<Vec<_>>();
        for (address, target) in targets {
            if let Ok(snapshot) = host.agent_status(&target) {
                if let Ok(member) = self.state.member_mut(&owner, &address) {
                    member.status = snapshot.status;
                }
            }
        }
    }

    /// 拉取成员当前句柄的状态与事件，并追加到有界会话视图缓存。
    fn refresh_member_session(&mut self, host: &dyn PluginHostApi, instance_id: &str) {
        let owner = self.plugin_id.clone().unwrap_or_default();
        let Some(member) = self
            .state
            .list_members(&owner)
            .into_iter()
            .find(|member| member.id.as_str() == instance_id)
        else {
            return;
        };
        if let Ok(snapshot) = host.agent_status(&member.current_agent_id) {
            if let Ok(stored) = self.state.member_mut(&owner, &member.id) {
                stored.status = snapshot.status;
            }
        }
        let session = self
            .member_sessions
            .entry(member.id.clone())
            .or_insert_with(|| MemberSessionView::new(member.current_agent_id.clone()));
        if session.target != member.current_agent_id {
            session.target = member.current_agent_id.clone();
        }
        match host.agent_events(&session.target, 256) {
            Ok(events) => {
                session.feedback = session.feedback.take().filter(|result| result.is_err());
                for event in events {
                    if session.timeline.len() >= MEMBER_SESSION_EVENT_LIMIT {
                        session.timeline.pop_front();
                    }
                    session.timeline.push_back(MemberTimelineItem::Event(event));
                }
            }
            Err(error) => session.feedback = Some(Err(format!("读取实时事件失败：{error}"))),
        }
    }

    /// 请求宿主把团队工作台压入通用子视图导航栈。
    fn open_team_workspace(&mut self, host: &dyn PluginHostApi) {
        self.navigation_sequence = self.navigation_sequence.saturating_add(1);
        let _ = host.navigate_view(UiNavigationRequest {
            request_id: format!("teammate-open-{}", self.navigation_sequence),
            action: UiNavigationAction::Push {
                view: UiViewInstance {
                    view_id: TEAM_WORKSPACE_VIEW.into(),
                    instance_id: "team".into(),
                    title: Some("团队".into()),
                },
            },
        });
    }

    /// 打开当前选中成员的实时会话子视图。
    fn open_selected_member_session(&mut self, host: &dyn PluginHostApi) {
        let owner = self.plugin_id.clone().unwrap_or_default();
        let members = self.state.list_members(&owner);
        let Some(member) = members.get(self.selected_member) else {
            return;
        };
        self.member_sessions
            .entry(member.id.clone())
            .or_insert_with(|| MemberSessionView::new(member.current_agent_id.clone()));
        self.navigation_sequence = self.navigation_sequence.saturating_add(1);
        let _ = host.navigate_view(UiNavigationRequest {
            request_id: format!("teammate-member-{}", self.navigation_sequence),
            action: UiNavigationAction::Push {
                view: UiViewInstance {
                    view_id: TEAM_SESSION_VIEW.into(),
                    instance_id: member.id.as_str().into(),
                    title: Some(member.role.clone()),
                },
            },
        });
    }

    /// 将会话输入发送给运行中成员，或基于成功终态会话启动后续运行。
    fn send_member_input(&mut self, host: &dyn PluginHostApi, instance_id: &str) {
        let owner = self.plugin_id.clone().unwrap_or_default();
        let Some(snapshot) = self
            .state
            .list_members(&owner)
            .into_iter()
            .find(|member| member.id.as_str() == instance_id)
        else {
            return;
        };
        let Some(session) = self.member_sessions.get_mut(&snapshot.id) else {
            return;
        };
        let input = session.input.trim().to_string();
        if input.is_empty() {
            return;
        }
        let result = match snapshot.status {
            AgentStatus::Queued | AgentStatus::Running => host
                .steer_agent(&snapshot.current_agent_id, &input)
                .map(|_| "消息已发送给成员".to_string()),
            AgentStatus::Succeeded => host
                .continue_agent(&agent_plugin::AgentContinueRequest::new(
                    snapshot.current_agent_id.clone(),
                    input.clone(),
                ))
                .and_then(|handle| {
                    let member = self.state.member_mut(&owner, &snapshot.id)?;
                    member.current = handle.clone();
                    member.status = AgentStatus::Queued;
                    session.target = handle.id;
                    Ok("已开始成员后续会话".to_string())
                }),
            AgentStatus::Ready | AgentStatus::Failed | AgentStatus::Cancelled => {
                Err(anyhow!("成员当前状态不能接收消息"))
            }
        };
        match result {
            Ok(message) => {
                if session.timeline.len() >= MEMBER_SESSION_EVENT_LIMIT {
                    session.timeline.pop_front();
                }
                session
                    .timeline
                    .push_back(MemberTimelineItem::User(input.clone()));
                session.input.clear();
                session.feedback = Some(Ok(message));
            }
            Err(error) => session.feedback = Some(Err(error.to_string())),
        }
    }
}

impl AgentPlugin for TeammatePlugin {
    /// 保存可信插件 ID、注册协作提示并提供版本化 mailbox service。
    fn activate(&mut self, host: &dyn PluginHostApi, context: ActivationContext) -> Result<()> {
        self.plugin_id = Some(context.plugin_id);
        host.upsert_service(&ServiceSpec {
            name: TEAMMATE_SERVICE.into(),
            version: TEAMMATE_SERVICE_VERSION.into(),
            description: Some("管理隔离的 teammate 成员、短期邮箱、确认和续跑注入".into()),
        })?;
        host.upsert_prompt(&PromptContribution {
            id: TEAMMATE_ORCHESTRATION_PROMPT_ID.into(),
            content: TEAMMATE_ORCHESTRATION_PROMPT.into(),
            priority: 110,
        })?;
        Ok(())
    }

    /// 注销协作提示和 service，并清空仅对当前激活实例有效的短期状态。
    fn deactivate(&mut self, host: &dyn PluginHostApi) -> Result<()> {
        host.remove_prompt(TEAMMATE_ORCHESTRATION_PROMPT_ID)?;
        host.remove_service(TEAMMATE_SERVICE)?;
        self.plugin_id = None;
        self.state = TeamState::default();
        self.member_sessions.clear();
        self.controller_activity = ControllerActivity::default();
        Ok(())
    }

    /// 返回模型可调用的 teammate 控制面工具。
    fn list_tools(&self) -> Vec<ToolSpec> {
        teammate_tools()
    }

    /// 根据主 Agent 生命周期事件更新团队栏中的队长活动状态。
    fn on_event(&mut self, event: AgentEvent) {
        self.controller_activity = match event.kind {
            AgentEventKind::RunFinished | AgentEventKind::StepLimitReached => {
                ControllerActivity::Waiting
            }
            AgentEventKind::ToolStarted => ControllerActivity::Working,
            AgentEventKind::RunStarted
            | AgentEventKind::TurnStarted
            | AgentEventKind::ModelRequest
            | AgentEventKind::ModelTextDelta
            | AgentEventKind::ToolFinished
            | AgentEventKind::ToolSkipped
            | AgentEventKind::SteeringInjected
            | AgentEventKind::FollowUpInjected => ControllerActivity::Analyzing,
            AgentEventKind::Extension
            | AgentEventKind::ModelThinkingDelta
            | AgentEventKind::ModelResponse
            | AgentEventKind::BillingUsage
            | AgentEventKind::TurnFinished => return,
        };
    }

    /// 执行 teammate 工具，并使用 Host 注入的 controller 身份作为发送者。
    fn call_tool_with_host(
        &mut self,
        host: &dyn PluginHostApi,
        call: ToolCall,
    ) -> Result<ToolResult> {
        let owner = self.owner()?.to_string();
        let operation = call.name.clone();
        let content = match operation.as_str() {
            "teammate_spawn" => self.spawn(host, &owner, decode(call.args.clone())?)?,
            "teammate_list" => self.list(&owner),
            "teammate_status" => self.status(host, &owner, decode(call.args.clone())?)?,
            "teammate_result" => self.result(host, &owner, decode(call.args.clone())?)?,
            "teammate_cancel" => self.cancel(host, &owner, decode(call.args.clone())?)?,
            "teammate_remove" => self.remove(host, &owner, decode(call.args.clone())?)?,
            "teammate_send" => {
                let sender = MessageSender::Controller {
                    agent_id: host.agent_identity()?,
                };
                self.send(host, &owner, sender, decode(call.args.clone())?)?
            }
            "teammate_inbox" => self.inbox(&owner, decode(call.args.clone())?)?,
            "teammate_ack" => self.ack(host, &owner, decode(call.args.clone())?)?,
            "teammate_dispatch" => self.dispatch(host, &owner, decode(call.args.clone())?)?,
            _ => {
                return Ok(ToolResult::error(
                    call.id,
                    call.name,
                    format!("未知 Teammate 工具：{operation}"),
                ))
            }
        };
        Ok(ToolResult::success(call.id, call.name, content))
    }

    /// 按 Host 注入的 caller ID 隔离并执行版本化 service 请求。
    fn handle_service(&mut self, host: &dyn PluginHostApi, call: ServiceCall) -> Result<Value> {
        if call.name != TEAMMATE_SERVICE {
            return Err(anyhow!("Teammate 插件未实现服务 `{}`", call.name));
        }
        let owner = call.caller_id;
        let request = decode(call.payload)?;
        self.handle_request(
            host,
            &owner,
            MessageSender::Plugin {
                plugin_id: owner.clone(),
            },
            request,
        )
    }

    /// 声明右侧团队入口与可动态打开的全屏工作台。
    fn describe_ui(&self) -> Vec<UiDeclaration> {
        vec![
            UiDeclaration {
                plugin_id: String::new(),
                view_id: TEAM_DOCK_VIEW.into(),
                title: "团队".into(),
                placement: UiPlacement::Right,
                size: UiSize {
                    width: Some(30),
                    height: None,
                },
                focusable: true,
            },
            UiDeclaration {
                plugin_id: String::new(),
                view_id: TEAM_WORKSPACE_VIEW.into(),
                title: "团队工作台".into(),
                placement: UiPlacement::Subview,
                size: UiSize::default(),
                focusable: true,
            },
            UiDeclaration {
                plugin_id: String::new(),
                view_id: TEAM_SESSION_VIEW.into(),
                title: "成员会话".into(),
                placement: UiPlacement::Subview,
                size: UiSize::default(),
                focusable: true,
            },
        ]
    }

    /// 根据宿主分配尺寸渲染团队摘要或全屏工作台。
    ///
    /// 团队摘要在当前 owner 没有成员时返回隐藏帧，子视图不受该条件影响。
    fn render_ui(&mut self, request: UiRenderRequest) -> Option<UiFrame> {
        let visible = request.view_id != TEAM_DOCK_VIEW || !self.ui_members().is_empty();
        let lines = match request.view_id.as_str() {
            TEAM_DOCK_VIEW => self.render_team_dock(&request),
            TEAM_WORKSPACE_VIEW => self.render_team_workspace(&request),
            TEAM_SESSION_VIEW => self.render_member_session(&request),
            _ => return None,
        };
        Some(UiFrame {
            view_id: request.view_id,
            visible,
            lines,
        })
    }

    /// 周期渲染前刷新成员状态和会话事件，再复用纯渲染逻辑生成帧。
    fn render_ui_with_host(
        &mut self,
        host: &dyn PluginHostApi,
        request: UiRenderRequest,
    ) -> Option<UiFrame> {
        match request.view_id.as_str() {
            TEAM_DOCK_VIEW | TEAM_WORKSPACE_VIEW => self.refresh_ui_statuses(host),
            TEAM_SESSION_VIEW => {
                if let Some(instance_id) = request.instance_id.as_deref() {
                    self.refresh_member_session(host, instance_id);
                }
            }
            _ => {}
        }
        self.render_ui(request)
    }

    /// 处理团队入口、成员选择和状态刷新输入，并通过通用导航 API 打开工作台。
    fn on_ui_input_with_host(&mut self, host: &dyn PluginHostApi, input: UiInput) {
        match input.event {
            UiInputEvent::Key { code, .. }
                if input.view_id == TEAM_DOCK_VIEW && code == "enter" =>
            {
                self.refresh_ui_statuses(host);
                self.open_team_workspace(host);
            }
            UiInputEvent::Mouse { kind, .. }
                if input.view_id == TEAM_DOCK_VIEW && kind.starts_with("down_") =>
            {
                self.refresh_ui_statuses(host);
                self.open_team_workspace(host);
            }
            UiInputEvent::Key { code, .. } if input.view_id == TEAM_WORKSPACE_VIEW => {
                let member_count = self.ui_members().len();
                match code.as_str() {
                    "up" => self.selected_member = self.selected_member.saturating_sub(1),
                    "down" => {
                        self.selected_member =
                            (self.selected_member + 1).min(member_count.saturating_sub(1));
                    }
                    "r" => self.refresh_ui_statuses(host),
                    "enter" => self.open_selected_member_session(host),
                    _ => {}
                }
            }
            UiInputEvent::Key { code, modifiers } if input.view_id == TEAM_SESSION_VIEW => {
                let Some(instance_id) = input.instance_id.as_deref() else {
                    return;
                };
                if code == "enter" {
                    self.send_member_input(host, instance_id);
                } else if code == "backspace" {
                    if let Some(session) = self
                        .member_sessions
                        .iter_mut()
                        .find(|(member_id, _)| member_id.as_str() == instance_id)
                        .map(|(_, session)| session)
                    {
                        session.input.pop();
                    }
                } else if code.chars().count() == 1
                    && !modifiers.iter().any(|modifier| {
                        matches!(modifier.as_str(), "control" | "alt" | "super" | "meta")
                    })
                {
                    if let Some(session) = self
                        .member_sessions
                        .iter_mut()
                        .find(|(member_id, _)| member_id.as_str() == instance_id)
                        .map(|(_, session)| session)
                    {
                        session.input.push_str(&code);
                        session.feedback = None;
                    }
                }
            }
            UiInputEvent::Mouse { kind, y, .. }
                if input.view_id == TEAM_WORKSPACE_VIEW && kind.starts_with("down_") =>
            {
                let member_count = self.ui_members().len();
                self.selected_member =
                    usize::from(y.saturating_sub(3)).min(member_count.saturating_sub(1));
            }
            _ => {}
        }
    }
}

/// 将 Agent 事件转换为适合会话视图的可见过程行，并聚合连续文本增量。
fn member_event_lines(timeline: &VecDeque<MemberTimelineItem>, width: usize) -> Vec<UiLine> {
    let text_width = width.saturating_sub(4).max(12);
    let mut rows: Vec<(String, UiColor)> = Vec::new();
    for item in timeline {
        let event = match item {
            MemberTimelineItem::Event(event) => event,
            MemberTimelineItem::User(message) => {
                rows.push((format!("你  {message}"), UiColor::Green));
                continue;
            }
        };
        match event.kind {
            AgentEventKind::RunStarted => rows.push(("开始运行".into(), UiColor::Cyan)),
            AgentEventKind::TurnStarted => {
                rows.push((format!("分析中 · 第 {} 步", event.step + 1), UiColor::Blue))
            }
            AgentEventKind::ModelTextDelta => {
                let delta = event.payload["delta"].as_str().unwrap_or_default();
                if let Some((text, _)) = rows
                    .last_mut()
                    .filter(|(text, _)| text.starts_with("回复  "))
                {
                    text.push_str(delta);
                } else if !delta.is_empty() {
                    rows.push((format!("回复  {delta}"), UiColor::White));
                }
            }
            AgentEventKind::ToolStarted => {
                let name = event.payload["name"].as_str().unwrap_or("tool");
                let args = compact_json(&event.payload["args"], 120);
                rows.push((
                    format!(
                        "调用  {name}{}",
                        if args.is_empty() {
                            args
                        } else {
                            format!("  {args}")
                        }
                    ),
                    UiColor::Yellow,
                ));
            }
            AgentEventKind::ToolFinished => {
                let name = event.payload["name"].as_str().unwrap_or("tool");
                let failed = event.payload["is_error"].as_bool().unwrap_or(false);
                let result = compact_json(&event.payload["result"], 120);
                rows.push((
                    format!(
                        "{}  {name}{}",
                        if failed { "失败" } else { "完成" },
                        if result.is_empty() {
                            result
                        } else {
                            format!("  {result}")
                        }
                    ),
                    if failed { UiColor::Red } else { UiColor::Green },
                ));
            }
            AgentEventKind::ToolSkipped => {
                let name = event.payload["name"].as_str().unwrap_or("tool");
                rows.push((format!("跳过  {name}"), UiColor::Gray));
            }
            AgentEventKind::SteeringInjected => {
                rows.push(("已接收新的互动消息".into(), UiColor::Cyan));
            }
            AgentEventKind::RunFinished => rows.push(("运行完成".into(), UiColor::Green)),
            AgentEventKind::StepLimitReached => {
                rows.push(("达到运行步数上限".into(), UiColor::Red));
            }
            AgentEventKind::Extension
            | AgentEventKind::ModelRequest
            | AgentEventKind::ModelThinkingDelta
            | AgentEventKind::ModelResponse
            | AgentEventKind::BillingUsage
            | AgentEventKind::TurnFinished
            | AgentEventKind::FollowUpInjected => {}
        }
    }
    rows.into_iter()
        .map(|(text, color)| {
            ui_line(vec![ui_span(
                clipped(&text, text_width),
                Some(color),
                false,
            )])
        })
        .collect()
}

/// 将事件中的 JSON 参数或结果压成单行摘要，避免成员会话被大块输出淹没。
fn compact_json(value: &Value, max_chars: usize) -> String {
    if value.is_null() {
        return String::new();
    }
    let text = match value {
        Value::String(text) => text.clone(),
        value => value.to_string(),
    };
    clipped(&text.replace(['\n', '\r'], " "), max_chars)
}

/// 构造一行协议无关终端内容。
fn ui_line(spans: Vec<UiSpan>) -> UiLine {
    UiLine { spans }
}

/// 构造带可选前景色和粗体的终端文本片段。
fn ui_span(text: impl Into<String>, foreground: Option<UiColor>, bold: bool) -> UiSpan {
    UiSpan {
        text: text.into(),
        style: UiStyle {
            foreground,
            bold,
            ..UiStyle::default()
        },
    }
}

/// 返回成员状态的单字符视觉标记。
fn status_marker(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Ready => "○",
        AgentStatus::Queued => "◌",
        AgentStatus::Running => "●",
        AgentStatus::Succeeded => "✓",
        AgentStatus::Failed => "×",
        AgentStatus::Cancelled => "-",
    }
}

/// 返回成员状态的简体中文标签。
fn status_label(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Ready => "就绪",
        AgentStatus::Queued => "排队",
        AgentStatus::Running => "运行中",
        AgentStatus::Succeeded => "已完成",
        AgentStatus::Failed => "失败",
        AgentStatus::Cancelled => "已取消",
    }
}

/// 返回与成员状态语义一致的终端颜色。
fn status_color(status: AgentStatus) -> UiColor {
    match status {
        AgentStatus::Ready => UiColor::Blue,
        AgentStatus::Queued => UiColor::Yellow,
        AgentStatus::Running => UiColor::Cyan,
        AgentStatus::Succeeded => UiColor::Green,
        AgentStatus::Failed => UiColor::Red,
        AgentStatus::Cancelled => UiColor::Gray,
    }
}

/// 按字符数量截断紧凑列表文本，避免窄终端换行破坏对齐。
fn clipped(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut output = text
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    output.push('…');
    output
}

/// 反序列化工具或 service 的公共 JSON 请求。
fn decode<T: for<'de> Deserialize<'de>>(value: Value) -> Result<T> {
    serde_json::from_value(value).context("Teammate 请求格式无效")
}

/// 校验角色和任务等非空、有界文本字段。
fn validated_text(value: String, field: &str, max_bytes: usize) -> Result<String> {
    if value.trim().is_empty() {
        return Err(anyhow!("字段 `{field}` 不能为空"));
    }
    if value.len() > max_bytes {
        return Err(anyhow!("字段 `{field}` 超过 {max_bytes} 字节上限"));
    }
    Ok(value)
}

/// 校验可路由且适合日志展示的消息主题。
fn validate_topic(topic: &str) -> Result<()> {
    let valid = !topic.is_empty()
        && topic.len() <= 128
        && topic
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if !valid {
        return Err(anyhow!(
            "消息 topic 只能包含 ASCII 字母、数字、点、下划线和连字符，长度为 1 到 128 字节"
        ));
    }
    Ok(())
}

/// 尽力发布结构化 teammate 事件；观察性事件失败不回滚已完成的状态变更。
fn emit(host: &dyn PluginHostApi, name: &str, data: Value) {
    let _ = host.emit_event(&ExtensionEvent {
        name: name.to_string(),
        data,
        presentation: None,
    });
}

/// 返回所有 teammate 工具的 JSON Schema。
fn teammate_tools() -> Vec<ToolSpec> {
    let member_schema = || {
        json!({
            "type": "object",
            "properties": {"member_id": {"type": "string", "minLength": 1}},
            "required": ["member_id"],
            "additionalProperties": false
        })
    };
    let message_schema = || {
        json!({
            "type": "object",
            "properties": {
                "member_id": {"type": "string", "minLength": 1},
                "message_id": {"type": "integer", "minimum": 1}
            },
            "required": ["member_id", "message_id"],
            "additionalProperties": false
        })
    };
    vec![
        ToolSpec::new(
            "teammate_spawn",
            "使用受限 worker profile 创建带角色的 teammate；立即返回成员地址，不等待任务完成。",
            json!({
                "type": "object",
                "properties": {
                    "role": {"type": "string", "minLength": 1, "maxLength": 128},
                    "input": {"type": "string", "minLength": 1}
                },
                "required": ["role", "input"],
                "additionalProperties": false
            }),
        ),
        ToolSpec::new(
            "teammate_list",
            "列出当前 controller 拥有的 teammate 成员及当前执行句柄。",
            json!({"type": "object", "properties": {}, "additionalProperties": false}),
        ),
        ToolSpec::new(
            "teammate_status",
            "查询成员当前执行句柄的状态。",
            member_schema(),
        ),
        ToolSpec::new(
            "teammate_result",
            "读取成员当前执行句柄的幂等终态结果。",
            member_schema(),
        ),
        ToolSpec::new(
            "teammate_cancel",
            "级联取消成员当前执行句柄及其后代。",
            member_schema(),
        ),
        ToolSpec::new(
            "teammate_remove",
            "取消并删除成员及其全部未确认消息，释放成员配额。",
            member_schema(),
        ),
        ToolSpec::new(
            "teammate_send",
            "以 Host 注入的 controller 身份向成员有界邮箱投递消息。",
            json!({
                "type": "object",
                "properties": {
                    "recipient": {"type": "string", "minLength": 1},
                    "topic": {"type": "string", "minLength": 1, "maxLength": 128},
                    "payload": {}
                },
                "required": ["recipient", "topic"],
                "additionalProperties": false
            }),
        ),
        ToolSpec::new(
            "teammate_inbox",
            "列出成员邮箱中尚未确认的消息，不改变队列状态。",
            json!({
                "type": "object",
                "properties": {
                    "member_id": {"type": "string", "minLength": 1},
                    "limit": {"type": "integer", "minimum": 1, "maximum": MAILBOX_CAPACITY}
                },
                "required": ["member_id"],
                "additionalProperties": false
            }),
        ),
        ToolSpec::new("teammate_ack", "确认并删除一条成员消息。", message_schema()),
        ToolSpec::new(
            "teammate_dispatch",
            "把指定消息注入成员的成功会话并异步启动续跑；成功后自动确认消息。",
            message_schema(),
        ),
    ]
}

export_plugin!(TeammatePlugin);

#[cfg(test)]
mod tests {
    use super::*;
    use agent_plugin::AgentLineage;

    /// 构造纯状态测试使用的 Agent 句柄。
    fn handle(id: &str) -> AgentHandle {
        let id = AgentId::parse(id).expect("测试 Agent ID 应合法");
        AgentHandle {
            id: id.clone(),
            lineage: AgentLineage {
                parent: None,
                root: id,
                depth: 1,
            },
        }
    }

    /// 邮箱必须保留可信发送者，并在确认后删除消息。
    #[test]
    fn mailbox_preserves_sender_and_requires_ack() {
        let mut state = TeamState::default();
        let member = state
            .add_member("owner-a", "reviewer".into(), handle("member-a"))
            .expect("成员应登记成功");
        let message = state
            .send(
                "owner-a",
                MessageSender::Plugin {
                    plugin_id: "caller-a".into(),
                },
                SendRequest {
                    recipient: member.id.clone(),
                    topic: "review.requested".into(),
                    payload: json!({"path": "src/lib.rs"}),
                },
            )
            .expect("消息应发送成功");

        let inbox = state.inbox("owner-a", &member.id, 20).expect("邮箱应可读");
        assert_eq!(inbox, vec![message.clone()]);
        assert!(
            matches!(message.sender, MessageSender::Plugin { plugin_id } if plugin_id == "caller-a")
        );

        state
            .ack("owner-a", &member.id, message.id)
            .expect("消息应确认成功");
        assert!(state
            .inbox("owner-a", &member.id, 20)
            .expect("邮箱应可读")
            .is_empty());
    }

    /// service caller 的 owner 边界必须阻止跨命名空间读取成员和邮箱。
    #[test]
    fn owner_isolation_rejects_cross_namespace_access() {
        let mut state = TeamState::default();
        let member = state
            .add_member("owner-a", "worker".into(), handle("member-a"))
            .expect("成员应登记成功");

        let error = state
            .inbox("owner-b", &member.id, 20)
            .expect_err("其他 owner 不应读取邮箱");
        assert!(error.to_string().contains("无权访问"));
        assert!(state.list_members("owner-b").is_empty());
    }

    /// dispatch 失败次数达到预算后必须停止生成新的续跑请求。
    #[test]
    fn dispatch_attempts_are_bounded() {
        let mut state = TeamState::default();
        let member = state
            .add_member("owner", "worker".into(), handle("member"))
            .expect("成员应登记成功");
        let message = state
            .send(
                "owner",
                MessageSender::Controller {
                    agent_id: AgentId::parse("controller").expect("测试 ID 应合法"),
                },
                SendRequest {
                    recipient: member.id.clone(),
                    topic: "task.retry".into(),
                    payload: Value::Null,
                },
            )
            .expect("消息应发送成功");

        for _ in 0..MAX_DISPATCH_ATTEMPTS {
            state.record_dispatch_failure(&member.id, message.id);
        }
        let error = state
            .dispatch_input("owner", &member.id, message.id)
            .expect_err("达到重试预算后应拒绝 dispatch");
        assert!(error.to_string().contains("重试上限"));
    }

    /// 插件必须声明可聚焦团队入口和全屏工作台，并在摘要中渲染成员状态。
    #[test]
    fn team_ui_declares_entry_and_renders_members() {
        let mut plugin = TeammatePlugin {
            plugin_id: Some("teammate".into()),
            ..TeammatePlugin::default()
        };
        plugin
            .state
            .add_member("teammate", "reviewer".into(), handle("member-ui"))
            .expect("UI 测试成员应登记成功");

        let declarations = plugin.describe_ui();
        assert_eq!(declarations.len(), 3);
        assert_eq!(declarations[0].placement, UiPlacement::Right);
        assert!(declarations[0].focusable);
        assert_eq!(declarations[1].placement, UiPlacement::Subview);

        let frame = plugin
            .render_ui(UiRenderRequest {
                plugin_id: "teammate".into(),
                view_id: TEAM_DOCK_VIEW.into(),
                instance_id: None,
                width: 30,
                height: 16,
                focused: false,
                frame: 1,
            })
            .expect("团队摘要应返回可见帧");
        let text = frame
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.text.as_str())
            .collect::<String>();
        assert!(text.contains("团队"), "{text}");
        assert!(text.contains("reviewer"), "{text}");
        assert!(text.contains('◌'), "{text}");
        assert!(text.contains("队长  等待"), "{text}");
        assert!(text.contains("排队"), "{text}");

        let workspace = plugin
            .render_ui(UiRenderRequest {
                plugin_id: "teammate".into(),
                view_id: TEAM_WORKSPACE_VIEW.into(),
                instance_id: Some("team".into()),
                width: 80,
                height: 24,
                focused: true,
                frame: 2,
            })
            .expect("团队工作台应返回可见帧");
        let workspace_text = workspace
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.text.as_str())
            .collect::<String>();
        assert!(workspace_text.contains("排队"), "{workspace_text}");
        assert!(workspace_text.contains("查看成员会话"), "{workspace_text}");

        let member = plugin.state.list_members("teammate").remove(0);
        plugin.member_sessions.insert(
            member.id.clone(),
            MemberSessionView {
                target: member.current_agent_id.clone(),
                timeline: VecDeque::from([MemberTimelineItem::Event(AgentEvent {
                    id: "event-1".into(),
                    run_id: "run-1".into(),
                    timestamp_ms: 1,
                    kind: AgentEventKind::ToolStarted,
                    step: 0,
                    payload: json!({"name": "read_file"}),
                })]),
                input: "请继续检查".into(),
                feedback: None,
            },
        );
        let session = plugin
            .render_ui(UiRenderRequest {
                plugin_id: "teammate".into(),
                view_id: TEAM_SESSION_VIEW.into(),
                instance_id: Some(member.id.as_str().into()),
                width: 80,
                height: 24,
                focused: true,
                frame: 3,
            })
            .expect("成员会话应返回可见帧");
        let session_text = session
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.text.as_str())
            .collect::<String>();
        assert!(session_text.contains("调用  read_file"), "{session_text}");
        assert!(session_text.contains("请继续检查"), "{session_text}");
    }

    /// 当前 owner 没有成员时，团队摘要不应占用右侧面板。
    #[test]
    fn team_dock_is_hidden_without_members() {
        let mut plugin = TeammatePlugin {
            plugin_id: Some("teammate".into()),
            ..TeammatePlugin::default()
        };
        let frame = plugin
            .render_ui(UiRenderRequest {
                plugin_id: "teammate".into(),
                view_id: TEAM_DOCK_VIEW.into(),
                instance_id: None,
                width: 30,
                height: 16,
                focused: false,
                frame: 1,
            })
            .expect("团队摘要应返回帧");

        assert!(!frame.visible);
    }
}

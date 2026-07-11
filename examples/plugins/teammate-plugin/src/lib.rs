//! 基于 Agent Runtime 控制面的 Teammate 协作插件。
//!
//! 插件拥有成员角色、短期邮箱、确认和消息注入规则；Host 只提供可信 controller
//! 身份、受限 Agent 派生及生命周期操作。

use agent_plugin::{
    export_plugin, ActivationContext, AgentHandle, AgentId, AgentPlugin, AgentSpawnRequest,
    ExtensionEvent, PluginHostApi, Result, ServiceCall, ServiceSpec, ToolCall, ToolResult,
    ToolSpec,
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
/// 单个 owner 可创建的最大成员数。
const MAX_MEMBERS_PER_OWNER: usize = 16;
/// 单个成员邮箱可保留的最大未确认消息数。
const MAILBOX_CAPACITY: usize = 64;
/// 单条消息 JSON payload 的最大编码字节数。
const MAX_PAYLOAD_BYTES: usize = 64 * 1024;
/// 单条消息转换为续跑输入的最大尝试次数。
const MAX_DISPATCH_ATTEMPTS: u32 = 5;

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
}

/// 插件维护的成员状态。
#[derive(Debug, Clone)]
struct Member {
    owner: String,
    role: String,
    current: AgentHandle,
}

impl Member {
    /// 生成不暴露 owner 内部索引的协议快照。
    fn snapshot(&self, address: &AgentId) -> MemberSnapshot {
        MemberSnapshot {
            id: address.clone(),
            current_agent_id: self.current.id.clone(),
            role: self.role.clone(),
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
        };
        let snapshot = member.snapshot(&address);
        self.members.insert(address.clone(), member);
        self.mailboxes.entry(address).or_default();
        Ok(snapshot)
    }

    /// 返回 owner 可见的成员列表，顺序按稳定成员地址排列。
    fn list_members(&self, owner: &str) -> Vec<MemberSnapshot> {
        self.members
            .iter()
            .filter(|(_, member)| member.owner == owner)
            .map(|(address, member)| member.snapshot(address))
            .collect()
    }

    /// 删除 owner 名下成员及其全部未确认消息，并返回删除前快照。
    fn remove_member(&mut self, owner: &str, address: &AgentId) -> Result<MemberSnapshot> {
        let snapshot = self.member(owner, address)?.snapshot(address);
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
        &self,
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
        Ok(json!({"snapshot": host.agent_status(&target)?}))
    }

    /// 查询成员当前 Agent 句柄的幂等终态结果。
    fn result(
        &self,
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
        Ok(json!({"completed": outcome.is_some(), "outcome": outcome}))
    }

    /// 级联取消成员当前 Agent 句柄。
    fn cancel(
        &self,
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
        Ok(json!({"cancelled": host.cancel_agent(&target)?}))
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
}

impl AgentPlugin for TeammatePlugin {
    /// 保存可信插件 ID 并注册版本化 mailbox service。
    fn activate(&mut self, host: &dyn PluginHostApi, context: ActivationContext) -> Result<()> {
        self.plugin_id = Some(context.plugin_id);
        host.upsert_service(&ServiceSpec {
            name: TEAMMATE_SERVICE.into(),
            version: TEAMMATE_SERVICE_VERSION.into(),
            description: Some("管理隔离的 teammate 成员、短期邮箱、确认和续跑注入".into()),
        })
    }

    /// 注销 service 并清空仅对当前激活实例有效的短期状态。
    fn deactivate(&mut self, host: &dyn PluginHostApi) -> Result<()> {
        host.remove_service(TEAMMATE_SERVICE)?;
        self.plugin_id = None;
        self.state = TeamState::default();
        Ok(())
    }

    /// 返回模型可调用的 teammate 控制面工具。
    fn list_tools(&self) -> Vec<ToolSpec> {
        teammate_tools()
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
}

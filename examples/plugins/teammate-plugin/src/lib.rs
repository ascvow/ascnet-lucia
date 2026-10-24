//! 基于 Agent Runtime 控制面的 Teammate 协作插件。
//!
//! 插件拥有成员角色、短期邮箱、确认和消息注入规则；Host 只提供可信 controller
//! 身份、受限 Agent 派生及生命周期操作。

use agent_plugin::{
    core_events, export_plugin, ActivationContext, AgentHandle, AgentId, AgentOutcome, AgentPlugin,
    AgentSpawnRequest, AgentStatus, AgentViewSession, EventOutcome, ExtensionEvent, PluginEvent,
    PluginHostApi, Result, RunFinishEventData, ServiceCall, ServiceSpec, ToolCall,
    ToolPermissionScope, ToolResult, ToolSpec, UiColor, UiCursor, UiDeclaration, UiFrame, UiInput,
    UiInputEvent, UiLine, UiNavigationAction, UiNavigationRequest, UiPlacement, UiRenderRequest,
    UiSize, UiSpan, UiStyle, UiViewInstance,
};
use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, VecDeque},
    path::{Path, PathBuf},
};

/// Host 注册并由 manifest 授权的唯一派生策略。
const WORKER_PROFILE: &str = "worker";
/// Host 注入的规范化工作区根目录元数据键。
const WORKSPACE_ROOT_METADATA: &str = "workspace_root";
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
/// 回收台账最多保留的已结束成员结果数，保证终态结果仍可读取而不无限增长。
const MAX_RETIRED_MEMBERS: usize = 8;
/// 插件记住的最近结束运行数；用于判断成员所属 controller 运行是否已经结束。
const MAX_FINISHED_RUNS: usize = 64;

/// 待重试回收的 Runtime 句柄上限。
///
/// 回收只在目标终态后成功；刚取消或仍在收尾的句柄会先留在队列里，由后续帧重试。
/// 上限防止异常情况下队列无限增长。
const MAX_PENDING_COLLECTIONS: usize = 64;
/// 主界面右侧的团队摘要入口。
const TEAM_DOCK_VIEW: &str = "teammate-team-dock";
/// 替换主界面的团队工作台子视图。
const TEAM_WORKSPACE_VIEW: &str = "teammate-team-workspace";
/// 展示单个成员实时执行过程并接收交互消息的子视图。
const TEAM_SESSION_VIEW: &str = "teammate-member-session";

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
    /// 是否为 owner 指定的独立队长 Agent。
    captain: bool,
    /// 插件最近一次观察到的当前执行状态。
    status: AgentStatus,
    /// 当前成员尚未确认的消息数量。
    unread_messages: usize,
    /// 成员失败时的终态原因；仅失败后出现，属于加法字段。
    ///
    /// 没有它时列表只显示 `failed`，用户无法判断是模型预算、provider 还是工具问题，
    /// 只能再额外调用一次 `teammate_result`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// 插件维护的成员状态。
#[derive(Debug, Clone)]
struct Member {
    owner: String,
    role: String,
    /// 是否为 owner 指定的独立队长 Agent。
    captain: bool,
    current: AgentHandle,
    status: AgentStatus,
    /// 最近一次观察到的失败终态原因；成功或运行中保持 `None`。
    error: Option<String>,
    /// 最近一次通过工具调用管理该成员的 controller 运行 ID。
    ///
    /// 成员在运行结束后是否该解散，取决于管理它的那一轮是否已经结束；Service 调用
    /// 没有运行身份，因此保持 `None` 并由调用方显式 remove。
    last_controller_run: Option<String>,
}

impl Member {
    /// 生成不暴露 owner 内部索引的协议快照。
    fn snapshot(&self, address: &AgentId, unread_messages: usize) -> MemberSnapshot {
        MemberSnapshot {
            id: address.clone(),
            current_agent_id: self.current.id.clone(),
            role: self.role.clone(),
            captain: self.captain,
            status: self.status,
            unread_messages,
            error: self.error.clone(),
        }
    }
}

/// 已回收成员的终态结果，供 controller 在本轮结束后仍能读取队友产出。
#[derive(Debug, Clone)]
struct RetiredMember {
    owner: String,
    snapshot: MemberSnapshot,
    outcome: Option<AgentOutcome>,
}

/// 单个 WASM 实例内的成员目录和短期邮箱。
#[derive(Default)]
struct TeamState {
    members: BTreeMap<AgentId, Member>,
    mailboxes: BTreeMap<AgentId, VecDeque<TeammateMessage>>,
    /// 按回收顺序保存的终态结果；超出上限时淘汰最早的条目。
    retired: Vec<RetiredMember>,
    /// 最近结束的运行 ID，按到达顺序排列。
    finished_runs: VecDeque<String>,
    next_message_id: u64,
}

impl TeamState {
    /// 校验 owner 配额并登记一个已经成功派生的成员。
    fn add_member(
        &mut self,
        owner: &str,
        role: String,
        captain: bool,
        handle: AgentHandle,
        controller_run: Option<String>,
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
        if captain
            && self
                .members
                .values()
                .any(|member| member.owner == owner && member.captain)
        {
            return Err(anyhow!("owner `{owner}` 已存在队长 Agent"));
        }
        let address = handle.id.clone();
        let member = Member {
            owner: owner.to_string(),
            role,
            captain,
            current: handle,
            status: AgentStatus::Queued,
            error: None,
            last_controller_run: controller_run,
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

    /// 判断 owner 名下是否存在该活跃成员。
    fn contains_member(&self, owner: &str, address: &AgentId) -> bool {
        self.members
            .get(address)
            .is_some_and(|member| member.owner == owner)
    }

    /// 记录一次已结束的运行；只保留最近若干条，避免长期运行无限增长。
    fn remember_finished_run(&mut self, run_id: &str) {
        if self.finished_runs.iter().any(|finished| finished == run_id) {
            return;
        }
        while self.finished_runs.len() >= MAX_FINISHED_RUNS {
            self.finished_runs.pop_front();
        }
        self.finished_runs.push_back(run_id.to_string());
    }

    /// 判断指定运行是否已经成功结束。
    fn run_finished(&self, run_id: &str) -> bool {
        self.finished_runs.iter().any(|finished| finished == run_id)
    }

    /// 把成员最近一次被管理的 controller 运行更新为 `run_id`。
    fn note_controller_run(&mut self, owner: &str, address: &AgentId, run_id: Option<&str>) {
        let Some(run_id) = run_id else {
            return;
        };
        if let Ok(member) = self.member_mut(owner, address) {
            member.last_controller_run = Some(run_id.to_string());
        }
    }

    /// 返回当前可以回收的成员地址。
    ///
    /// 只回收同时满足以下条件的成员：状态已进入终态、所属 controller 运行已经结束、
    /// 邮箱没有未确认消息。未读消息是 controller 尚未消费的数据，不能随回收一起丢弃。
    fn retirement_candidates(&self, owner: &str) -> Vec<AgentId> {
        self.members
            .iter()
            .filter(|(_, member)| member.owner == owner)
            .filter(|(address, member)| {
                member.status.is_terminal()
                    && member
                        .last_controller_run
                        .as_deref()
                        .is_some_and(|run_id| self.run_finished(run_id))
                    && self.mailboxes.get(*address).map_or(0, VecDeque::len) == 0
            })
            .map(|(address, _)| address.clone())
            .collect()
    }

    /// 把成员移出活跃目录并登记终态结果，返回回收后的快照。
    fn retire(
        &mut self,
        owner: &str,
        address: &AgentId,
        outcome: Option<AgentOutcome>,
    ) -> Result<MemberSnapshot> {
        let unread = self.mailboxes.get(address).map_or(0, VecDeque::len);
        let mut snapshot = self.member(owner, address)?.snapshot(address, unread);
        // 回收时补齐失败原因：终态结果已经拿到，不需要 controller 再查询一次。
        if let Some(AgentOutcome::Failed { error }) = &outcome {
            snapshot.error = Some(error.clone());
        }
        self.members.remove(address);
        self.mailboxes.remove(address);
        while self.retired.len() >= MAX_RETIRED_MEMBERS {
            self.retired.remove(0);
        }
        self.retired.push(RetiredMember {
            owner: owner.to_string(),
            snapshot: snapshot.clone(),
            outcome,
        });
        Ok(snapshot)
    }

    /// 查找回收台账中的终态结果。
    fn retired_member(&self, owner: &str, address: &AgentId) -> Option<&RetiredMember> {
        self.retired
            .iter()
            .find(|retired| retired.owner == owner && &retired.snapshot.id == address)
    }

    /// 删除一条回收记录的 owner 视图，返回其快照。
    fn remove_retired(&mut self, owner: &str, address: &AgentId) -> Option<MemberSnapshot> {
        let index = self
            .retired
            .iter()
            .position(|retired| retired.owner == owner && &retired.snapshot.id == address)?;
        Some(self.retired.remove(index).snapshot)
    }

    /// 返回 owner 名下已回收成员的快照，顺序按回收顺序排列。
    fn list_retired(&self, owner: &str) -> Vec<MemberSnapshot> {
        self.retired
            .iter()
            .filter(|retired| retired.owner == owner)
            .map(|retired| retired.snapshot.clone())
            .collect()
    }

    /// 查找并校验 owner 对成员的访问权。
    fn member(&self, owner: &str, address: &AgentId) -> Result<&Member> {
        let member = self
            .members
            .get(address)
            .ok_or_else(|| self.missing_member_error(owner, address))?;
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
        let retired = self.retired_member(owner, address).is_some();
        match self.members.get_mut(address) {
            Some(member) if member.owner == owner => Ok(member),
            Some(_) => Err(anyhow!(
                "owner `{owner}` 无权访问成员 `{}`",
                address.as_str()
            )),
            None => Err(Self::member_error(owner, address, retired)),
        }
    }

    /// 生成统一的缺失成员错误；已回收成员给出可继续读取结果的提示。
    fn missing_member_error(&self, owner: &str, address: &AgentId) -> anyhow::Error {
        Self::member_error(
            owner,
            address,
            self.retired_member(owner, address).is_some(),
        )
    }

    /// 按是否已回收构造成员缺失错误。
    fn member_error(owner: &str, address: &AgentId, retired: bool) -> anyhow::Error {
        if retired {
            return anyhow!(
                "成员 `{}` 已回收：运行结果仍可用 teammate_result 读取，需要继续协作请重新 spawn",
                address.as_str()
            );
        }
        anyhow!(
            "Teammate 成员不存在：{}（owner `{owner}`）",
            address.as_str()
        )
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

    /// 在续跑成功入队后更新当前句柄并自动确认消息，返回被替换的旧句柄。
    ///
    /// 投胎会替换成员当前句柄；旧句柄已是终态，调用方应请 Runtime 回收它的会话与
    /// 名额，否则反复派活会持续累积 principal 的累计 Agent 数量。
    fn finish_dispatch(
        &mut self,
        owner: &str,
        address: &AgentId,
        message_id: u64,
        handle: AgentHandle,
    ) -> Result<Option<AgentId>> {
        self.ack(owner, address, message_id)?;
        let member = self
            .members
            .get_mut(address)
            .ok_or_else(|| anyhow!("Teammate 成员不存在：{}", address.as_str()))?;
        let previous = member.current.id.clone();
        member.current = handle;
        member.status = AgentStatus::Queued;
        // 新一次运行开始后，旧的失败原因已经不能代表当前句柄。
        member.error = None;
        Ok(Some(previous))
    }
}

/// 创建成员的公共请求。
#[derive(Debug, Deserialize)]
struct SpawnRequest {
    role: String,
    input: String,
    /// 显式指定该派生成员为独立队长；缺省时保持普通成员语义。
    #[serde(default)]
    captain: bool,
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
    Spawn {
        role: String,
        input: String,
        /// 显式指定该派生成员为独立队长；旧请求缺省为普通成员。
        #[serde(default)]
        captain: bool,
    },
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
    /// Host 注入的规范化工作区根目录，仅用于把派生任务中的绝对路径改写为相对路径。
    workspace_root: Option<PathBuf>,
    state: TeamState,
    selected_member: usize,
    navigation_sequence: u64,
    member_sessions: BTreeMap<AgentId, AgentViewSession>,
    /// 等待 Runtime 回收的已结束句柄；投胎会替换当前句柄，旧句柄的会话与名额需要释放。
    pending_collections: VecDeque<AgentId>,
}

impl TeammatePlugin {
    /// 返回当前插件自身作为工具调用 owner 的可信 ID。
    fn owner(&self) -> Result<&str> {
        self.plugin_id
            .as_deref()
            .ok_or_else(|| anyhow!("Teammate 插件尚未激活"))
    }

    /// 创建成员并登记首次派生句柄。
    ///
    /// `controller_run` 是发起派生的 controller 运行：它结束时应解散已经完成的成员。
    fn spawn(
        &mut self,
        host: &dyn PluginHostApi,
        owner: &str,
        request: SpawnRequest,
        controller_run: Option<&str>,
    ) -> Result<Value> {
        let role = validated_text(request.role, "role", 128)?;
        let input = validated_text(request.input, "input", 32 * 1024)?;
        let members = self.state.list_members(owner);
        if members.len() >= MAX_MEMBERS_PER_OWNER {
            return Err(anyhow!(
                "owner `{owner}` 的成员数已达到上限 {MAX_MEMBERS_PER_OWNER}"
            ));
        }
        if request.captain && members.iter().any(|member| member.captain) {
            return Err(anyhow!("owner `{owner}` 已存在队长 Agent"));
        }
        let identity = if request.captain {
            "teammate 队长 Agent"
        } else {
            "teammate 成员"
        };
        let agent_input =
            worker_agent_input(identity, &role, &input, self.workspace_root.as_deref());
        let handle =
            host.spawn_agent(&AgentSpawnRequest::new(WORKER_PROFILE, agent_input.clone()))?;
        let member = self.state.add_member(
            owner,
            role,
            request.captain,
            handle,
            controller_run.map(str::to_string),
        )?;
        let mut session = AgentViewSession::new(member.current_agent_id.clone());
        session.record_user_message(agent_input);
        self.member_sessions.insert(member.id.clone(), session);
        emit(
            host,
            "teammate.member.spawned",
            json!({"owner": owner, "member": member}),
        );
        Ok(json!({"member": member, "completed": false}))
    }

    /// 返回 owner 的成员目录；活跃成员与已回收成员分开列出。
    fn list(&self, owner: &str) -> Value {
        json!({
            "members": self.state.list_members(owner),
            "retired_members": self.state.list_retired(owner),
        })
    }

    /// 查询成员当前 Agent 句柄的 Runtime 状态。
    ///
    /// 状态为失败时顺带读取一次终态结果，把原因存入成员快照并在响应中返回，
    /// 避免调用方在状态里只能看到 `failed`。
    fn status(
        &mut self,
        host: &dyn PluginHostApi,
        owner: &str,
        request: MemberRequest,
        controller_run: Option<&str>,
    ) -> Result<Value> {
        self.state
            .note_controller_run(owner, &request.member_id, controller_run);
        let target = self
            .state
            .member(owner, &request.member_id)?
            .current
            .id
            .clone();
        let snapshot = host.agent_status(&target)?;
        let failure_reason = if snapshot.status == AgentStatus::Failed {
            match host.agent_result(&target) {
                Ok(Some(AgentOutcome::Failed { error })) => Some(error),
                _ => None,
            }
        } else {
            None
        };
        let member = self.state.member_mut(owner, &request.member_id)?;
        member.status = snapshot.status;
        if let Some(error) = &failure_reason {
            member.error = Some(error.clone());
        }
        let mut response = json!({ "snapshot": snapshot });
        if let Some(error) = failure_reason {
            response["error"] = json!(error);
        }
        Ok(response)
    }

    /// 查询成员当前 Agent 句柄的幂等终态结果。
    ///
    /// 成员在 controller 运行结束时已回收时，结果从有界台账返回，仍然幂等可读。
    fn result(
        &mut self,
        host: &dyn PluginHostApi,
        owner: &str,
        request: MemberRequest,
        controller_run: Option<&str>,
    ) -> Result<Value> {
        if let Some(retired) = self.state.retired_member(owner, &request.member_id) {
            return Ok(json!({
                "completed": true,
                "retired": true,
                "outcome": retired.outcome.clone(),
            }));
        }
        self.state
            .note_controller_run(owner, &request.member_id, controller_run);
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
        Ok(json!({
            "completed": outcome.is_some(),
            "retired": false,
            "outcome": outcome,
        }))
    }

    /// 级联取消成员当前 Agent 句柄。
    fn cancel(
        &mut self,
        host: &dyn PluginHostApi,
        owner: &str,
        request: MemberRequest,
        controller_run: Option<&str>,
    ) -> Result<Value> {
        self.state
            .note_controller_run(owner, &request.member_id, controller_run);
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
    ///
    /// 对已经自动回收的成员保持幂等：只清理台账记录，不重复操作 Runtime。
    fn remove(
        &mut self,
        host: &dyn PluginHostApi,
        owner: &str,
        request: MemberRequest,
    ) -> Result<Value> {
        if !self.state.contains_member(owner, &request.member_id) {
            if let Some(member) = self.state.remove_retired(owner, &request.member_id) {
                return Ok(json!({
                    "removed": true,
                    "cancelled": false,
                    "retired": true,
                    "member": member,
                }));
            }
        }
        let target = self
            .state
            .member(owner, &request.member_id)?
            .current
            .id
            .clone();
        let cancelled = host.cancel_agent(&target)?;
        let member = self.state.remove_member(owner, &request.member_id)?;
        // 成员移除后同步释放预先创建的 prompt 与历史视图，避免重复派生持续占用内存。
        self.member_sessions.remove(&request.member_id);
        // 取消是异步收尾：句柄进入待回收队列，由后续帧在终态后释放名额与会话。
        self.schedule_collection(target);
        emit(
            host,
            "teammate.member.removed",
            json!({"owner": owner, "member": member, "cancelled": cancelled}),
        );
        Ok(json!({
            "removed": true,
            "cancelled": cancelled,
            "retired": false,
            "member": member,
        }))
    }

    /// 回收终态成员并登记结果，返回本轮回收的成员快照。
    ///
    /// 只处理状态已终态、所属 controller 运行已经结束、且没有未读消息的成员：运行中的
    /// 成员需要继续工作，未读消息是 controller 尚未消费的数据，都不能随回收丢弃。
    /// Runtime 状态是权威值，先用它刷新缓存再判断，避免缓存停留在 `Running` 而漏收。
    fn retire_finished_members(&mut self, host: &dyn PluginHostApi) -> Vec<MemberSnapshot> {
        let owner = self.plugin_id.clone().unwrap_or_default();
        for member in self.state.list_members(&owner) {
            if let Ok(snapshot) = host.agent_status(&member.current_agent_id) {
                if let Ok(stored) = self.state.member_mut(&owner, &member.id) {
                    stored.status = snapshot.status;
                }
            }
        }
        let mut retired = Vec::new();
        for address in self.state.retirement_candidates(&owner) {
            let Some(target) = self
                .state
                .members
                .get(&address)
                .map(|member| member.current.id.clone())
            else {
                continue;
            };
            let outcome = host.agent_result(&target).ok().flatten();
            if let Ok(snapshot) = self.state.retire(&owner, &address, outcome) {
                self.member_sessions.remove(&address);
                // 成员已离开活跃目录，Runtime 侧同一投胎的会话与名额也应释放。
                self.schedule_collection(target);
                retired.push(snapshot);
            }
        }
        for member in &retired {
            emit(
                host,
                "teammate.member.retired",
                json!({"owner": owner, "member": member}),
            );
        }
        self.flush_collections(host);
        retired
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
        let handle = match host.continue_agent(&agent_plugin::AgentContinueRequest::new(
            target,
            input.clone(),
        )) {
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
        let previous = self.state.finish_dispatch(
            owner,
            &request.member_id,
            request.message_id,
            handle.clone(),
        )?;
        if let Some(previous) = previous {
            self.schedule_collection(previous);
        }
        let session = self
            .member_sessions
            .entry(request.member_id.clone())
            .or_insert_with(|| AgentViewSession::new(handle.id.clone()));
        session.replace_target(handle.id.clone());
        session.record_user_message(input);
        // 旧句柄此刻已经终态，立即尝试回收名额，避免连续派活攒下未回收的投胎。
        self.flush_collections(host);
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
        controller_run: Option<&str>,
        request: ServiceRequest,
    ) -> Result<Value> {
        match request {
            ServiceRequest::Spawn {
                role,
                input,
                captain,
            } => self.spawn(
                host,
                owner,
                SpawnRequest {
                    role,
                    input,
                    captain,
                },
                controller_run,
            ),
            ServiceRequest::List => Ok(self.list(owner)),
            ServiceRequest::Status { member_id } => {
                self.status(host, owner, MemberRequest { member_id }, controller_run)
            }
            ServiceRequest::Result { member_id } => {
                self.result(host, owner, MemberRequest { member_id }, controller_run)
            }
            ServiceRequest::Cancel { member_id } => {
                self.cancel(host, owner, MemberRequest { member_id }, controller_run)
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
        let content_height = usize::from(request.height).max(1);
        let mut lines = vec![
            ui_line(vec![
                ui_span(format!("{} members", members.len()), None, true),
                ui_span(
                    format!("  {active} running"),
                    Some(if active > 0 {
                        UiColor::Cyan
                    } else {
                        UiColor::Gray
                    }),
                    active > 0,
                ),
                ui_span(
                    format!("  {unread} messages"),
                    Some(if unread > 0 {
                        UiColor::Yellow
                    } else {
                        UiColor::Gray
                    }),
                    unread > 0,
                ),
            ]),
            render_captain_summary(members.iter().find(|member| member.captain)),
            ui_line(Vec::new()),
            ui_line(vec![ui_span("Members", Some(UiColor::Gray), true)]),
        ];
        let member_capacity = content_height.saturating_sub(5);
        let visible_members = if members.len() > member_capacity {
            member_capacity.saturating_sub(1)
        } else {
            members.len()
        };
        for member in members
            .iter()
            .filter(|member| !member.captain)
            .take(visible_members)
        {
            lines.push(ui_line(vec![
                ui_span(
                    status_marker(member.status),
                    Some(member_color(member)),
                    true,
                ),
                ui_span(format!(" {}", clipped(&member.role, 14)), None, false),
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
        let hidden_members = members.len().saturating_sub(visible_members);
        if hidden_members > 0 {
            lines.push(ui_line(vec![ui_span(
                format!("+{hidden_members} members"),
                Some(UiColor::Gray),
                false,
            )]));
        }
        while lines.len().saturating_add(1) < content_height {
            lines.push(ui_line(Vec::new()));
        }
        lines.push(ui_line(vec![
            ui_span(
                if request.focused { "› " } else { "  " },
                Some(if request.focused {
                    UiColor::Cyan
                } else {
                    UiColor::Gray
                }),
                request.focused,
            ),
            ui_action_span("Team workspace", request.focused),
            ui_span(
                if request.focused {
                    "  Enter"
                } else {
                    "  Click → Enter"
                },
                Some(if request.focused {
                    UiColor::Cyan
                } else {
                    UiColor::Gray
                }),
                request.focused,
            ),
        ]));
        lines
    }

    /// 渲染全屏团队工作台中的成员目录和当前成员详情。
    fn render_team_workspace(&mut self, request: &UiRenderRequest) -> Vec<UiLine> {
        let members = self.ui_members();
        let mut lines = vec![
            ui_line(vec![ui_span("Team workspace", Some(UiColor::Cyan), true)]),
            ui_line(vec![ui_span(
                format!("Members {}", members.len()),
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
                    Some(member_color(member)),
                    true,
                ),
                ui_span(
                    if member.captain {
                        format!("Captain  {}", clipped(&member.role, 19))
                    } else {
                        clipped(&member.role, 28)
                    },
                    Some(member_color(member)),
                    selected || member.captain,
                ),
                ui_span(
                    format!("  {}", status_label(member.status)),
                    Some(UiColor::Gray),
                    false,
                ),
                ui_span(
                    if member.unread_messages > 0 {
                        format!("  {} {}", member.unread_messages, "messages")
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
                "No members",
                Some(UiColor::Gray),
                false,
            )]));
            return lines;
        }
        if let Some(member) = members.get(self.selected_member) {
            lines.push(ui_line(Vec::new()));
            lines.push(ui_line(vec![ui_span(
                "Member details",
                Some(UiColor::Blue),
                true,
            )]));
            lines.push(ui_line(vec![ui_span(
                format!("Role  {}", member.role),
                Some(member_color(member)),
                member.captain,
            )]));
            lines.push(ui_line(vec![ui_span(
                format!(
                    "Type  {}",
                    if member.captain { "Captain" } else { "Member" }
                ),
                Some(if member.captain {
                    UiColor::Magenta
                } else {
                    UiColor::Gray
                }),
                member.captain,
            )]));
            lines.push(ui_line(vec![ui_span(
                format!("{}  {}", "Status", status_label(member.status)),
                Some(status_color(member.status)),
                false,
            )]));
            if let Some(error) = &member.error {
                // 失败原因可能是预算或 provider 的完整错误链，截断到一行避免把面板撑开。
                lines.push(ui_line(vec![ui_span(
                    format!("Error  {}", clipped(error, 60)),
                    Some(UiColor::Red),
                    false,
                )]));
            }
            lines.push(ui_line(vec![ui_span(
                format!("{}  {}", "Address", member.id.as_str()),
                Some(UiColor::Gray),
                false,
            )]));
            lines.push(ui_line(vec![ui_span(
                format!("{}  {}", "Current", member.current_agent_id.as_str()),
                Some(UiColor::Gray),
                false,
            )]));
            lines.push(ui_line(vec![ui_span(
                "Open member session",
                Some(UiColor::Green),
                true,
            )]));
        }
        lines
    }

    /// 渲染与主 Agent 一致的成员会话时间线和可编辑输入框。
    fn render_member_session(&self, request: &UiRenderRequest) -> Vec<UiLine> {
        let Some(instance_id) = request.instance_id.as_deref() else {
            return vec![ui_line(vec![ui_span(
                "Member session is missing an instance ID",
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
            // 回收后成员会话视图可能仍停留在导航栈里：说明原因，而不是报“找不到”。
            let released = self
                .state
                .list_retired(owner)
                .iter()
                .any(|member| member.id.as_str() == instance_id);
            return vec![ui_line(vec![ui_span(
                if released {
                    "Member completed and was released"
                } else {
                    "Member not found"
                },
                Some(if released {
                    UiColor::Gray
                } else {
                    UiColor::Red
                }),
                false,
            )])];
        };
        let Some(session) = self.member_sessions.get(&member.id) else {
            return vec![ui_line(vec![ui_span(
                "Connecting to member session",
                Some(UiColor::Gray),
                false,
            )])];
        };
        session.render(request.width, request.height)
    }

    /// 把共享 Agent 输入光标转换为成员子视图帧内的相对坐标。
    fn member_session_cursor(&self, request: &UiRenderRequest) -> Option<UiCursor> {
        let instance_id = request.instance_id.as_deref()?;
        let owner = self.plugin_id.as_deref().unwrap_or_default();
        let member = self
            .state
            .list_members(owner)
            .into_iter()
            .find(|member| member.id.as_str() == instance_id)?;
        let session = self.member_sessions.get(&member.id)?;
        session.cursor_position(request.width, request.height)
    }

    /// 把成员会话的键盘或滚轮输入交给共享 Agent 视图，并在续跑产生新句柄时更新成员映射。
    fn handle_member_session_input(
        &mut self,
        host: &dyn PluginHostApi,
        instance_id: Option<&str>,
        event: UiInputEvent,
    ) {
        let Some(instance_id) = instance_id else {
            return;
        };
        let Some(member_id) = self
            .member_sessions
            .keys()
            .find(|member_id| member_id.as_str() == instance_id)
            .cloned()
        else {
            return;
        };
        let continued = self
            .member_sessions
            .get_mut(&member_id)
            .and_then(|session| session.handle_input(host, &event));
        if let Some(handle) = continued {
            let owner = self.plugin_id.clone().unwrap_or_default();
            let mut previous = None;
            if let Ok(member) = self.state.member_mut(&owner, &member_id) {
                previous = Some(member.current.id.clone());
                member.current = handle;
                member.status = AgentStatus::Queued;
                member.error = None;
            }
            // 成员会话里的每轮新任务也会投胎，旧句柄同样要回收。
            if let Some(previous) = previous {
                self.schedule_collection(previous);
            }
        }
        self.flush_collections(host);
    }

    /// 刷新当前工具 owner 名下全部成员的 Runtime 状态缓存。
    ///
    /// 状态进入失败终态时顺带读取一次幂等的终态结果，把原因记到成员快照里；否则
    /// 团队面板只能显示 `failed`，用户无法判断是模型预算、provider 还是工具问题。
    /// 原因只取一次，已记录的成员不会在后续帧重复查询。
    fn refresh_ui_statuses(&mut self, host: &dyn PluginHostApi) {
        self.flush_collections(host);
        let owner = self.plugin_id.clone().unwrap_or_default();
        let targets = self
            .state
            .list_members(&owner)
            .into_iter()
            .map(|member| (member.id, member.current_agent_id, member.error.is_some()))
            .collect::<Vec<_>>();
        for (address, target, has_error) in targets {
            if let Ok(snapshot) = host.agent_status(&target) {
                if let Ok(member) = self.state.member_mut(&owner, &address) {
                    member.status = snapshot.status;
                }
            }
            let failed_without_reason = !has_error
                && self
                    .state
                    .members
                    .get(&address)
                    .is_some_and(|member| member.status == AgentStatus::Failed);
            if !failed_without_reason {
                continue;
            }
            let error = match host.agent_result(&target) {
                Ok(Some(AgentOutcome::Failed { error })) => Some(error),
                _ => None,
            };
            if let (Some(error), Ok(member)) = (error, self.state.member_mut(&owner, &address)) {
                member.error = Some(error);
            }
        }
    }

    /// 让共享 Agent 主界面刷新成员当前句柄的状态与增量事件。
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
        let session = self
            .member_sessions
            .entry(member.id.clone())
            .or_insert_with(|| AgentViewSession::new(member.current_agent_id.clone()));
        session.replace_target(member.current_agent_id);
        session.refresh(host);
        if let Some(status) = session.status() {
            if let Ok(stored) = self.state.member_mut(&owner, &member.id) {
                stored.status = status;
            }
        }
    }

    /// 请求 Runtime 回收一个已终态句柄；尚未终态时留待后续帧重试。
    ///
    /// 回收会释放该次投胎的私有会话与累计名额，因此每次替换或删除成员都必须登记，
    /// 否则长会话里的反复派活会不断累积 principal 的累计 Agent 数量。
    fn schedule_collection(&mut self, target: AgentId) {
        if self.pending_collections.len() >= MAX_PENDING_COLLECTIONS {
            self.pending_collections.pop_front();
        }
        self.pending_collections.push_back(target);
    }

    /// 逐帧重试待回收句柄，把成功或已回收的条目移出队列。
    fn flush_collections(&mut self, host: &dyn PluginHostApi) {
        let pending = std::mem::take(&mut self.pending_collections);
        for target in pending {
            // 错误表示目标仍在运行或已不可管理；两者都保留一次重试机会。
            if host.collect_agent(&target).is_err()
                && self.pending_collections.len() < MAX_PENDING_COLLECTIONS
            {
                self.pending_collections.push_back(target);
            }
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
                    title: Some("Team".into()),
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
            .or_insert_with(|| AgentViewSession::new(member.current_agent_id.clone()));
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
}

impl AgentPlugin for TeammatePlugin {
    /// 保存可信插件 ID 与工作区根目录，并提供版本化 mailbox service。
    fn activate(&mut self, host: &dyn PluginHostApi, context: ActivationContext) -> Result<()> {
        self.workspace_root = activation_workspace_root(&context);
        self.plugin_id = Some(context.plugin_id);
        host.upsert_service(&ServiceSpec {
            name: TEAMMATE_SERVICE.into(),
            version: TEAMMATE_SERVICE_VERSION.into(),
            description: Some("管理隔离的 teammate 成员、短期邮箱、确认和续跑注入".into()),
        })?;
        Ok(())
    }

    /// 注销 service，并清空仅对当前激活实例有效的短期状态。
    fn deactivate(&mut self, host: &dyn PluginHostApi) -> Result<()> {
        host.remove_service(TEAMMATE_SERVICE)?;
        self.plugin_id = None;
        self.workspace_root = None;
        self.state = TeamState::default();
        self.member_sessions.clear();
        Ok(())
    }

    /// 每轮 Agent 运行结束时回收已经完成且不再被管理的成员。
    ///
    /// 运行结束是"本轮工作已交付"的可靠信号：成员结果已写入 Runtime，controller 不会
    /// 在同一轮里再对它 dispatch 后续任务。仍处于排队或运行中的成员继续保留，避免中断
    /// 正在执行的工作；成员自身的运行晚于 controller 结束时，会在下一次运行时补收。
    fn on_event(&mut self, host: &dyn PluginHostApi, event: &mut PluginEvent) -> EventOutcome {
        if event.name() != core_events::RUN_FINISH {
            return EventOutcome::ready();
        }
        let Ok(data) = event.parse::<RunFinishEventData>() else {
            return EventOutcome::ready();
        };
        self.state.remember_finished_run(&data.run_id);
        self.retire_finished_members(host);
        EventOutcome::ready()
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
        let controller_run = call.run_id.as_deref();
        let content = match operation.as_str() {
            "teammate_spawn" => {
                self.spawn(host, &owner, decode(call.args.clone())?, controller_run)?
            }
            "teammate_list" => self.list(&owner),
            "teammate_status" => {
                self.status(host, &owner, decode(call.args.clone())?, controller_run)?
            }
            "teammate_result" => {
                self.result(host, &owner, decode(call.args.clone())?, controller_run)?
            }
            "teammate_cancel" => {
                self.cancel(host, &owner, decode(call.args.clone())?, controller_run)?
            }
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
            // Service 调用没有 controller 运行身份，成员保留到调用方显式 remove。
            None,
            request,
        )
    }

    /// 声明右侧团队入口与可动态打开的全屏工作台。
    fn describe_ui(&self) -> Vec<UiDeclaration> {
        vec![
            UiDeclaration {
                plugin_id: String::new(),
                view_id: TEAM_DOCK_VIEW.into(),
                title: "Team".into(),
                placement: UiPlacement::Right,
                size: UiSize {
                    width: Some(36),
                    height: None,
                },
                focusable: true,
                input_triggers: Vec::new(),
            },
            UiDeclaration {
                plugin_id: String::new(),
                view_id: TEAM_WORKSPACE_VIEW.into(),
                title: "Team workspace".into(),
                placement: UiPlacement::Subview,
                size: UiSize::default(),
                focusable: true,
                input_triggers: Vec::new(),
            },
            UiDeclaration {
                plugin_id: String::new(),
                view_id: TEAM_SESSION_VIEW.into(),
                title: "Member session".into(),
                placement: UiPlacement::Subview,
                size: UiSize::default(),
                focusable: true,
                input_triggers: Vec::new(),
            },
        ]
    }

    /// 根据宿主分配尺寸渲染团队摘要或全屏工作台。
    ///
    /// 团队摘要在当前 owner 没有成员时返回隐藏帧，子视图不受该条件影响。
    fn render_ui(&mut self, request: UiRenderRequest) -> Option<UiFrame> {
        let visible = request.view_id != TEAM_DOCK_VIEW || !self.ui_members().is_empty();
        let cursor = (request.view_id == TEAM_SESSION_VIEW)
            .then(|| self.member_session_cursor(&request))
            .flatten();
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
            cursor,
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
                // 状态由进入后的视图刷新统一读取，导航本身不重复查询全部成员。
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
                self.handle_member_session_input(
                    host,
                    input.instance_id.as_deref(),
                    UiInputEvent::Key { code, modifiers },
                );
            }
            UiInputEvent::Mouse { kind, x, y } if input.view_id == TEAM_SESSION_VIEW => {
                self.handle_member_session_input(
                    host,
                    input.instance_id.as_deref(),
                    UiInputEvent::Mouse { kind, x, y },
                );
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

/// 构造团队入口操作文本；聚焦时切换为高亮选中态，未聚焦时保留可点击链接语义。
fn ui_action_span(text: impl Into<String>, focused: bool) -> UiSpan {
    UiSpan {
        text: text.into(),
        style: UiStyle {
            foreground: Some(if focused {
                UiColor::Cyan
            } else {
                UiColor::Green
            }),
            bold: true,
            underlined: true,
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

/// 返回成员状态标签。
fn status_label(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Ready => "Ready",
        AgentStatus::Queued => "Queued",
        AgentStatus::Running => "Running",
        AgentStatus::Succeeded => "Completed",
        AgentStatus::Failed => "Failed",
        AgentStatus::Cancelled => "Cancelled",
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

/// 返回成员在团队视图中的强调色；队长固定使用洋红色以区别普通成员。
fn member_color(member: &MemberSnapshot) -> UiColor {
    if member.captain {
        UiColor::Magenta
    } else {
        status_color(member.status)
    }
}

/// 渲染独立队长成员摘要；未创建队长时不再把主 Agent 伪装为队长。
fn render_captain_summary(captain: Option<&MemberSnapshot>) -> UiLine {
    let Some(captain) = captain else {
        return ui_line(vec![ui_span("◇ No captain", Some(UiColor::Gray), false)]);
    };
    ui_line(vec![
        ui_span("● Captain  ", Some(UiColor::Magenta), true),
        ui_span(clipped(&captain.role, 10), Some(UiColor::Magenta), true),
        ui_span(
            format!("  {}", status_label(captain.status)),
            Some(UiColor::Gray),
            false,
        ),
    ])
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

/// 从 Host 激活元数据读取规范化工作区根；缺失或非绝对值保持兼容的无根目录模式。
fn activation_workspace_root(context: &ActivationContext) -> Option<PathBuf> {
    let root = PathBuf::from(context.metadata.get(WORKSPACE_ROOT_METADATA)?);
    if !root.is_absolute()
        || root
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return None;
    }
    Some(root)
}

/// 构造派生成员的完整启动输入，并明确文件工具只能接收工作区相对路径。
fn worker_agent_input(
    identity: &str,
    role: &str,
    input: &str,
    workspace_root: Option<&Path>,
) -> String {
    let input = workspace_relative_task_input(input, workspace_root);
    format!(
        "你是{identity}，角色为 `{role}`。请完成以下任务。使用文件类工具时必须传入工作区相对路径；任务文本中的工作区绝对路径应先转换为相对路径，不要直接传给工具。\n\n{input}"
    )
}

/// 把任务正文中指向当前工作区的绝对路径前缀改写为 `.`，其他路径和文本保持原样。
fn workspace_relative_task_input(input: &str, workspace_root: Option<&Path>) -> String {
    let Some(root) = workspace_root.and_then(Path::to_str) else {
        return input.to_string();
    };
    let root = root.trim_end_matches(std::path::MAIN_SEPARATOR);
    if root.is_empty() {
        return input.to_string();
    }

    let mut output = String::with_capacity(input.len());
    let mut cursor = 0;
    while let Some(offset) = input[cursor..].find(root) {
        let start = cursor + offset;
        let end = start + root.len();
        let starts_path = start == 0
            || input[..start].chars().next_back().is_some_and(|character| {
                !character.is_ascii_alphanumeric()
                    && !matches!(character, '/' | '\\' | '.' | '_' | '-')
            });
        let follows_path_boundary =
            end == input.len() || input[end..].starts_with(std::path::MAIN_SEPARATOR);
        output.push_str(&input[cursor..start]);
        if starts_path && follows_path_boundary {
            output.push('.');
        } else {
            output.push_str(root);
        }
        cursor = end;
    }
    output.push_str(&input[cursor..]);
    output
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

/// 返回所有 teammate 控制面工具及其可信权限 scope。
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
            "用于复杂项目中两个或更多相互独立、互不阻塞且适合并行推进的子任务，或确实需要与主任务解耦的独立审查。只有并行收益明显高于协调成本时，才用受限 worker profile 创建带角色的 teammate，并提供完整可执行的任务输入。普通单点需求、单一路径代码审查、强顺序依赖任务，以及当前 Agent 可连续完成的工作通常不需要 teammate。可将独立成员指定为唯一队长；调用立即返回成员地址。成员完成后先在本轮内用 teammate_result 取回结果；本轮结束时仍处于完成态的成员会被自动回收（结果保留在回收台账里仍可读取），运行中的成员继续保留。",
            json!({
                "type": "object",
                "properties": {
                    "role": {"type": "string", "minLength": 1, "maxLength": 128},
                    "input": {"type": "string", "minLength": 1},
                    "captain": {
                        "type": "boolean",
                        "default": false,
                        "description": "是否将该独立成员设为队长；每个团队最多一个"
                    }
                },
                "required": ["role", "input"],
                "additionalProperties": false
            }),
        ),
        ToolSpec::new(
            "teammate_list",
            "列出当前 controller 的活跃成员、已回收成员及其当前执行句柄。",
            json!({"type": "object", "properties": {}, "additionalProperties": false}),
        ),
        ToolSpec::new(
            "teammate_status",
            "查询成员当前执行句柄的状态。",
            member_schema(),
        ),
        ToolSpec::new(
            "teammate_result",
            "读取成员当前执行句柄的幂等终态结果；成员被自动回收后仍可从回收台账读取。",
            member_schema(),
        ),
        ToolSpec::new(
            "teammate_cancel",
            "级联取消成员当前执行句柄及其后代。",
            member_schema(),
        ),
        ToolSpec::new(
            "teammate_remove",
            "立即取消并删除成员及其全部未确认消息，释放成员配额；已回收成员只清理台账记录。正常情况下不需要手动调用，完成态成员会在本轮运行时结束时自动回收。",
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
    .into_iter()
    .map(|tool| {
        if matches!(tool.name.as_str(), "teammate_spawn" | "teammate_dispatch") {
            // 派生或续跑成员会消耗模型与 Runtime 资源，必须进入权限策略。
            tool
        } else {
            tool.with_permission_scope(ToolPermissionScope::Control)
        }
    })
    .collect()
}

export_plugin!(TeammatePlugin);

#[cfg(test)]
mod tests {
    use super::*;
    use agent_plugin::AgentLineage;

    /// 派生与续跑成员保持 unknown，其余短控制面操作才声明为 control。
    #[test]
    fn teammate_tools_separate_delegation_from_control_scope() {
        let tools = teammate_tools();
        assert!(!tools.is_empty());
        for tool in tools {
            let expected = if matches!(tool.name.as_str(), "teammate_spawn" | "teammate_dispatch") {
                ToolPermissionScope::Unknown
            } else {
                ToolPermissionScope::Control
            };
            assert_eq!(
                tool.permission_scope, expected,
                "工具 {} scope 错误",
                tool.name
            );
        }
    }

    /// 派生工具描述必须要求真实并行收益，普通单点任务不能仅因工具可用而派生成员。
    #[test]
    fn teammate_spawn_description_requires_parallel_value() {
        let description = teammate_tools()
            .into_iter()
            .find(|tool| tool.name == "teammate_spawn")
            .expect("应暴露 teammate_spawn")
            .description;

        assert!(description.contains("复杂项目"));
        assert!(description.contains("互不阻塞"));
        assert!(description.contains("并行收益明显高于协调成本"));
        assert!(description.contains("普通单点需求"));
        assert!(description.contains("当前 Agent 可连续完成"));
        assert!(
            description.contains("自动回收"),
            "工具描述必须说明完成即解散"
        );
        assert!(
            description.contains("回收台账"),
            "工具描述必须说明结果仍可读取"
        );
    }

    /// 派生成员输入必须把当前工作区绝对路径转换为相对路径，并保留外部路径原文。
    #[test]
    fn worker_input_uses_workspace_relative_paths() {
        let root = Path::new("/workspace/project");
        let input = worker_agent_input(
            "teammate 成员",
            "审查员",
            "检查 /workspace/project/src/lib.rs，并对照 /workspace/project-other/README.md 与 /tmp/workspace/project/src/main.rs",
            Some(root),
        );

        assert!(input.contains("文件类工具时必须传入工作区相对路径"));
        assert!(input.contains("检查 ./src/lib.rs"));
        assert!(input.contains("/workspace/project-other/README.md"));
        assert!(input.contains("/tmp/workspace/project/src/main.rs"));
        assert!(!input.contains("/workspace/project/src/lib.rs"));
    }

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
            .add_member(
                "owner-a",
                "reviewer".into(),
                false,
                handle("member-a"),
                None,
            )
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
            .add_member("owner-a", "worker".into(), false, handle("member-a"), None)
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
            .add_member("owner", "worker".into(), false, handle("member"), None)
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

    /// 每个 owner 只能登记一个独立队长，且队长身份必须出现在成员快照中。
    #[test]
    fn captain_is_unique_teammate_member() {
        let mut state = TeamState::default();
        let captain = state
            .add_member("owner", "lead".into(), true, handle("captain"), None)
            .expect("首个队长应登记成功");
        assert!(captain.captain);
        let error = state
            .add_member("owner", "backup".into(), true, handle("backup"), None)
            .expect_err("同一 owner 不应登记第二个队长");
        assert!(error.to_string().contains("已存在队长 Agent"));
        state
            .add_member("other", "lead".into(), true, handle("other-captain"), None)
            .expect("不同 owner 可拥有各自队长");
    }

    /// 只有终态、所属 controller 运行已结束且没有未读消息的成员才可被回收。
    #[test]
    fn retirement_requires_terminal_finished_and_acked() {
        let mut state = TeamState::default();
        let done = state
            .add_member(
                "owner",
                "reviewer".into(),
                false,
                handle("done"),
                Some("run-a".into()),
            )
            .expect("成员应登记成功");
        let fresh = state
            .add_member(
                "owner",
                "reviewer-2".into(),
                false,
                handle("fresh"),
                Some("run-b".into()),
            )
            .expect("成员应登记成功");
        let service = state
            .add_member("owner", "service".into(), false, handle("service"), None)
            .expect("service 成员应登记成功");
        for address in [&done.id, &fresh.id, &service.id] {
            state.member_mut("owner", address).expect("成员存在").status = AgentStatus::Succeeded;
        }

        // controller 运行尚未结束：不能回收任何成员。
        assert!(state.retirement_candidates("owner").is_empty());
        state.remember_finished_run("run-a");
        assert_eq!(
            state.retirement_candidates("owner"),
            vec![done.id.clone()],
            "只回收运行已结束的终态成员"
        );

        // 未读消息是尚未消费的数据，必须阻止回收。
        state
            .send(
                "owner",
                MessageSender::Controller {
                    agent_id: AgentId::parse("controller").expect("测试 ID 应合法"),
                },
                SendRequest {
                    recipient: done.id.clone(),
                    topic: "review.requested".into(),
                    payload: Value::Null,
                },
            )
            .expect("消息应发送成功");
        assert!(state.retirement_candidates("owner").is_empty());
        let message = state
            .inbox("owner", &done.id, 20)
            .expect("邮箱应可读")
            .remove(0);
        state
            .ack("owner", &done.id, message.id)
            .expect("消息应确认成功");
        assert_eq!(state.retirement_candidates("owner"), vec![done.id.clone()]);
    }

    /// 回收后台账保留终态结果，缺失成员错误应指向残留结果。
    #[test]
    fn retired_member_keeps_outcome_and_reports_recycle() {
        let mut state = TeamState::default();
        let member = state
            .add_member(
                "owner",
                "worker".into(),
                false,
                handle("retired"),
                Some("run".into()),
            )
            .expect("成员应登记成功");
        state
            .member_mut("owner", &member.id)
            .expect("成员存在")
            .status = AgentStatus::Failed;
        let outcome = Some(AgentOutcome::Failed {
            error: "boom".into(),
        });

        state
            .retire("owner", &member.id, outcome.clone())
            .expect("终态成员应可回收");

        assert!(state.list_members("owner").is_empty());
        assert_eq!(state.list_retired("owner").len(), 1);
        assert_eq!(
            state
                .retired_member("owner", &member.id)
                .expect("台账应保留结果")
                .outcome,
            outcome
        );
        assert_eq!(
            state
                .retired_member("owner", &member.id)
                .expect("台账应保留快照")
                .snapshot
                .error
                .as_deref(),
            Some("boom"),
            "回收快照必须携带失败原因"
        );
        let error = state
            .member("owner", &member.id)
            .expect_err("活跃目录不应再有该成员");
        assert!(error.to_string().contains("已回收"), "{error}");
        assert_eq!(
            state
                .remove_retired("owner", &member.id)
                .expect("台账记录应可删除")
                .id,
            member.id
        );
        assert!(state.remove_retired("owner", &member.id).is_none());
    }

    /// 回收台账必须有界，超出上限时淘汰最早的终态结果。
    #[test]
    fn retired_ledger_is_bounded() {
        let mut state = TeamState::default();
        for index in 0..MAX_RETIRED_MEMBERS + 2 {
            let id = format!("member-{index}");
            let address = AgentId::parse(&id).expect("测试 ID 应合法");
            state
                .add_member("owner", "worker".into(), false, handle(&id), None)
                .expect("成员应登记成功");
            state
                .member_mut("owner", &address)
                .expect("成员存在")
                .status = AgentStatus::Succeeded;
            state
                .retire("owner", &address, None)
                .expect("终态成员应可回收");
        }

        assert_eq!(state.retired.len(), MAX_RETIRED_MEMBERS);
        assert!(state
            .retired_member("owner", &AgentId::parse("member-0").expect("测试 ID"))
            .is_none());
        assert!(state
            .retired_member(
                "owner",
                &AgentId::parse(format!("member-{}", MAX_RETIRED_MEMBERS + 1)).expect("测试 ID")
            )
            .is_some());
    }

    /// 成员快照的失败原因必须是加法字段：正常成员不输出空字段，失败后可直接看到。
    #[test]
    fn member_snapshot_serializes_optional_failure_reason() {
        let mut state = TeamState::default();
        let member = state
            .add_member("owner", "worker".into(), false, handle("member"), None)
            .expect("成员应登记成功");

        let running = serde_json::to_value(&member).expect("快照应可序列化");
        assert!(
            running.get("error").is_none(),
            "运行中的成员不应输出空 error 字段"
        );

        state
            .member_mut("owner", &member.id)
            .expect("成员存在")
            .error = Some("principal 的 Agent 模型请求累计次数超过上限".into());
        let failed = state.list_members("owner").remove(0);
        let value = serde_json::to_value(&failed).expect("快照应可序列化");
        assert_eq!(
            value["error"],
            "principal 的 Agent 模型请求累计次数超过上限"
        );
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
            .add_member("teammate", "lead".into(), true, handle("captain-ui"), None)
            .expect("UI 测试队长应登记成功");
        plugin
            .state
            .add_member(
                "teammate",
                "reviewer".into(),
                false,
                handle("member-ui"),
                None,
            )
            .expect("UI 测试成员应登记成功");

        let declarations = plugin.describe_ui();
        assert_eq!(declarations.len(), 3);
        assert_eq!(declarations[0].placement, UiPlacement::Right);
        assert_eq!(declarations[0].size.width, Some(36));
        assert!(declarations[0].focusable);
        assert_eq!(declarations[1].placement, UiPlacement::Subview);

        let frame = plugin
            .render_ui(UiRenderRequest {
                session_id: None,
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
        assert!(text.contains("members"), "{text}");
        assert!(text.contains("reviewer"), "{text}");
        assert!(text.contains('◌'), "{text}");
        assert!(text.contains("Captain  lead  Queued"), "{text}");
        assert!(text.contains("Queued"), "{text}");
        assert!(text.contains("Team workspace  Click → Enter"), "{text}");
        assert_eq!(frame.lines.len(), 16);
        assert!(frame.lines[3]
            .spans
            .iter()
            .any(|span| span.text == "Members"));
        assert!(frame.lines[4]
            .spans
            .iter()
            .any(|span| span.text.contains("reviewer")));
        assert!(frame.lines[1].spans.iter().any(|span| {
            span.text.contains("Captain") && span.style.foreground == Some(UiColor::Magenta)
        }));

        let focused = plugin
            .render_ui(UiRenderRequest {
                session_id: None,
                plugin_id: "teammate".into(),
                view_id: TEAM_DOCK_VIEW.into(),
                instance_id: None,
                width: 30,
                height: 16,
                focused: true,
                frame: 2,
            })
            .expect("聚焦团队摘要应返回可见帧");
        let action = focused.lines.last().expect("团队摘要应包含入口行");
        assert_eq!(action.spans[0].text, "› ");
        assert_eq!(action.spans[2].text, "  Enter");
        assert!(action.spans[1].style.underlined);

        let workspace = plugin
            .render_ui(UiRenderRequest {
                session_id: None,
                plugin_id: "teammate".into(),
                view_id: TEAM_WORKSPACE_VIEW.into(),
                instance_id: Some("team".into()),
                width: 80,
                height: 24,
                focused: true,
                frame: 3,
            })
            .expect("团队工作台应返回可见帧");
        let workspace_text = workspace
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.text.as_str())
            .collect::<String>();
        assert!(workspace_text.contains("Queued"), "{workspace_text}");
        assert!(
            workspace_text.contains("Open member session"),
            "{workspace_text}"
        );

        let member = plugin.state.list_members("teammate").remove(0);
        plugin.member_sessions.insert(
            member.id.clone(),
            AgentViewSession::new(member.current_agent_id.clone()),
        );
        let session = plugin
            .render_ui(UiRenderRequest {
                session_id: None,
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
        assert!(
            session_text.contains("Waiting for Agent events"),
            "{session_text}"
        );
        assert_eq!(session.cursor, Some(UiCursor { x: 2, y: 22 }));
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
                session_id: None,
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

    /// 已回收成员的会话视图应说明已完成并释放，而不是报“找不到”。
    #[test]
    fn retired_member_session_explains_release() {
        let mut plugin = TeammatePlugin {
            plugin_id: Some("teammate".into()),
            ..TeammatePlugin::default()
        };
        let member = plugin
            .state
            .add_member(
                "teammate",
                "worker".into(),
                false,
                handle("released-ui"),
                None,
            )
            .expect("成员应登记成功");
        plugin
            .state
            .member_mut("teammate", &member.id)
            .expect("成员存在")
            .status = AgentStatus::Succeeded;
        plugin
            .state
            .retire("teammate", &member.id, None)
            .expect("完成成员应可回收");

        let frame = plugin
            .render_ui(UiRenderRequest {
                session_id: None,
                plugin_id: "teammate".into(),
                view_id: TEAM_SESSION_VIEW.into(),
                instance_id: Some(member.id.as_str().into()),
                width: 60,
                height: 10,
                focused: true,
                frame: 1,
            })
            .expect("回收后的成员会话应返回帧");
        let text = frame
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.text.as_str())
            .collect::<String>();
        assert!(text.contains("completed and was released"), "{text}");
    }
}

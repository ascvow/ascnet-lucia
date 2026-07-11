//! 基于 Agent Runtime 的动态 DAG 工作流插件。
//!
//! 插件拥有工作流定义、依赖调度和失败传播；Host 只提供受限 Agent 派生、观察与取消。

use agent_plugin::{
    export_plugin, ActivationContext, AgentId, AgentOutcome, AgentPlugin, AgentSpawnRequest,
    PluginHostApi, PromptContribution, Result, ToolCall, ToolResult, ToolSpec,
};
use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

/// manifest 允许工作流节点使用的唯一 Agent 派生策略。
const WORKER_PROFILE: &str = "worker";
/// 单个工作流允许的最大并行节点数，防止 Guest 绕过应用级资源规划。
const MAX_PARALLELISM: usize = 32;
/// 节点提示词的最大字节数，避免状态和跨 ABI 请求无限增长。
const MAX_PROMPT_BYTES: usize = 32 * 1024;
/// 引导主 Agent 合理创建 DAG 工作流的 developer 提示 ID。
const WORKFLOW_ORCHESTRATION_PROMPT_ID: &str = "workflow-orchestration";
/// 引导主 Agent 选择工作流工具的编排规则。
const WORKFLOW_ORCHESTRATION_PROMPT: &str = "当任务包含明确依赖关系、可并行的多个阶段，或需要可追踪的失败传播时，优先使用 workflow_create 建立 DAG 工作流。先定义节点与依赖，再在节点齐备后使用 workflow_seal，并通过 workflow_tick 推进和观察执行。简单的一次性任务、无法明确依赖关系的探索任务不要创建工作流。";

/// 保存当前组件实例内所有动态工作流。
#[derive(Default)]
struct WorkflowPlugin {
    workflows: BTreeMap<String, Workflow>,
    next_workflow_id: u64,
}

/// 一个可在封存前持续追加节点的 DAG 工作流。
#[derive(Debug, Serialize)]
struct Workflow {
    id: String,
    name: String,
    max_parallelism: usize,
    failure_policy: FailurePolicy,
    sealed: bool,
    status: WorkflowStatus,
    nodes: BTreeMap<String, WorkflowNode>,
}

/// 工作流整体状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum WorkflowStatus {
    Active,
    Succeeded,
    Failed,
    Cancelled,
}

/// 节点失败后的调度策略。
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum FailurePolicy {
    /// 首次失败后停止整个工作流并取消仍在运行的节点。
    #[default]
    Stop,
    /// 只跳过依赖失败节点的下游节点，继续独立分支。
    Continue,
}

/// 工作流中的一个 Agent 任务节点。
#[derive(Debug, Serialize)]
struct WorkflowNode {
    id: String,
    prompt: String,
    dependencies: Vec<String>,
    status: NodeStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_id: Option<AgentId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// 节点从等待到终态的稳定状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum NodeStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Skipped,
}

impl NodeStatus {
    /// 判断节点是否已进入不可再次调度的终态。
    fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Skipped
        )
    }
}

/// 创建工作流时接受的参数。
#[derive(Deserialize)]
struct CreateWorkflowArgs {
    name: String,
    #[serde(default = "default_max_parallelism")]
    max_parallelism: usize,
    #[serde(default)]
    failure_policy: FailurePolicy,
    #[serde(default)]
    sealed: bool,
    #[serde(default)]
    nodes: Vec<NodeInput>,
}

/// 新节点的稳定输入结构。
#[derive(Clone, Deserialize)]
struct NodeInput {
    id: String,
    prompt: String,
    #[serde(default)]
    dependencies: Vec<String>,
}

/// 向开放工作流追加节点时接受的参数。
#[derive(Deserialize)]
struct AddNodeArgs {
    workflow_id: String,
    node: NodeInput,
}

impl AgentPlugin for WorkflowPlugin {
    /// 注册工作流编排提示，使主 Agent 能在适合的任务中主动选择 DAG 工具。
    fn activate(&mut self, host: &dyn PluginHostApi, _context: ActivationContext) -> Result<()> {
        host.upsert_prompt(&PromptContribution {
            id: WORKFLOW_ORCHESTRATION_PROMPT_ID.into(),
            content: WORKFLOW_ORCHESTRATION_PROMPT.into(),
            priority: 110,
        })?;
        Ok(())
    }

    /// 删除本插件注册的工作流编排提示，避免插件卸载后继续影响模型决策。
    fn deactivate(&mut self, host: &dyn PluginHostApi) -> Result<()> {
        host.remove_prompt(WORKFLOW_ORCHESTRATION_PROMPT_ID)
    }

    /// 返回动态工作流控制面工具。
    fn list_tools(&self) -> Vec<ToolSpec> {
        vec![
            ToolSpec::new(
                "workflow_create",
                "创建一个动态 DAG 工作流。开放工作流可以继续追加节点；sealed=true 时创建后立即封存。",
                create_workflow_schema(),
            ),
            ToolSpec::new(
                "workflow_add_node",
                "向未封存的工作流追加节点。依赖必须引用该工作流中已经存在的节点。",
                json!({
                    "type": "object",
                    "properties": {
                        "workflow_id": workflow_id_property(),
                        "node": node_schema()
                    },
                    "required": ["workflow_id", "node"],
                    "additionalProperties": false
                }),
            ),
            workflow_id_tool(
                "workflow_seal",
                "封存工作流，禁止继续追加节点；全部节点进入终态后工作流才会完成。",
            ),
            workflow_id_tool(
                "workflow_tick",
                "推进工作流一次：同步运行结果、传播失败并启动并行度预算内的就绪节点。调用不会等待 Agent 完成。",
            ),
            workflow_id_tool("workflow_get", "读取工作流及全部节点的当前状态。"),
            workflow_id_tool(
                "workflow_cancel",
                "取消工作流中的运行节点并跳过尚未启动的节点。",
            ),
        ]
    }

    /// 执行一次短工作流控制操作，不在 Guest 内阻塞等待 Agent。
    fn call_tool_with_host(
        &mut self,
        host: &dyn PluginHostApi,
        call: ToolCall,
    ) -> Result<ToolResult> {
        let operation = call.name.clone();
        let content = match operation.as_str() {
            "workflow_create" => self.create_workflow(call.args.clone())?,
            "workflow_add_node" => self.add_node(call.args.clone())?,
            "workflow_seal" => self.seal_workflow(&required_workflow_id(&call.args)?)?,
            "workflow_tick" => self.tick_workflow(host, &required_workflow_id(&call.args)?)?,
            "workflow_get" => self.workflow_snapshot(&required_workflow_id(&call.args)?)?,
            "workflow_cancel" => self.cancel_workflow(host, &required_workflow_id(&call.args)?)?,
            _ => {
                return Ok(ToolResult::error(
                    call.id,
                    call.name,
                    format!("未知工作流工具：{operation}"),
                ));
            }
        };
        Ok(ToolResult::success(call.id, call.name, content))
    }
}

impl WorkflowPlugin {
    /// 校验定义并创建组件实例内唯一的工作流。
    fn create_workflow(&mut self, args: Value) -> Result<Value> {
        let args: CreateWorkflowArgs =
            serde_json::from_value(args).context("工作流创建参数无效")?;
        validate_name(&args.name)?;
        validate_parallelism(args.max_parallelism)?;
        validate_initial_nodes(&args.nodes)?;

        self.next_workflow_id = self
            .next_workflow_id
            .checked_add(1)
            .ok_or_else(|| anyhow!("工作流 ID 已耗尽"))?;
        let id = format!("workflow-{}", self.next_workflow_id);
        let nodes = args
            .nodes
            .into_iter()
            .map(|node| (node.id.clone(), WorkflowNode::from_input(node)))
            .collect();
        let workflow = Workflow {
            id: id.clone(),
            name: args.name,
            max_parallelism: args.max_parallelism,
            failure_policy: args.failure_policy,
            sealed: args.sealed,
            status: WorkflowStatus::Active,
            nodes,
        };
        self.workflows.insert(id.clone(), workflow);
        self.workflow_snapshot(&id)
    }

    /// 向开放工作流追加一个只依赖既有节点的新节点。
    fn add_node(&mut self, args: Value) -> Result<Value> {
        let args: AddNodeArgs = serde_json::from_value(args).context("工作流节点参数无效")?;
        validate_node(&args.node)?;
        let workflow = self.workflow_mut(&args.workflow_id)?;
        if workflow.status != WorkflowStatus::Active {
            return Err(anyhow!("工作流 `{}` 已进入终态", workflow.id));
        }
        if workflow.sealed {
            return Err(anyhow!("工作流 `{}` 已封存", workflow.id));
        }
        if workflow.nodes.contains_key(&args.node.id) {
            return Err(anyhow!("节点 `{}` 已存在", args.node.id));
        }
        for dependency in &args.node.dependencies {
            if !workflow.nodes.contains_key(dependency) {
                return Err(anyhow!(
                    "节点 `{}` 的依赖 `{dependency}` 不存在",
                    args.node.id
                ));
            }
        }
        workflow
            .nodes
            .insert(args.node.id.clone(), WorkflowNode::from_input(args.node));
        workflow_value(workflow)
    }

    /// 封存工作流，使其可以在节点收敛后进入整体终态。
    fn seal_workflow(&mut self, workflow_id: &str) -> Result<Value> {
        let workflow = self.workflow_mut(workflow_id)?;
        if workflow.status != WorkflowStatus::Active {
            return Err(anyhow!("工作流 `{workflow_id}` 已进入终态"));
        }
        workflow.sealed = true;
        settle_workflow_status(workflow);
        workflow_value(workflow)
    }

    /// 推进一步工作流，最多启动本轮并行度预算允许的就绪节点。
    fn tick_workflow(&mut self, host: &dyn PluginHostApi, workflow_id: &str) -> Result<Value> {
        let workflow = self.workflow_mut(workflow_id)?;
        if workflow.status != WorkflowStatus::Active {
            return workflow_value(workflow);
        }

        refresh_running_nodes(host, workflow)?;
        apply_failure_policy(host, workflow)?;
        skip_blocked_nodes(workflow);

        if workflow.status == WorkflowStatus::Active {
            spawn_ready_nodes(host, workflow);
            settle_workflow_status(workflow);
        }
        workflow_value(workflow)
    }

    /// 取消运行节点并把尚未调度的节点标记为跳过。
    fn cancel_workflow(&mut self, host: &dyn PluginHostApi, workflow_id: &str) -> Result<Value> {
        let workflow = self.workflow_mut(workflow_id)?;
        if workflow.status == WorkflowStatus::Active {
            for node in workflow.nodes.values_mut() {
                match node.status {
                    NodeStatus::Running => {
                        if let Some(agent_id) = &node.agent_id {
                            host.cancel_agent(agent_id)?;
                        }
                        node.status = NodeStatus::Cancelled;
                    }
                    NodeStatus::Pending => node.status = NodeStatus::Skipped,
                    _ => {}
                }
            }
            workflow.sealed = true;
            workflow.status = WorkflowStatus::Cancelled;
        }
        workflow_value(workflow)
    }

    /// 返回工作流的序列化快照。
    fn workflow_snapshot(&self, workflow_id: &str) -> Result<Value> {
        let workflow = self
            .workflows
            .get(workflow_id)
            .ok_or_else(|| anyhow!("工作流 `{workflow_id}` 不存在"))?;
        workflow_value(workflow)
    }

    /// 返回指定工作流的可变引用。
    fn workflow_mut(&mut self, workflow_id: &str) -> Result<&mut Workflow> {
        self.workflows
            .get_mut(workflow_id)
            .ok_or_else(|| anyhow!("工作流 `{workflow_id}` 不存在"))
    }
}

impl WorkflowNode {
    /// 从已校验输入构造等待调度的节点。
    fn from_input(input: NodeInput) -> Self {
        Self {
            id: input.id,
            prompt: input.prompt,
            dependencies: input.dependencies,
            status: NodeStatus::Pending,
            agent_id: None,
            output: None,
            error: None,
        }
    }
}

/// 同步所有运行节点的幂等终态结果。
fn refresh_running_nodes(host: &dyn PluginHostApi, workflow: &mut Workflow) -> Result<()> {
    let running = workflow
        .nodes
        .iter()
        .filter(|(_, node)| node.status == NodeStatus::Running)
        .map(|(id, node)| (id.clone(), node.agent_id.clone()))
        .collect::<Vec<_>>();

    for (node_id, agent_id) in running {
        let agent_id = agent_id.ok_or_else(|| anyhow!("运行节点 `{node_id}` 缺少 Agent ID"))?;
        let Some(outcome) = host.agent_result(&agent_id)? else {
            continue;
        };
        let node = workflow
            .nodes
            .get_mut(&node_id)
            .ok_or_else(|| anyhow!("节点 `{node_id}` 在结果同步时消失"))?;
        match outcome {
            AgentOutcome::Succeeded { result } => {
                node.status = NodeStatus::Succeeded;
                node.output = Some(result.final_text);
            }
            AgentOutcome::Failed { error } => {
                node.status = NodeStatus::Failed;
                node.error = Some(error);
            }
            AgentOutcome::Cancelled => node.status = NodeStatus::Cancelled,
        }
    }
    Ok(())
}

/// 应用首次失败后的全局停止策略。
fn apply_failure_policy(host: &dyn PluginHostApi, workflow: &mut Workflow) -> Result<()> {
    let has_failure = workflow
        .nodes
        .values()
        .any(|node| matches!(node.status, NodeStatus::Failed | NodeStatus::Cancelled));
    if !has_failure || !matches!(workflow.failure_policy, FailurePolicy::Stop) {
        return Ok(());
    }

    for node in workflow.nodes.values_mut() {
        match node.status {
            NodeStatus::Running => {
                if let Some(agent_id) = &node.agent_id {
                    host.cancel_agent(agent_id)?;
                }
                node.status = NodeStatus::Cancelled;
            }
            NodeStatus::Pending => node.status = NodeStatus::Skipped,
            _ => {}
        }
    }
    workflow.sealed = true;
    workflow.status = WorkflowStatus::Failed;
    Ok(())
}

/// 反复标记依赖非成功终态的节点，直到失败传播稳定。
fn skip_blocked_nodes(workflow: &mut Workflow) {
    loop {
        let blocked = workflow
            .nodes
            .iter()
            .filter(|(_, node)| node.status == NodeStatus::Pending)
            .filter(|(_, node)| {
                node.dependencies.iter().any(|dependency| {
                    workflow.nodes.get(dependency).is_some_and(|dependency| {
                        dependency.status.is_terminal()
                            && dependency.status != NodeStatus::Succeeded
                    })
                })
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        if blocked.is_empty() {
            break;
        }
        for id in blocked {
            if let Some(node) = workflow.nodes.get_mut(&id) {
                node.status = NodeStatus::Skipped;
                node.error = Some("依赖节点未成功完成".into());
            }
        }
    }
}

/// 启动当前并行度预算内全部就绪节点；单个派生失败只影响对应节点。
fn spawn_ready_nodes(host: &dyn PluginHostApi, workflow: &mut Workflow) {
    let running = workflow
        .nodes
        .values()
        .filter(|node| node.status == NodeStatus::Running)
        .count();
    let capacity = workflow.max_parallelism.saturating_sub(running);
    let ready = workflow
        .nodes
        .iter()
        .filter(|(_, node)| node.status == NodeStatus::Pending)
        .filter(|(_, node)| {
            node.dependencies.iter().all(|dependency| {
                workflow
                    .nodes
                    .get(dependency)
                    .is_some_and(|dependency| dependency.status == NodeStatus::Succeeded)
            })
        })
        .map(|(id, _)| id.clone())
        .take(capacity)
        .collect::<Vec<_>>();

    for node_id in ready {
        let input = build_node_input(workflow, &node_id);
        let node = workflow
            .nodes
            .get_mut(&node_id)
            .expect("就绪节点必须仍存在");
        match host.spawn_agent(&AgentSpawnRequest::new(WORKER_PROFILE, input)) {
            Ok(handle) => {
                node.status = NodeStatus::Running;
                node.agent_id = Some(handle.id);
            }
            Err(error) => {
                node.status = NodeStatus::Failed;
                node.error = Some(format!("启动 Agent 失败：{error}"));
            }
        }
    }
}

/// 合并节点提示词与其直接依赖的可信终态输出。
fn build_node_input(workflow: &Workflow, node_id: &str) -> String {
    let node = workflow.nodes.get(node_id).expect("待启动节点必须存在");
    if node.dependencies.is_empty() {
        return node.prompt.clone();
    }
    let outputs = node
        .dependencies
        .iter()
        .filter_map(|dependency| {
            workflow
                .nodes
                .get(dependency)
                .and_then(|node| node.output.as_ref())
                .map(|output| (dependency, output))
        })
        .collect::<BTreeMap<_, _>>();
    format!(
        "{}\n\n依赖节点结果：\n{}",
        node.prompt,
        serde_json::to_string_pretty(&outputs).expect("依赖输出必须可序列化")
    )
}

/// 在封存且所有节点终止时计算工作流最终状态。
fn settle_workflow_status(workflow: &mut Workflow) {
    if !workflow.sealed
        || workflow
            .nodes
            .values()
            .any(|node| !node.status.is_terminal())
    {
        return;
    }
    workflow.status = if workflow
        .nodes
        .values()
        .all(|node| node.status == NodeStatus::Succeeded)
    {
        WorkflowStatus::Succeeded
    } else {
        WorkflowStatus::Failed
    };
}

/// 校验工作流名称。
fn validate_name(name: &str) -> Result<()> {
    if name.trim().is_empty() || name.len() > 256 {
        return Err(anyhow!("工作流名称必须为 1 到 256 字节的非空字符串"));
    }
    Ok(())
}

/// 校验并行度在插件声明的结构限制内。
fn validate_parallelism(max_parallelism: usize) -> Result<()> {
    if !(1..=MAX_PARALLELISM).contains(&max_parallelism) {
        return Err(anyhow!(
            "max_parallelism 必须位于 1 到 {MAX_PARALLELISM} 之间"
        ));
    }
    Ok(())
}

/// 校验节点标识、提示词和依赖去重。
fn validate_node(node: &NodeInput) -> Result<()> {
    if node.id.trim().is_empty()
        || node.id.len() > 128
        || !node
            .id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(anyhow!("节点 ID `{}` 格式无效", node.id));
    }
    if node.prompt.trim().is_empty() || node.prompt.len() > MAX_PROMPT_BYTES {
        return Err(anyhow!(
            "节点 `{}` 的提示词必须为 1 到 {MAX_PROMPT_BYTES} 字节",
            node.id
        ));
    }
    let unique_dependencies = node.dependencies.iter().collect::<BTreeSet<_>>();
    if unique_dependencies.len() != node.dependencies.len() {
        return Err(anyhow!("节点 `{}` 包含重复依赖", node.id));
    }
    if node
        .dependencies
        .iter()
        .any(|dependency| dependency == &node.id)
    {
        return Err(anyhow!("节点 `{}` 不能依赖自身", node.id));
    }
    Ok(())
}

/// 校验初始节点集合的引用完整性和无环性。
fn validate_initial_nodes(nodes: &[NodeInput]) -> Result<()> {
    let mut by_id = BTreeMap::new();
    for node in nodes {
        validate_node(node)?;
        if by_id.insert(node.id.as_str(), node).is_some() {
            return Err(anyhow!("节点 `{}` 重复", node.id));
        }
    }
    for node in nodes {
        for dependency in &node.dependencies {
            if !by_id.contains_key(dependency.as_str()) {
                return Err(anyhow!("节点 `{}` 的依赖 `{dependency}` 不存在", node.id));
            }
        }
    }

    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();
    for node in nodes {
        visit_node(node.id.as_str(), &by_id, &mut visiting, &mut visited)?;
    }
    Ok(())
}

/// 深度优先检查初始 DAG 是否包含依赖环。
fn visit_node<'a>(
    node_id: &'a str,
    nodes: &BTreeMap<&'a str, &'a NodeInput>,
    visiting: &mut BTreeSet<&'a str>,
    visited: &mut BTreeSet<&'a str>,
) -> Result<()> {
    if visited.contains(node_id) {
        return Ok(());
    }
    if !visiting.insert(node_id) {
        return Err(anyhow!("工作流包含涉及节点 `{node_id}` 的依赖环"));
    }
    let node = nodes
        .get(node_id)
        .ok_or_else(|| anyhow!("节点 `{node_id}` 不存在"))?;
    for dependency in &node.dependencies {
        visit_node(dependency, nodes, visiting, visited)?;
    }
    visiting.remove(node_id);
    visited.insert(node_id);
    Ok(())
}

/// 默认允许四个工作流节点并行运行。
fn default_max_parallelism() -> usize {
    4
}

/// 从工具参数中读取工作流标识。
fn required_workflow_id(args: &Value) -> Result<String> {
    let workflow_id = args
        .get("workflow_id")
        .and_then(Value::as_str)
        .context("参数 `workflow_id` 必须是字符串")?;
    if workflow_id.trim().is_empty() {
        return Err(anyhow!("参数 `workflow_id` 不能为空"));
    }
    Ok(workflow_id.to_owned())
}

/// 把工作流转换为稳定 JSON 快照。
fn workflow_value(workflow: &Workflow) -> Result<Value> {
    serde_json::to_value(workflow).context("序列化工作流状态失败")
}

/// 创建只接受工作流标识的工具定义。
fn workflow_id_tool(name: &str, description: &str) -> ToolSpec {
    ToolSpec::new(
        name,
        description,
        json!({
            "type": "object",
            "properties": {"workflow_id": workflow_id_property()},
            "required": ["workflow_id"],
            "additionalProperties": false
        }),
    )
}

/// 返回工作流标识的 JSON Schema 属性。
fn workflow_id_property() -> Value {
    json!({
        "type": "string",
        "description": "workflow_create 返回的不透明工作流 ID。",
        "minLength": 1
    })
}

/// 返回节点输入的 JSON Schema。
fn node_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": {
                "type": "string",
                "pattern": "^[A-Za-z0-9._-]{1,128}$"
            },
            "prompt": {"type": "string", "minLength": 1, "maxLength": MAX_PROMPT_BYTES},
            "dependencies": {
                "type": "array",
                "items": {"type": "string"},
                "uniqueItems": true,
                "default": []
            }
        },
        "required": ["id", "prompt"],
        "additionalProperties": false
    })
}

/// 返回创建工作流工具的 JSON Schema。
fn create_workflow_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": {"type": "string", "minLength": 1, "maxLength": 256},
            "max_parallelism": {
                "type": "integer",
                "minimum": 1,
                "maximum": MAX_PARALLELISM,
                "default": default_max_parallelism()
            },
            "failure_policy": {
                "type": "string",
                "enum": ["stop", "continue"],
                "default": "stop"
            },
            "sealed": {"type": "boolean", "default": false},
            "nodes": {"type": "array", "items": node_schema(), "default": []}
        },
        "required": ["name"],
        "additionalProperties": false
    })
}

export_plugin!(WorkflowPlugin);

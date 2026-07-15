//! 基于 Agent Runtime 的动态 DAG 工作流插件。
//!
//! 插件拥有工作流定义、依赖调度和失败传播；Host 只提供受限 Agent 派生、观察与取消。

use agent_plugin::{
    export_plugin, ActivationContext, AgentId, AgentOutcome, AgentPlugin, AgentSpawnRequest,
    AgentViewSession, PluginHostApi, PromptContribution, Result, ToolCall, ToolResult, ToolSpec,
    UiColor, UiDeclaration, UiFrame, UiInput, UiInputEvent, UiLine, UiNavigationAction,
    UiNavigationRequest, UiPlacement, UiRenderRequest, UiSize, UiSpan, UiStyle, UiViewInstance,
};
use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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
/// 输入框上方展示活动工作流进度的紧凑视图。
const WORKFLOW_SHELF_VIEW: &str = "workflow-shelf";
/// 展示单个 DAG 及节点详情的工作台子视图。
const WORKFLOW_WORKSPACE_VIEW: &str = "workflow-workspace";
/// 展示并交互单个工作流节点 Agent 的子视图。
const WORKFLOW_NODE_VIEW: &str = "workflow-node-agent";

/// 保存当前组件实例内所有动态工作流。
#[derive(Default)]
struct WorkflowPlugin {
    workflows: BTreeMap<String, Workflow>,
    next_workflow_id: u64,
    selected_workflow: Option<String>,
    selected_node: usize,
    navigation_sequence: u64,
    node_sessions: BTreeMap<String, AgentViewSession>,
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

    /// 声明活动工作流摘要、DAG 工作台和节点 Agent 主界面。
    fn describe_ui(&self) -> Vec<UiDeclaration> {
        vec![
            UiDeclaration {
                plugin_id: String::new(),
                view_id: WORKFLOW_SHELF_VIEW.into(),
                title: "工作流".into(),
                placement: UiPlacement::ComposerShelf,
                size: UiSize {
                    width: None,
                    height: Some(3),
                },
                focusable: true,
                input_triggers: Vec::new(),
            },
            UiDeclaration {
                plugin_id: String::new(),
                view_id: WORKFLOW_WORKSPACE_VIEW.into(),
                title: "工作流".into(),
                placement: UiPlacement::Subview,
                size: UiSize::default(),
                focusable: true,
                input_triggers: Vec::new(),
            },
            UiDeclaration {
                plugin_id: String::new(),
                view_id: WORKFLOW_NODE_VIEW.into(),
                title: "节点 Agent".into(),
                placement: UiPlacement::Subview,
                size: UiSize::default(),
                focusable: true,
                input_triggers: Vec::new(),
            },
        ]
    }

    /// 根据 Host 分配的尺寸渲染工作流摘要、DAG 或节点 Agent 主界面。
    fn render_ui(&mut self, request: UiRenderRequest) -> Option<UiFrame> {
        let lines = match request.view_id.as_str() {
            WORKFLOW_SHELF_VIEW => self.render_workflow_shelf(&request),
            WORKFLOW_WORKSPACE_VIEW => self.render_workflow_workspace(&request),
            WORKFLOW_NODE_VIEW => self.render_node_agent(&request),
            _ => return None,
        };
        let visible = request.view_id != WORKFLOW_SHELF_VIEW
            || self
                .active_workflow_id()
                .and_then(|id| self.workflows.get(id))
                .is_some();
        Some(UiFrame {
            view_id: request.view_id,
            visible,
            lines,
            cursor: None,
        })
    }

    /// 节点子视图渲染前刷新对应 Agent 的实时状态和增量事件。
    fn render_ui_with_host(
        &mut self,
        host: &dyn PluginHostApi,
        request: UiRenderRequest,
    ) -> Option<UiFrame> {
        if request.view_id == WORKFLOW_NODE_VIEW {
            if let Some(instance_id) = request.instance_id.as_deref() {
                self.refresh_node_session(host, instance_id);
            }
        }
        self.render_ui(request)
    }

    /// 处理摘要入口、DAG 节点选择、显式刷新和节点 Agent 消息输入。
    fn on_ui_input_with_host(&mut self, host: &dyn PluginHostApi, input: UiInput) {
        match input.event {
            UiInputEvent::Key { code, .. }
                if input.view_id == WORKFLOW_SHELF_VIEW && code == "enter" =>
            {
                self.open_active_workflow(host);
            }
            UiInputEvent::Mouse { kind, .. }
                if input.view_id == WORKFLOW_SHELF_VIEW && kind.starts_with("down_") =>
            {
                self.open_active_workflow(host);
            }
            UiInputEvent::Key { code, .. } if input.view_id == WORKFLOW_WORKSPACE_VIEW => {
                let Some(workflow_id) = input.instance_id.as_deref() else {
                    return;
                };
                let node_count = self
                    .workflows
                    .get(workflow_id)
                    .map_or(0, |workflow| workflow.nodes.len());
                match code.as_str() {
                    "up" => self.selected_node = self.selected_node.saturating_sub(1),
                    "down" => {
                        self.selected_node =
                            (self.selected_node + 1).min(node_count.saturating_sub(1));
                    }
                    "r" => {
                        let _ = self.tick_workflow(host, workflow_id);
                    }
                    "enter" => self.open_selected_node(host, workflow_id),
                    _ => {}
                }
            }
            UiInputEvent::Key { code, modifiers } if input.view_id == WORKFLOW_NODE_VIEW => {
                let Some(instance_id) = input.instance_id.as_deref() else {
                    return;
                };
                let event = UiInputEvent::Key { code, modifiers };
                let continued = self
                    .node_sessions
                    .get_mut(instance_id)
                    .and_then(|session| session.handle_input(host, &event));
                if let Some(handle) = continued {
                    self.replace_node_agent(instance_id, handle.id);
                }
            }
            _ => {}
        }
    }
}

impl WorkflowPlugin {
    /// 返回优先选中的活动工作流；选中项已结束时回退到最近创建的活动项。
    fn active_workflow_id(&self) -> Option<&str> {
        self.selected_workflow
            .as_deref()
            .filter(|id| {
                self.workflows
                    .get(*id)
                    .is_some_and(|workflow| workflow.status == WorkflowStatus::Active)
            })
            .or_else(|| {
                self.workflows
                    .iter()
                    .rev()
                    .find(|(_, workflow)| workflow.status == WorkflowStatus::Active)
                    .map(|(id, _)| id.as_str())
            })
    }

    /// 渲染输入框上方的工作流进度摘要。
    fn render_workflow_shelf(&self, request: &UiRenderRequest) -> Vec<UiLine> {
        let Some(workflow) = self
            .active_workflow_id()
            .and_then(|id| self.workflows.get(id))
        else {
            return Vec::new();
        };
        let completed = workflow
            .nodes
            .values()
            .filter(|node| node.status == NodeStatus::Succeeded)
            .count();
        let running = workflow
            .nodes
            .values()
            .filter(|node| node.status == NodeStatus::Running)
            .count();
        let failed = workflow
            .nodes
            .values()
            .filter(|node| matches!(node.status, NodeStatus::Failed | NodeStatus::Cancelled))
            .count();
        vec![
            ui_text_line(
                &clip(
                    &format!(
                        "工作流  {}  {completed}/{}",
                        workflow.name,
                        workflow.nodes.len()
                    ),
                    usize::from(request.width),
                ),
                Some(UiColor::Cyan),
                true,
            ),
            ui_text_line(
                &clip(
                    &format!(
                        "运行 {running}  等待 {}  失败 {failed}",
                        workflow
                            .nodes
                            .values()
                            .filter(|node| node.status == NodeStatus::Pending)
                            .count()
                    ),
                    usize::from(request.width),
                ),
                Some(if failed > 0 {
                    UiColor::Red
                } else {
                    UiColor::Gray
                }),
                false,
            ),
            ui_text_line("Enter 查看 DAG", Some(UiColor::Gray), false),
        ]
    }

    /// 渲染单个工作流的 DAG 节点列表和当前选中节点详情。
    fn render_workflow_workspace(&self, request: &UiRenderRequest) -> Vec<UiLine> {
        let Some(workflow_id) = request.instance_id.as_deref() else {
            return vec![ui_text_line(
                "工作流视图缺少实例 ID",
                Some(UiColor::Red),
                false,
            )];
        };
        let Some(workflow) = self.workflows.get(workflow_id) else {
            return vec![ui_text_line("工作流不存在", Some(UiColor::Red), false)];
        };
        let width = usize::from(request.width);
        let completed = workflow
            .nodes
            .values()
            .filter(|node| node.status == NodeStatus::Succeeded)
            .count();
        let mut lines = vec![
            ui_text_line(
                &clip(
                    &format!(
                        "{}  {}  {completed}/{}",
                        workflow.name,
                        workflow_status_label(workflow.status),
                        workflow.nodes.len()
                    ),
                    width,
                ),
                Some(workflow_status_color(workflow.status)),
                true,
            ),
            ui_text_line(
                &clip(
                    &format!(
                        "并行度 {}  失败策略 {}  {}",
                        workflow.max_parallelism,
                        failure_policy_label(workflow.failure_policy),
                        if workflow.sealed {
                            "已封存"
                        } else {
                            "可编辑"
                        }
                    ),
                    width,
                ),
                Some(UiColor::Gray),
                false,
            ),
            UiLine { spans: Vec::new() },
        ];
        let selected = self
            .selected_node
            .min(workflow.nodes.len().saturating_sub(1));
        for (index, node) in workflow.nodes.values().enumerate() {
            let dependencies = if node.dependencies.is_empty() {
                "起点".to_string()
            } else {
                format!("依赖 {}", node.dependencies.join(", "))
            };
            lines.push(ui_text_line(
                &clip(
                    &format!(
                        "{} {} {}  {}",
                        if index == selected { ">" } else { " " },
                        node_status_marker(node.status),
                        node.id,
                        dependencies
                    ),
                    width,
                ),
                Some(node_status_color(node.status)),
                index == selected,
            ));
        }
        if let Some(node) = workflow.nodes.values().nth(selected) {
            lines.push(UiLine { spans: Vec::new() });
            lines.push(ui_text_line(
                &clip(&format!("节点  {}", node.id), width),
                Some(UiColor::Cyan),
                true,
            ));
            lines.push(ui_text_line(
                &clip(&node.prompt.replace(['\n', '\r'], " "), width),
                None,
                false,
            ));
            lines.push(ui_text_line(
                if node.agent_id.is_some() {
                    "Enter 进入节点 Agent  ·  r 推进工作流"
                } else {
                    "r 推进工作流"
                },
                Some(UiColor::Gray),
                false,
            ));
        }
        lines.truncate(usize::from(request.height));
        lines
    }

    /// 渲染节点上下文，并把剩余区域交给共享 Agent 主界面。
    fn render_node_agent(&self, request: &UiRenderRequest) -> Vec<UiLine> {
        let Some(instance_id) = request.instance_id.as_deref() else {
            return vec![ui_text_line(
                "节点视图缺少实例 ID",
                Some(UiColor::Red),
                false,
            )];
        };
        let Some((workflow_id, node_id)) = split_node_instance(instance_id) else {
            return vec![ui_text_line(
                "节点视图实例 ID 无效",
                Some(UiColor::Red),
                false,
            )];
        };
        let Some(workflow) = self.workflows.get(workflow_id) else {
            return vec![ui_text_line("工作流不存在", Some(UiColor::Red), false)];
        };
        let Some(node) = workflow.nodes.get(node_id) else {
            return vec![ui_text_line("工作流节点不存在", Some(UiColor::Red), false)];
        };
        let mut lines = vec![
            ui_text_line(
                &clip(
                    &format!(
                        "{} / {}  {}",
                        workflow.name,
                        node.id,
                        node_status_label(node.status)
                    ),
                    usize::from(request.width),
                ),
                Some(node_status_color(node.status)),
                true,
            ),
            ui_text_line(
                &clip(
                    &node.prompt.replace(['\n', '\r'], " "),
                    usize::from(request.width),
                ),
                Some(UiColor::Gray),
                false,
            ),
            UiLine { spans: Vec::new() },
        ];
        if let Some(session) = self.node_sessions.get(instance_id) {
            lines.extend(session.render(request.width, request.height.saturating_sub(3)));
        } else {
            lines.push(ui_text_line(
                if node.agent_id.is_some() {
                    "正在连接节点 Agent"
                } else {
                    "节点尚未启动"
                },
                Some(UiColor::Gray),
                false,
            ));
        }
        lines.truncate(usize::from(request.height));
        lines
    }

    /// 刷新节点对应的共享 Agent 主界面，不推进其他工作流节点。
    fn refresh_node_session(&mut self, host: &dyn PluginHostApi, instance_id: &str) {
        let Some((workflow_id, node_id)) = split_node_instance(instance_id) else {
            return;
        };
        let Some(target) = self
            .workflows
            .get(workflow_id)
            .and_then(|workflow| workflow.nodes.get(node_id))
            .and_then(|node| node.agent_id.clone())
        else {
            return;
        };
        let session = self
            .node_sessions
            .entry(instance_id.to_string())
            .or_insert_with(|| AgentViewSession::new(target.clone()));
        session.replace_target(target);
        session.refresh(host);
    }

    /// 打开当前活动工作流的 DAG 工作台。
    fn open_active_workflow(&mut self, host: &dyn PluginHostApi) {
        let Some(workflow_id) = self.active_workflow_id().map(str::to_string) else {
            return;
        };
        let title = self
            .workflows
            .get(&workflow_id)
            .map(|workflow| workflow.name.clone());
        self.selected_workflow = Some(workflow_id.clone());
        self.selected_node = 0;
        self.navigation_sequence = self.navigation_sequence.saturating_add(1);
        let _ = host.navigate_view(UiNavigationRequest {
            request_id: format!("workflow-open-{}", self.navigation_sequence),
            action: UiNavigationAction::Push {
                view: UiViewInstance {
                    view_id: WORKFLOW_WORKSPACE_VIEW.into(),
                    instance_id: workflow_id,
                    title,
                },
            },
        });
    }

    /// 打开 DAG 工作台中当前选中且已启动的节点 Agent。
    fn open_selected_node(&mut self, host: &dyn PluginHostApi, workflow_id: &str) {
        let Some((node_id, target)) = self.workflows.get(workflow_id).and_then(|workflow| {
            workflow
                .nodes
                .values()
                .nth(
                    self.selected_node
                        .min(workflow.nodes.len().saturating_sub(1)),
                )
                .and_then(|node| {
                    node.agent_id
                        .clone()
                        .map(|target| (node.id.clone(), target))
                })
        }) else {
            return;
        };
        let instance_id = node_instance_id(workflow_id, &node_id);
        self.node_sessions
            .entry(instance_id.clone())
            .or_insert_with(|| AgentViewSession::new(target));
        self.navigation_sequence = self.navigation_sequence.saturating_add(1);
        let _ = host.navigate_view(UiNavigationRequest {
            request_id: format!("workflow-node-{}", self.navigation_sequence),
            action: UiNavigationAction::Push {
                view: UiViewInstance {
                    view_id: WORKFLOW_NODE_VIEW.into(),
                    instance_id,
                    title: Some(node_id),
                },
            },
        });
    }

    /// 用成功会话续跑产生的新句柄替换节点的当前 Agent 映射。
    fn replace_node_agent(&mut self, instance_id: &str, agent_id: AgentId) {
        let Some((workflow_id, node_id)) = split_node_instance(instance_id) else {
            return;
        };
        if let Some(node) = self
            .workflows
            .get_mut(workflow_id)
            .and_then(|workflow| workflow.nodes.get_mut(node_id))
        {
            node.agent_id = Some(agent_id);
            node.status = NodeStatus::Running;
            node.output = None;
            node.error = None;
        }
    }

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
        self.selected_workflow = Some(id.clone());
        self.selected_node = 0;
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

/// 返回工作流整体状态的紧凑中文标签。
fn workflow_status_label(status: WorkflowStatus) -> &'static str {
    match status {
        WorkflowStatus::Active => "执行中",
        WorkflowStatus::Succeeded => "已完成",
        WorkflowStatus::Failed => "失败",
        WorkflowStatus::Cancelled => "已取消",
    }
}

/// 返回工作流整体状态的终端颜色。
fn workflow_status_color(status: WorkflowStatus) -> UiColor {
    match status {
        WorkflowStatus::Active => UiColor::Cyan,
        WorkflowStatus::Succeeded => UiColor::Green,
        WorkflowStatus::Failed => UiColor::Red,
        WorkflowStatus::Cancelled => UiColor::Gray,
    }
}

/// 返回失败传播策略的中文标签。
fn failure_policy_label(policy: FailurePolicy) -> &'static str {
    match policy {
        FailurePolicy::Stop => "失败即停止",
        FailurePolicy::Continue => "独立分支继续",
    }
}

/// 返回节点状态的固定宽度标记。
fn node_status_marker(status: NodeStatus) -> &'static str {
    match status {
        NodeStatus::Pending => "[ ]",
        NodeStatus::Running => "[>]",
        NodeStatus::Succeeded => "[x]",
        NodeStatus::Failed => "[!]",
        NodeStatus::Cancelled => "[-]",
        NodeStatus::Skipped => "[~]",
    }
}

/// 返回节点状态的紧凑中文标签。
fn node_status_label(status: NodeStatus) -> &'static str {
    match status {
        NodeStatus::Pending => "等待",
        NodeStatus::Running => "运行中",
        NodeStatus::Succeeded => "已完成",
        NodeStatus::Failed => "失败",
        NodeStatus::Cancelled => "已取消",
        NodeStatus::Skipped => "已跳过",
    }
}

/// 返回节点状态的终端颜色。
fn node_status_color(status: NodeStatus) -> UiColor {
    match status {
        NodeStatus::Pending | NodeStatus::Skipped | NodeStatus::Cancelled => UiColor::Gray,
        NodeStatus::Running => UiColor::Yellow,
        NodeStatus::Succeeded => UiColor::Green,
        NodeStatus::Failed => UiColor::Red,
    }
}

/// 构造带单一样式的协议无关终端行。
fn ui_text_line(text: &str, foreground: Option<UiColor>, bold: bool) -> UiLine {
    UiLine {
        spans: vec![UiSpan {
            text: text.to_string(),
            style: UiStyle {
                foreground,
                bold,
                ..UiStyle::default()
            },
        }],
    }
}

/// 按终端显示宽度截断文本，避免中文内容越过 Host 分配区域。
fn clip(text: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_string();
    }
    if max_width <= 3 {
        return ".".repeat(max_width);
    }
    let content_width = max_width - 3;
    let mut width = 0usize;
    let mut result = String::new();
    for character in text.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if width + character_width > content_width {
            break;
        }
        result.push(character);
        width += character_width;
    }
    result.push_str("...");
    result
}

/// 生成可逆的节点子视图实例 ID；节点 ID 不允许包含斜杠。
fn node_instance_id(workflow_id: &str, node_id: &str) -> String {
    format!("{workflow_id}/{node_id}")
}

/// 从节点子视图实例 ID 中恢复工作流和节点 ID。
fn split_node_instance(instance_id: &str) -> Option<(&str, &str)> {
    instance_id
        .split_once('/')
        .filter(|(workflow_id, node_id)| !workflow_id.is_empty() && !node_id.is_empty())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造固定尺寸的插件 UI 渲染请求。
    fn request(view_id: &str, instance_id: Option<&str>) -> UiRenderRequest {
        UiRenderRequest {
            plugin_id: "workflow".into(),
            view_id: view_id.into(),
            instance_id: instance_id.map(str::to_string),
            width: 80,
            height: 24,
            focused: false,
            frame: 1,
        }
    }

    /// 将协议行压成便于断言的纯文本。
    fn frame_text(frame: &UiFrame) -> String {
        frame
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 工作流插件必须把摘要放在输入区上方，并把 DAG 与 Agent 放进子视图。
    #[test]
    fn declares_shelf_workspace_and_node_views() {
        let declarations = WorkflowPlugin::default().describe_ui();

        assert_eq!(declarations.len(), 3);
        assert!(matches!(
            declarations[0].placement,
            UiPlacement::ComposerShelf
        ));
        assert!(matches!(declarations[1].placement, UiPlacement::Subview));
        assert!(matches!(declarations[2].placement, UiPlacement::Subview));
    }

    /// 创建 DAG 后摘要应显示进度，工作台应显示节点和依赖关系。
    #[test]
    fn renders_workflow_summary_and_dag() {
        let mut plugin = WorkflowPlugin::default();
        plugin
            .create_workflow(json!({
                "name": "发布检查",
                "max_parallelism": 2,
                "nodes": [
                    {"id": "build", "prompt": "构建产物"},
                    {"id": "review", "prompt": "复核产物", "dependencies": ["build"]}
                ]
            }))
            .expect("工作流应创建成功");

        let shelf = plugin
            .render_ui(request(WORKFLOW_SHELF_VIEW, None))
            .expect("摘要应返回帧");
        assert!(shelf.visible);
        assert!(frame_text(&shelf).contains("发布检查"));

        let workspace = plugin
            .render_ui(request(WORKFLOW_WORKSPACE_VIEW, Some("workflow-1")))
            .expect("工作台应返回帧");
        let text = frame_text(&workspace);
        assert!(text.contains("build"), "{text}");
        assert!(text.contains("依赖 build"), "{text}");
    }

    /// 节点实例 ID 必须无损保留工作流和节点身份。
    #[test]
    fn node_instance_id_round_trips() {
        let instance_id = node_instance_id("workflow-3", "review.final");

        assert_eq!(
            split_node_instance(&instance_id),
            Some(("workflow-3", "review.final"))
        );
    }
}

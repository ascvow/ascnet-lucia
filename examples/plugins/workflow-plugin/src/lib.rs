//! 参考 Claude Code 动态任务列表的工作流插件。
//!
//! 插件维护单一动态任务列表：任务可随时追加、修改、取消或删除；依赖齐备的任务
//! 自动派生受限 worker Agent 执行并注入依赖输出，无需封存或手动推进。
//! Host 只提供受限 Agent 派生、观察与取消。

use agent_plugin::{
    export_plugin, ActivationContext, AgentId, AgentOutcome, AgentPlugin, AgentSpawnRequest,
    AgentViewSession, PluginHostApi, PromptContribution, Result, ToolCall, ToolResult, ToolSpec,
    UiColor, UiDeclaration, UiFrame, UiInput, UiInputEvent, UiLine, UiNavigationAction,
    UiNavigationRequest, UiPlacement, UiRenderRequest, UiSize, UiSpan, UiStyle, UiViewInstance,
};
use anyhow::{anyhow, Context};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// manifest 允许任务使用的唯一 Agent 派生策略。
const WORKER_PROFILE: &str = "worker";
/// 同时运行的任务 Agent 上限；超出预算的就绪任务排队等待自动调度。
const MAX_PARALLELISM: usize = 4;
/// 任务提示词的最大字节数，避免状态和跨 ABI 请求无限增长。
const MAX_PROMPT_BYTES: usize = 32 * 1024;
/// 引导主 Agent 合理使用动态任务列表的 developer 提示 ID。
const TASK_ORCHESTRATION_PROMPT_ID: &str = "workflow-orchestration";
/// 引导主 Agent 以动态任务列表编排多阶段工作的规则。
const TASK_ORCHESTRATION_PROMPT: &str = "包含多个阶段或明确依赖关系的任务，用 task_create 维护动态任务列表：先登记当前已知的任务与依赖，执行中随时追加新任务、修改待处理任务或取消不再需要的任务。依赖齐备的任务会自动派生 worker Agent 并注入依赖任务结果，无需手动推进；用 task_list 观察进度，用 task_get 读取任务输出，失败任务用 task_update 重置为 pending 即可重跑。简单的一次性任务不要创建任务列表。";
/// 输入框上方展示任务进度的紧凑视图。
const TASK_SHELF_VIEW: &str = "workflow-shelf";
/// 展示任务列表及选中任务详情的工作台子视图。
const TASK_WORKSPACE_VIEW: &str = "workflow-workspace";
/// 展示并交互单个任务 Agent 的子视图。
const TASK_AGENT_VIEW: &str = "workflow-task-agent";
/// 任务工作台的固定视图实例 ID；插件只维护一个任务列表。
const TASK_WORKSPACE_INSTANCE: &str = "tasks";

/// 维护单一动态任务列表和任务 Agent 会话。
#[derive(Default)]
struct WorkflowPlugin {
    tasks: BTreeMap<String, Task>,
    /// 任务创建顺序，用于展示和公平调度。
    order: Vec<String>,
    selected_task: usize,
    navigation_sequence: u64,
    task_sessions: BTreeMap<String, AgentViewSession>,
}

/// 任务列表中的一个 Agent 任务。
struct Task {
    id: String,
    prompt: String,
    depends_on: Vec<String>,
    status: TaskStatus,
    agent_id: Option<AgentId>,
    output: Option<String>,
    error: Option<String>,
}

/// 任务的调度状态；completed、failed、cancelled 为终态。
#[derive(Clone, Copy, PartialEq, Eq)]
enum TaskStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
    Cancelled,
}

/// task_create 接受的批量任务参数。
#[derive(Deserialize)]
struct CreateTasksArgs {
    tasks: Vec<TaskInput>,
}

/// 新任务的稳定输入结构。
#[derive(Deserialize)]
struct TaskInput {
    id: String,
    prompt: String,
    #[serde(default)]
    depends_on: Vec<String>,
}

/// task_update 接受的参数；除 id 外至少需要一个更新字段。
#[derive(Deserialize)]
struct UpdateTaskArgs {
    id: String,
    status: Option<UpdateStatus>,
    prompt: Option<String>,
    depends_on: Option<Vec<String>>,
    #[serde(default)]
    delete: bool,
}

/// task_update 允许写入的目标状态。
#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum UpdateStatus {
    /// 把失败或已取消的任务重置为待处理，以便自动重跑。
    Pending,
    /// 取消待处理或运行中的任务。
    Cancelled,
}

impl AgentPlugin for WorkflowPlugin {
    /// 注册任务编排提示，使主 Agent 能在多阶段任务中主动维护任务列表。
    fn activate(&mut self, host: &dyn PluginHostApi, _context: ActivationContext) -> Result<()> {
        host.upsert_prompt(&PromptContribution {
            id: TASK_ORCHESTRATION_PROMPT_ID.into(),
            content: TASK_ORCHESTRATION_PROMPT.into(),
            priority: 110,
        })?;
        Ok(())
    }

    /// 删除本插件注册的任务编排提示，避免插件卸载后继续影响模型决策。
    fn deactivate(&mut self, host: &dyn PluginHostApi) -> Result<()> {
        host.remove_prompt(TASK_ORCHESTRATION_PROMPT_ID)
    }

    /// 返回动态任务列表控制面工具。
    fn list_tools(&self) -> Vec<ToolSpec> {
        vec![
            ToolSpec::new(
                "task_create",
                "创建一个或多个任务加入动态任务列表。依赖可以引用已有任务或同批任务；依赖齐备的任务会立即自动派生 worker Agent 执行，无需手动推进。",
                create_tasks_schema(),
            ),
            ToolSpec::new(
                "task_update",
                "更新任务：把失败或已取消的任务重置为 pending 重跑、取消任务、修改待处理任务的提示词或依赖，或删除无人依赖的任务。",
                update_task_schema(),
            ),
            ToolSpec::new(
                "task_list",
                "同步运行结果后返回全部任务的状态与阻塞依赖；任务输出通过 task_get 读取。",
                ToolSpec::empty_object_schema(),
            ),
            ToolSpec::new(
                "task_get",
                "同步运行结果后读取单个任务的完整详情，包括执行输出与错误。",
                task_get_schema(),
            ),
        ]
    }

    /// 执行一次短任务控制操作；每次调用都会同步结果并自动调度，不在 Guest 内阻塞等待 Agent。
    fn call_tool_with_host(
        &mut self,
        host: &dyn PluginHostApi,
        call: ToolCall,
    ) -> Result<ToolResult> {
        let operation = call.name.clone();
        let content = match operation.as_str() {
            "task_create" => self.create_tasks(host, call.args.clone())?,
            "task_update" => self.update_task(host, call.args.clone())?,
            "task_list" => {
                self.sync_and_schedule(host)?;
                self.list_snapshot()
            }
            "task_get" => self.get_task(host, &call.args)?,
            _ => {
                return Ok(ToolResult::error(
                    call.id,
                    call.name,
                    format!("未知任务工具：{operation}"),
                ));
            }
        };
        Ok(ToolResult::success(call.id, call.name, content))
    }

    /// 声明任务进度摘要、任务列表工作台和任务 Agent 主界面。
    fn describe_ui(&self) -> Vec<UiDeclaration> {
        vec![
            UiDeclaration {
                plugin_id: String::new(),
                view_id: TASK_SHELF_VIEW.into(),
                title: "任务".into(),
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
                view_id: TASK_WORKSPACE_VIEW.into(),
                title: "任务列表".into(),
                placement: UiPlacement::Subview,
                size: UiSize::default(),
                focusable: true,
                input_triggers: Vec::new(),
            },
            UiDeclaration {
                plugin_id: String::new(),
                view_id: TASK_AGENT_VIEW.into(),
                title: "任务 Agent".into(),
                placement: UiPlacement::Subview,
                size: UiSize::default(),
                focusable: true,
                input_triggers: Vec::new(),
            },
        ]
    }

    /// 根据 Host 分配的尺寸渲染任务摘要、任务列表或任务 Agent 主界面。
    fn render_ui(&mut self, request: UiRenderRequest) -> Option<UiFrame> {
        let lines = match request.view_id.as_str() {
            TASK_SHELF_VIEW => self.render_task_shelf(&request),
            TASK_WORKSPACE_VIEW => self.render_task_workspace(&request),
            TASK_AGENT_VIEW => self.render_task_agent(&request),
            _ => return None,
        };
        let visible = request.view_id != TASK_SHELF_VIEW || self.needs_attention();
        Some(UiFrame {
            view_id: request.view_id,
            visible,
            lines,
            cursor: None,
        })
    }

    /// 渲染前同步运行结果并自动派生就绪任务，使调度无需模型手动推进。
    fn render_ui_with_host(
        &mut self,
        host: &dyn PluginHostApi,
        request: UiRenderRequest,
    ) -> Option<UiFrame> {
        match request.view_id.as_str() {
            TASK_SHELF_VIEW | TASK_WORKSPACE_VIEW => {
                let _ = self.sync_and_schedule(host);
            }
            TASK_AGENT_VIEW => {
                if let Some(instance_id) = request.instance_id.as_deref() {
                    self.refresh_task_session(host, instance_id);
                }
            }
            _ => {}
        }
        self.render_ui(request)
    }

    /// 处理摘要入口、任务选择、显式刷新和任务 Agent 消息输入。
    fn on_ui_input_with_host(&mut self, host: &dyn PluginHostApi, input: UiInput) {
        match input.event {
            UiInputEvent::Key { code, .. }
                if input.view_id == TASK_SHELF_VIEW && code == "enter" =>
            {
                self.open_task_workspace(host);
            }
            UiInputEvent::Mouse { kind, .. }
                if input.view_id == TASK_SHELF_VIEW && kind.starts_with("down_") =>
            {
                self.open_task_workspace(host);
            }
            UiInputEvent::Key { code, .. } if input.view_id == TASK_WORKSPACE_VIEW => {
                match code.as_str() {
                    "up" => self.selected_task = self.selected_task.saturating_sub(1),
                    "down" => {
                        self.selected_task =
                            (self.selected_task + 1).min(self.order.len().saturating_sub(1));
                    }
                    "r" => {
                        let _ = self.sync_and_schedule(host);
                    }
                    "enter" => self.open_selected_task(host),
                    _ => {}
                }
            }
            UiInputEvent::Key { code, modifiers } if input.view_id == TASK_AGENT_VIEW => {
                let Some(task_id) = input.instance_id.as_deref() else {
                    return;
                };
                let event = UiInputEvent::Key { code, modifiers };
                let continued = self
                    .task_sessions
                    .get_mut(task_id)
                    .and_then(|session| session.handle_input(host, &event));
                if let Some(handle) = continued {
                    self.replace_task_agent(task_id, handle.id);
                }
            }
            _ => {}
        }
    }
}

impl WorkflowPlugin {
    /// 判断是否存在需要展示的待处理、运行中或失败任务。
    fn needs_attention(&self) -> bool {
        self.tasks.values().any(|task| {
            matches!(
                task.status,
                TaskStatus::Pending | TaskStatus::InProgress | TaskStatus::Failed
            )
        })
    }

    /// 统计指定状态的任务数量。
    fn count(&self, status: TaskStatus) -> usize {
        self.tasks
            .values()
            .filter(|task| task.status == status)
            .count()
    }

    /// 渲染输入框上方的任务进度摘要。
    fn render_task_shelf(&self, request: &UiRenderRequest) -> Vec<UiLine> {
        if self.tasks.is_empty() {
            return Vec::new();
        }
        let width = usize::from(request.width);
        let failed = self.count(TaskStatus::Failed);
        vec![
            ui_text_line(
                &clip(
                    &format!(
                        "任务  {}/{}",
                        self.count(TaskStatus::Completed),
                        self.tasks.len()
                    ),
                    width,
                ),
                Some(UiColor::Cyan),
                true,
            ),
            ui_text_line(
                &clip(
                    &format!(
                        "运行 {}  等待 {}  失败 {failed}",
                        self.count(TaskStatus::InProgress),
                        self.count(TaskStatus::Pending)
                    ),
                    width,
                ),
                Some(if failed > 0 {
                    UiColor::Red
                } else {
                    UiColor::Gray
                }),
                false,
            ),
            ui_text_line("Enter 查看任务列表", Some(UiColor::Gray), false),
        ]
    }

    /// 渲染任务列表和当前选中任务的详情。
    fn render_task_workspace(&self, request: &UiRenderRequest) -> Vec<UiLine> {
        if self.tasks.is_empty() {
            return vec![ui_text_line("任务列表为空", Some(UiColor::Gray), false)];
        }
        let width = usize::from(request.width);
        let mut lines = vec![
            ui_text_line(
                &clip(
                    &format!(
                        "任务列表  {}/{}",
                        self.count(TaskStatus::Completed),
                        self.tasks.len()
                    ),
                    width,
                ),
                Some(UiColor::Cyan),
                true,
            ),
            ui_text_line(
                &clip(&format!("自动调度  并行上限 {MAX_PARALLELISM}"), width),
                Some(UiColor::Gray),
                false,
            ),
            UiLine { spans: Vec::new() },
        ];
        let selected = self.selected_task.min(self.order.len().saturating_sub(1));
        for (index, task) in self
            .order
            .iter()
            .filter_map(|id| self.tasks.get(id))
            .enumerate()
        {
            let mut row = format!(
                "{} {} {}",
                if index == selected { ">" } else { " " },
                task_status_marker(task.status),
                task.id
            );
            if !task.depends_on.is_empty() {
                row.push_str(&format!("  依赖 {}", task.depends_on.join(", ")));
            }
            lines.push(ui_text_line(
                &clip(&row, width),
                Some(task_status_color(task.status)),
                index == selected,
            ));
        }
        if let Some(task) = self.order.get(selected).and_then(|id| self.tasks.get(id)) {
            lines.push(UiLine { spans: Vec::new() });
            lines.push(ui_text_line(
                &clip(
                    &format!("任务  {}  {}", task.id, task_status_label(task.status)),
                    width,
                ),
                Some(UiColor::Cyan),
                true,
            ));
            lines.push(ui_text_line(
                &clip(&task.prompt.replace(['\n', '\r'], " "), width),
                None,
                false,
            ));
            if let Some(error) = &task.error {
                lines.push(ui_text_line(
                    &clip(&error.replace(['\n', '\r'], " "), width),
                    Some(UiColor::Red),
                    false,
                ));
            } else if let Some(output) = &task.output {
                lines.push(ui_text_line(
                    &clip(&output.replace(['\n', '\r'], " "), width),
                    Some(UiColor::Gray),
                    false,
                ));
            }
            lines.push(ui_text_line(
                if task.agent_id.is_some() {
                    "Enter 打开任务 Agent  ·  r 刷新任务"
                } else {
                    "r 刷新任务"
                },
                Some(UiColor::Gray),
                false,
            ));
        }
        lines.truncate(usize::from(request.height));
        lines
    }

    /// 渲染任务上下文，并把剩余区域交给共享 Agent 主界面。
    fn render_task_agent(&self, request: &UiRenderRequest) -> Vec<UiLine> {
        let Some(task_id) = request.instance_id.as_deref() else {
            return vec![ui_text_line(
                "任务视图缺少实例 ID",
                Some(UiColor::Red),
                false,
            )];
        };
        let Some(task) = self.tasks.get(task_id) else {
            return vec![ui_text_line("任务不存在", Some(UiColor::Red), false)];
        };
        let mut lines = vec![
            ui_text_line(
                &clip(
                    &format!("{}  {}", task.id, task_status_label(task.status)),
                    usize::from(request.width),
                ),
                Some(task_status_color(task.status)),
                true,
            ),
            ui_text_line(
                &clip(
                    &task.prompt.replace(['\n', '\r'], " "),
                    usize::from(request.width),
                ),
                Some(UiColor::Gray),
                false,
            ),
            UiLine { spans: Vec::new() },
        ];
        if let Some(session) = self.task_sessions.get(task_id) {
            lines.extend(session.render(request.width, request.height.saturating_sub(3)));
        } else {
            lines.push(ui_text_line(
                if task.agent_id.is_some() {
                    "正在连接任务 Agent"
                } else {
                    "任务尚未启动"
                },
                Some(UiColor::Gray),
                false,
            ));
        }
        lines.truncate(usize::from(request.height));
        lines
    }

    /// 刷新任务对应的共享 Agent 主界面，不推进其他任务调度。
    fn refresh_task_session(&mut self, host: &dyn PluginHostApi, task_id: &str) {
        let Some(target) = self
            .tasks
            .get(task_id)
            .and_then(|task| task.agent_id.clone())
        else {
            return;
        };
        let session = self
            .task_sessions
            .entry(task_id.to_string())
            .or_insert_with(|| AgentViewSession::new(target.clone()));
        session.replace_target(target);
        session.refresh(host);
    }

    /// 打开任务列表工作台。
    fn open_task_workspace(&mut self, host: &dyn PluginHostApi) {
        if self.tasks.is_empty() {
            return;
        }
        self.selected_task = 0;
        self.navigation_sequence = self.navigation_sequence.saturating_add(1);
        let _ = host.navigate_view(UiNavigationRequest {
            request_id: format!("workflow-open-{}", self.navigation_sequence),
            action: UiNavigationAction::Push {
                view: UiViewInstance {
                    view_id: TASK_WORKSPACE_VIEW.into(),
                    instance_id: TASK_WORKSPACE_INSTANCE.into(),
                    title: Some("任务列表".into()),
                },
            },
        });
    }

    /// 打开当前选中且已启动的任务 Agent。
    fn open_selected_task(&mut self, host: &dyn PluginHostApi) {
        let selected = self.selected_task.min(self.order.len().saturating_sub(1));
        let Some((task_id, target)) = self
            .order
            .get(selected)
            .and_then(|id| self.tasks.get(id))
            .and_then(|task| {
                task.agent_id
                    .clone()
                    .map(|target| (task.id.clone(), target))
            })
        else {
            return;
        };
        self.task_sessions
            .entry(task_id.clone())
            .or_insert_with(|| AgentViewSession::new(target));
        self.navigation_sequence = self.navigation_sequence.saturating_add(1);
        let _ = host.navigate_view(UiNavigationRequest {
            request_id: format!("workflow-task-{}", self.navigation_sequence),
            action: UiNavigationAction::Push {
                view: UiViewInstance {
                    view_id: TASK_AGENT_VIEW.into(),
                    instance_id: task_id.clone(),
                    title: Some(task_id),
                },
            },
        });
    }

    /// 用成功会话续跑产生的新句柄替换任务的当前 Agent 映射。
    fn replace_task_agent(&mut self, task_id: &str, agent_id: AgentId) {
        if let Some(task) = self.tasks.get_mut(task_id) {
            task.agent_id = Some(agent_id);
            task.status = TaskStatus::InProgress;
            task.output = None;
            task.error = None;
        }
    }

    /// 校验并批量创建任务，随后立即同步与调度。
    fn create_tasks(&mut self, host: &dyn PluginHostApi, args: Value) -> Result<Value> {
        let args: CreateTasksArgs = serde_json::from_value(args).context("任务创建参数无效")?;
        if args.tasks.is_empty() {
            return Err(anyhow!("tasks 不能为空"));
        }
        self.insert_tasks(args.tasks)?;
        self.sync_and_schedule(host)?;
        Ok(self.list_snapshot())
    }

    /// 校验批量任务的标识、依赖引用和无环性后插入列表。
    fn insert_tasks(&mut self, inputs: Vec<TaskInput>) -> Result<()> {
        let mut known: BTreeSet<&str> = self.tasks.keys().map(String::as_str).collect();
        for input in &inputs {
            validate_task_input(input)?;
            if !known.insert(input.id.as_str()) {
                return Err(anyhow!("任务 `{}` 已存在", input.id));
            }
        }
        for input in &inputs {
            for dependency in &input.depends_on {
                if !known.contains(dependency.as_str()) {
                    return Err(anyhow!("任务 `{}` 的依赖 `{dependency}` 不存在", input.id));
                }
            }
        }
        self.ensure_acyclic_with(
            &inputs
                .iter()
                .map(|input| (input.id.as_str(), input.depends_on.as_slice()))
                .collect(),
        )?;
        for input in inputs {
            self.order.push(input.id.clone());
            self.tasks.insert(input.id.clone(), Task::from_input(input));
        }
        Ok(())
    }

    /// 按参数更新单个任务，随后立即同步与调度。
    fn update_task(&mut self, host: &dyn PluginHostApi, args: Value) -> Result<Value> {
        let args: UpdateTaskArgs = serde_json::from_value(args).context("任务更新参数无效")?;
        if args.delete {
            if args.status.is_some() || args.prompt.is_some() || args.depends_on.is_some() {
                return Err(anyhow!("delete 不能与其他更新字段同时使用"));
            }
            self.delete_task(&args.id)?;
        } else {
            if args.status.is_none() && args.prompt.is_none() && args.depends_on.is_none() {
                return Err(anyhow!(
                    "task_update 至少需要 status、prompt、depends_on 或 delete 之一"
                ));
            }
            if let Some(status) = args.status {
                self.transition_task(host, &args.id, status)?;
            }
            if let Some(prompt) = args.prompt {
                self.edit_prompt(&args.id, prompt)?;
            }
            if let Some(depends_on) = args.depends_on {
                self.edit_depends_on(&args.id, depends_on)?;
            }
        }
        self.sync_and_schedule(host)?;
        Ok(self.list_snapshot())
    }

    /// 执行取消或重置状态转换；取消运行中任务时同步取消其 Agent。
    fn transition_task(
        &mut self,
        host: &dyn PluginHostApi,
        task_id: &str,
        status: UpdateStatus,
    ) -> Result<()> {
        match status {
            UpdateStatus::Pending => self.reset_task(task_id),
            UpdateStatus::Cancelled => {
                let task = self.task_mut(task_id)?;
                match task.status {
                    TaskStatus::Pending => {
                        task.status = TaskStatus::Cancelled;
                        Ok(())
                    }
                    TaskStatus::InProgress => {
                        if let Some(agent_id) = &task.agent_id {
                            host.cancel_agent(agent_id)?;
                        }
                        task.status = TaskStatus::Cancelled;
                        Ok(())
                    }
                    _ => Err(anyhow!("任务 `{task_id}` 已进入终态，无法取消")),
                }
            }
        }
    }

    /// 把失败或已取消的任务重置为待处理并清空执行痕迹，以便自动重跑。
    fn reset_task(&mut self, task_id: &str) -> Result<()> {
        let task = self.task_mut(task_id)?;
        if !matches!(task.status, TaskStatus::Failed | TaskStatus::Cancelled) {
            return Err(anyhow!(
                "任务 `{task_id}` 不是失败或已取消状态，无法重置为 pending"
            ));
        }
        task.status = TaskStatus::Pending;
        task.agent_id = None;
        task.output = None;
        task.error = None;
        Ok(())
    }

    /// 修改待处理任务的提示词。
    fn edit_prompt(&mut self, task_id: &str, prompt: String) -> Result<()> {
        validate_prompt(task_id, &prompt)?;
        let task = self.task_mut(task_id)?;
        if task.status != TaskStatus::Pending {
            return Err(anyhow!("只有待处理任务可以修改提示词"));
        }
        task.prompt = prompt;
        Ok(())
    }

    /// 整体替换待处理任务的依赖，并保证依赖图仍然无环。
    fn edit_depends_on(&mut self, task_id: &str, depends_on: Vec<String>) -> Result<()> {
        let task = self.task_mut(task_id)?;
        if task.status != TaskStatus::Pending {
            return Err(anyhow!("只有待处理任务可以修改依赖"));
        }
        let unique = depends_on.iter().collect::<BTreeSet<_>>();
        if unique.len() != depends_on.len() {
            return Err(anyhow!("任务 `{task_id}` 包含重复依赖"));
        }
        for dependency in &depends_on {
            if dependency == task_id {
                return Err(anyhow!("任务 `{task_id}` 不能依赖自身"));
            }
            if !self.tasks.contains_key(dependency) {
                return Err(anyhow!("任务 `{task_id}` 的依赖 `{dependency}` 不存在"));
            }
        }
        self.ensure_acyclic_with(&BTreeMap::from([(task_id, depends_on.as_slice())]))?;
        self.task_mut(task_id)?.depends_on = depends_on;
        Ok(())
    }

    /// 删除未运行且无人依赖的任务及其残留会话。
    fn delete_task(&mut self, task_id: &str) -> Result<()> {
        let task = self
            .tasks
            .get(task_id)
            .ok_or_else(|| anyhow!("任务 `{task_id}` 不存在"))?;
        if task.status == TaskStatus::InProgress {
            return Err(anyhow!("任务 `{task_id}` 正在运行，请先取消再删除"));
        }
        if let Some(dependent) = self
            .tasks
            .values()
            .find(|other| other.depends_on.iter().any(|dep| dep == task_id))
        {
            return Err(anyhow!(
                "任务 `{}` 依赖 `{task_id}`，无法删除",
                dependent.id
            ));
        }
        self.tasks.remove(task_id);
        self.order.retain(|id| id != task_id);
        self.task_sessions.remove(task_id);
        Ok(())
    }

    /// 同步后读取单个任务的完整详情。
    fn get_task(&mut self, host: &dyn PluginHostApi, args: &Value) -> Result<Value> {
        self.sync_and_schedule(host)?;
        let task_id = required_task_id(args)?;
        let task = self
            .tasks
            .get(&task_id)
            .ok_or_else(|| anyhow!("任务 `{task_id}` 不存在"))?;
        Ok(self.task_value(task, true))
    }

    /// 同步运行中任务的终态并自动派生并行度预算内的就绪任务。
    fn sync_and_schedule(&mut self, host: &dyn PluginHostApi) -> Result<()> {
        self.refresh_running_tasks(host)?;
        self.spawn_ready_tasks(host);
        Ok(())
    }

    /// 同步所有运行中任务的幂等终态结果。
    fn refresh_running_tasks(&mut self, host: &dyn PluginHostApi) -> Result<()> {
        let running = self
            .tasks
            .iter()
            .filter(|(_, task)| task.status == TaskStatus::InProgress)
            .map(|(id, task)| (id.clone(), task.agent_id.clone()))
            .collect::<Vec<_>>();

        for (task_id, agent_id) in running {
            let agent_id =
                agent_id.ok_or_else(|| anyhow!("运行中任务 `{task_id}` 缺少 Agent ID"))?;
            let Some(outcome) = host.agent_result(&agent_id)? else {
                continue;
            };
            let task = self
                .tasks
                .get_mut(&task_id)
                .ok_or_else(|| anyhow!("任务 `{task_id}` 在结果同步时消失"))?;
            match outcome {
                AgentOutcome::Succeeded { result } => {
                    task.status = TaskStatus::Completed;
                    task.output = Some(result.final_text);
                }
                AgentOutcome::Failed { error } => {
                    task.status = TaskStatus::Failed;
                    task.error = Some(error);
                }
                AgentOutcome::Cancelled => task.status = TaskStatus::Cancelled,
            }
        }
        Ok(())
    }

    /// 按创建顺序启动并行度预算内的就绪任务；单个派生失败只影响对应任务。
    fn spawn_ready_tasks(&mut self, host: &dyn PluginHostApi) {
        let capacity = MAX_PARALLELISM.saturating_sub(self.count(TaskStatus::InProgress));
        let ready = self
            .order
            .iter()
            .filter(|id| {
                self.tasks
                    .get(*id)
                    .is_some_and(|task| task.status == TaskStatus::Pending)
            })
            .filter(|id| {
                self.tasks.get(*id).is_some_and(|task| {
                    task.depends_on.iter().all(|dependency| {
                        self.tasks
                            .get(dependency)
                            .is_some_and(|dependency| dependency.status == TaskStatus::Completed)
                    })
                })
            })
            .take(capacity)
            .cloned()
            .collect::<Vec<_>>();

        for task_id in ready {
            let input = self.build_task_input(&task_id);
            let Some(task) = self.tasks.get_mut(&task_id) else {
                continue;
            };
            match host.spawn_agent(&AgentSpawnRequest::new(WORKER_PROFILE, input)) {
                Ok(handle) => {
                    task.status = TaskStatus::InProgress;
                    task.agent_id = Some(handle.id);
                }
                Err(error) => {
                    task.status = TaskStatus::Failed;
                    task.error = Some(format!("启动 Agent 失败：{error}"));
                }
            }
        }
    }

    /// 合并任务提示词与其直接依赖的可信终态输出。
    fn build_task_input(&self, task_id: &str) -> String {
        let task = self.tasks.get(task_id).expect("待启动任务必须存在");
        if task.depends_on.is_empty() {
            return task.prompt.clone();
        }
        let outputs = task
            .depends_on
            .iter()
            .filter_map(|dependency| {
                self.tasks
                    .get(dependency)
                    .and_then(|task| task.output.as_ref())
                    .map(|output| (dependency, output))
            })
            .collect::<BTreeMap<_, _>>();
        format!(
            "{}\n\n依赖任务结果：\n{}",
            task.prompt,
            serde_json::to_string_pretty(&outputs).expect("依赖输出必须可序列化")
        )
    }

    /// 校验现有任务与增量修改合并后的依赖图仍然无环。
    fn ensure_acyclic_with(&self, overrides: &BTreeMap<&str, &[String]>) -> Result<()> {
        let mut graph: BTreeMap<&str, &[String]> = self
            .tasks
            .iter()
            .map(|(id, task)| (id.as_str(), task.depends_on.as_slice()))
            .collect();
        for (id, depends_on) in overrides {
            graph.insert(id, depends_on);
        }
        let mut visiting = BTreeSet::new();
        let mut visited = BTreeSet::new();
        for id in graph.keys() {
            visit_task(id, &graph, &mut visiting, &mut visited)?;
        }
        Ok(())
    }

    /// 返回按创建顺序排列的全部任务快照；任务输出通过 task_get 读取。
    fn list_snapshot(&self) -> Value {
        let tasks = self
            .order
            .iter()
            .filter_map(|id| self.tasks.get(id))
            .map(|task| self.task_value(task, false))
            .collect::<Vec<_>>();
        json!({ "tasks": tasks })
    }

    /// 序列化单个任务；待处理任务附带尚未完成的阻塞依赖。
    fn task_value(&self, task: &Task, include_output: bool) -> Value {
        let mut value = json!({
            "id": task.id,
            "prompt": task.prompt,
            "status": task_status_key(task.status),
            "depends_on": task.depends_on,
        });
        if task.status == TaskStatus::Pending {
            let blocked_by = task
                .depends_on
                .iter()
                .filter(|dependency| {
                    self.tasks
                        .get(*dependency)
                        .is_none_or(|dependency| dependency.status != TaskStatus::Completed)
                })
                .collect::<Vec<_>>();
            if !blocked_by.is_empty() {
                value["blocked_by"] = json!(blocked_by);
            }
        }
        if let Some(error) = &task.error {
            value["error"] = json!(error);
        }
        if include_output {
            if let Some(output) = &task.output {
                value["output"] = json!(output);
            }
        }
        value
    }

    /// 返回指定任务的可变引用。
    fn task_mut(&mut self, task_id: &str) -> Result<&mut Task> {
        self.tasks
            .get_mut(task_id)
            .ok_or_else(|| anyhow!("任务 `{task_id}` 不存在"))
    }
}

impl Task {
    /// 从已校验输入构造等待调度的任务。
    fn from_input(input: TaskInput) -> Self {
        Self {
            id: input.id,
            prompt: input.prompt,
            depends_on: input.depends_on,
            status: TaskStatus::Pending,
            agent_id: None,
            output: None,
            error: None,
        }
    }
}

/// 深度优先检查任务依赖图是否存在环。
fn visit_task<'a>(
    task_id: &'a str,
    graph: &BTreeMap<&'a str, &'a [String]>,
    visiting: &mut BTreeSet<&'a str>,
    visited: &mut BTreeSet<&'a str>,
) -> Result<()> {
    if visited.contains(task_id) {
        return Ok(());
    }
    if !visiting.insert(task_id) {
        return Err(anyhow!("任务列表包含涉及任务 `{task_id}` 的依赖环"));
    }
    let depends_on = graph
        .get(task_id)
        .ok_or_else(|| anyhow!("任务 `{task_id}` 不存在"))?;
    for dependency in depends_on.iter() {
        visit_task(dependency, graph, visiting, visited)?;
    }
    visiting.remove(task_id);
    visited.insert(task_id);
    Ok(())
}

/// 返回任务状态的稳定 JSON 键。
fn task_status_key(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "pending",
        TaskStatus::InProgress => "in_progress",
        TaskStatus::Completed => "completed",
        TaskStatus::Failed => "failed",
        TaskStatus::Cancelled => "cancelled",
    }
}

/// 返回任务状态的固定宽度标记。
fn task_status_marker(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "[ ]",
        TaskStatus::InProgress => "[>]",
        TaskStatus::Completed => "[x]",
        TaskStatus::Failed => "[!]",
        TaskStatus::Cancelled => "[-]",
    }
}

/// 返回任务状态的紧凑中文标签。
fn task_status_label(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "等待",
        TaskStatus::InProgress => "运行中",
        TaskStatus::Completed => "已完成",
        TaskStatus::Failed => "失败",
        TaskStatus::Cancelled => "已取消",
    }
}

/// 返回任务状态的终端颜色。
fn task_status_color(status: TaskStatus) -> UiColor {
    match status {
        TaskStatus::Pending | TaskStatus::Cancelled => UiColor::Gray,
        TaskStatus::InProgress => UiColor::Yellow,
        TaskStatus::Completed => UiColor::Green,
        TaskStatus::Failed => UiColor::Red,
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

/// 校验任务标识、提示词和依赖去重。
fn validate_task_input(input: &TaskInput) -> Result<()> {
    if input.id.trim().is_empty()
        || input.id.len() > 128
        || !input
            .id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(anyhow!("任务 ID `{}` 格式无效", input.id));
    }
    validate_prompt(&input.id, &input.prompt)?;
    let unique = input.depends_on.iter().collect::<BTreeSet<_>>();
    if unique.len() != input.depends_on.len() {
        return Err(anyhow!("任务 `{}` 包含重复依赖", input.id));
    }
    if input
        .depends_on
        .iter()
        .any(|dependency| dependency == &input.id)
    {
        return Err(anyhow!("任务 `{}` 不能依赖自身", input.id));
    }
    Ok(())
}

/// 校验任务提示词的长度边界。
fn validate_prompt(task_id: &str, prompt: &str) -> Result<()> {
    if prompt.trim().is_empty() || prompt.len() > MAX_PROMPT_BYTES {
        return Err(anyhow!(
            "任务 `{task_id}` 的提示词必须为 1 到 {MAX_PROMPT_BYTES} 字节"
        ));
    }
    Ok(())
}

/// 从工具参数中读取任务标识。
fn required_task_id(args: &Value) -> Result<String> {
    let task_id = args
        .get("id")
        .and_then(Value::as_str)
        .context("参数 `id` 必须是字符串")?;
    if task_id.trim().is_empty() {
        return Err(anyhow!("参数 `id` 不能为空"));
    }
    Ok(task_id.to_owned())
}

/// 返回任务标识的 JSON Schema 属性。
fn task_id_property() -> Value {
    json!({
        "type": "string",
        "description": "task_create 中登记的任务 ID。",
        "minLength": 1
    })
}

/// 返回新任务输入的 JSON Schema。
fn task_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": {
                "type": "string",
                "pattern": "^[A-Za-z0-9._-]{1,128}$"
            },
            "prompt": {"type": "string", "minLength": 1, "maxLength": MAX_PROMPT_BYTES},
            "depends_on": {
                "type": "array",
                "items": {"type": "string"},
                "uniqueItems": true,
                "default": [],
                "description": "该任务依赖的已有任务或同批任务 ID。"
            }
        },
        "required": ["id", "prompt"],
        "additionalProperties": false
    })
}

/// 返回批量创建任务工具的 JSON Schema。
fn create_tasks_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "tasks": {
                "type": "array",
                "minItems": 1,
                "items": task_schema()
            }
        },
        "required": ["tasks"],
        "additionalProperties": false
    })
}

/// 返回更新任务工具的 JSON Schema。
fn update_task_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": task_id_property(),
            "status": {
                "type": "string",
                "enum": ["pending", "cancelled"],
                "description": "cancelled 取消待处理或运行中的任务；pending 重置失败或已取消的任务以便自动重跑。"
            },
            "prompt": {
                "type": "string",
                "minLength": 1,
                "maxLength": MAX_PROMPT_BYTES,
                "description": "替换待处理任务的提示词。"
            },
            "depends_on": {
                "type": "array",
                "items": {"type": "string"},
                "uniqueItems": true,
                "description": "整体替换待处理任务的依赖。"
            },
            "delete": {
                "type": "boolean",
                "default": false,
                "description": "删除任务；仅允许删除未运行且无人依赖的任务，不能与其他字段同用。"
            }
        },
        "required": ["id"],
        "additionalProperties": false
    })
}

/// 返回读取单个任务详情工具的 JSON Schema。
fn task_get_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"id": task_id_property()},
        "required": ["id"],
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

    /// 构造便于测试的任务输入。
    fn task_input(id: &str, prompt: &str, depends_on: &[&str]) -> TaskInput {
        TaskInput {
            id: id.into(),
            prompt: prompt.into(),
            depends_on: depends_on.iter().map(|dep| dep.to_string()).collect(),
        }
    }

    /// 工作流插件必须把摘要放在输入区上方，并把任务列表与 Agent 放进子视图。
    #[test]
    fn declares_shelf_workspace_and_task_views() {
        let declarations = WorkflowPlugin::default().describe_ui();

        assert_eq!(declarations.len(), 3);
        assert!(matches!(
            declarations[0].placement,
            UiPlacement::ComposerShelf
        ));
        assert!(matches!(declarations[1].placement, UiPlacement::Subview));
        assert!(matches!(declarations[2].placement, UiPlacement::Subview));
    }

    /// 创建任务后摘要应显示进度，工作台应显示任务、依赖和阻塞信息。
    #[test]
    fn renders_task_summary_and_list() {
        let mut plugin = WorkflowPlugin::default();
        plugin
            .insert_tasks(vec![
                task_input("build", "构建产物", &[]),
                task_input("review", "复核产物", &["build"]),
            ])
            .expect("任务应创建成功");

        let shelf = plugin
            .render_ui(request(TASK_SHELF_VIEW, None))
            .expect("摘要应返回帧");
        assert!(shelf.visible);
        assert!(frame_text(&shelf).contains("0/2"));

        let workspace = plugin
            .render_ui(request(TASK_WORKSPACE_VIEW, Some(TASK_WORKSPACE_INSTANCE)))
            .expect("工作台应返回帧");
        let text = frame_text(&workspace);
        assert!(text.contains("build"), "{text}");
        assert!(text.contains("依赖 build"), "{text}");

        let snapshot = plugin.list_snapshot();
        assert_eq!(snapshot["tasks"][0]["id"], "build");
        assert_eq!(snapshot["tasks"][1]["blocked_by"][0], "build");
    }

    /// 批内依赖环必须在创建时被拒绝。
    #[test]
    fn rejects_dependency_cycle_on_create() {
        let mut plugin = WorkflowPlugin::default();

        let error = plugin
            .insert_tasks(vec![
                task_input("a", "任务 A", &["b"]),
                task_input("b", "任务 B", &["a"]),
            ])
            .expect_err("依赖环应被拒绝");

        assert!(error.to_string().contains("依赖环"), "{error}");
    }

    /// 修改依赖引入环时必须被拒绝。
    #[test]
    fn rejects_dependency_cycle_on_update() {
        let mut plugin = WorkflowPlugin::default();
        plugin
            .insert_tasks(vec![
                task_input("a", "任务 A", &[]),
                task_input("b", "任务 B", &["a"]),
            ])
            .expect("任务应创建成功");

        let error = plugin
            .edit_depends_on("a", vec!["b".into()])
            .expect_err("依赖环应被拒绝");

        assert!(error.to_string().contains("依赖环"), "{error}");
    }

    /// 失败任务重置后应清空执行痕迹并回到待处理；非终态任务不允许重置。
    #[test]
    fn resets_failed_task_to_pending() {
        let mut plugin = WorkflowPlugin::default();
        plugin
            .insert_tasks(vec![task_input("deploy", "部署产物", &[])])
            .expect("任务应创建成功");
        {
            let task = plugin.task_mut("deploy").expect("任务应存在");
            task.status = TaskStatus::Failed;
            task.error = Some("部署超时".into());
        }

        plugin.reset_task("deploy").expect("失败任务应可重置");
        let task = plugin.tasks.get("deploy").expect("任务应存在");
        assert!(task.status == TaskStatus::Pending);
        assert!(task.error.is_none());

        let error = plugin
            .reset_task("deploy")
            .expect_err("待处理任务不应可重置");
        assert!(error.to_string().contains("无法重置"), "{error}");
    }

    /// 被其他任务依赖的任务不允许删除；解除依赖后可以删除。
    #[test]
    fn rejects_delete_with_dependents() {
        let mut plugin = WorkflowPlugin::default();
        plugin
            .insert_tasks(vec![
                task_input("build", "构建产物", &[]),
                task_input("review", "复核产物", &["build"]),
            ])
            .expect("任务应创建成功");

        let error = plugin
            .delete_task("build")
            .expect_err("被依赖的任务不应可删除");
        assert!(error.to_string().contains("无法删除"), "{error}");

        plugin
            .delete_task("review")
            .expect("无人依赖的任务应可删除");
        plugin.delete_task("build").expect("解除依赖后应可删除");
        assert!(plugin.tasks.is_empty());
        assert!(plugin.order.is_empty());
    }

    /// 全部任务进入完成或取消终态后，摘要应自动隐藏。
    #[test]
    fn hides_shelf_when_all_tasks_settled() {
        let mut plugin = WorkflowPlugin::default();
        plugin
            .insert_tasks(vec![task_input("build", "构建产物", &[])])
            .expect("任务应创建成功");
        plugin.task_mut("build").expect("任务应存在").status = TaskStatus::Completed;

        let shelf = plugin
            .render_ui(request(TASK_SHELF_VIEW, None))
            .expect("摘要应返回帧");

        assert!(!shelf.visible);
    }
}

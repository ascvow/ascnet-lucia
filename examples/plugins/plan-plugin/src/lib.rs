//! 为 Agent 提供结构化计划管理和声明式 TUI 状态面板。

use agent_plugin::{
    core_events, export_plugin, ActivationContext, AgentPlugin, EventOutcome, PluginEvent,
    PluginHostApi, Result, RunFinishEventData, ToolCall, ToolPermissionScope, ToolResult, ToolSpec,
    UiColor, UiDeclaration, UiFrame, UiInput, UiInputEvent, UiLine, UiNavigationAction,
    UiNavigationRequest, UiPlacement, UiRenderRequest, UiSize, UiSpan, UiStyle, UiViewInstance,
};
use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashSet;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const UPDATE_PLAN_TOOL: &str = "update_plan";
const GET_PLAN_TOOL: &str = "get_plan";
const PLAN_STATE_KEY: &str = "current_plan";
const PLAN_SHELF_VIEW: &str = "plan-shelf";
const PLAN_DETAIL_VIEW: &str = "plan-detail";
const PLAN_SCHEMA_VERSION: u32 = 1;
/// 紧凑计划架最多展示的内容行数，不包含宿主绘制的顶部分隔线。
const PLAN_SHELF_CONTENT_LINES: usize = 5;
/// 紧凑计划架的总布局高度，包含一行宿主顶部分隔线。
const PLAN_SHELF_HEIGHT: u16 = 6;
/// 值得长期跟踪的计划最少步骤数；少于该数量的任务直接执行即可。
const MIN_TRACKED_STEPS: usize = 3;

/// 单个计划步骤的执行状态。
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum PlanStatus {
    /// 尚未开始的步骤。
    Pending,
    /// 当前正在执行的唯一步骤。
    InProgress,
    /// 已完成的步骤。
    Completed,
}

/// 计划中的单个可执行步骤。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct PlanItem {
    /// 面向用户的具体步骤描述。
    step: String,
    /// 当前执行状态。
    status: PlanStatus,
}

/// `update_plan` 工具接收的完整计划快照。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdatePlanArgs {
    /// 可选的计划调整原因或进度说明。
    #[serde(default)]
    explanation: Option<String>,
    /// 按执行顺序排列的全部步骤；空数组表示清空计划。
    plan: Vec<PlanItem>,
}

/// 无参数工具使用的严格空对象。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyArgs {}

/// 插件实例内保存并返回给模型的版本化计划状态。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
struct PlanState {
    /// 状态结构版本，用于拒绝不兼容的未来数据。
    #[serde(default = "default_schema_version")]
    schema_version: u32,
    /// 每次成功更新后单调递增的修订号。
    #[serde(default)]
    revision: u64,
    /// 最近一次更新附带的说明。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    explanation: Option<String>,
    /// 当前计划的有序步骤。
    #[serde(default)]
    plan: Vec<PlanItem>,
}

impl Default for PlanState {
    fn default() -> Self {
        Self {
            schema_version: PLAN_SCHEMA_VERSION,
            revision: 0,
            explanation: None,
            plan: Vec::new(),
        }
    }
}

impl PlanState {
    /// 从工具参数构造规范化的新快照，并校验计划不变量。
    fn from_update(args: UpdatePlanArgs, revision: u64) -> Result<Self> {
        let explanation = args
            .explanation
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let plan = args
            .plan
            .into_iter()
            .map(|item| PlanItem {
                step: item.step.trim().to_string(),
                status: item.status,
            })
            .collect();
        let state = Self {
            schema_version: PLAN_SCHEMA_VERSION,
            revision,
            explanation,
            plan,
        };
        state.validate()?;
        state.ensure_worth_tracking()?;
        Ok(state)
    }

    /// 校验状态版本、步骤内容、唯一性和单一进行中约束。
    fn validate(&self) -> Result<()> {
        if self.schema_version != PLAN_SCHEMA_VERSION {
            return Err(anyhow!("不支持的计划状态版本：{}", self.schema_version));
        }

        let mut steps = HashSet::new();
        let mut in_progress = 0usize;
        for (index, item) in self.plan.iter().enumerate() {
            if item.step.trim().is_empty() {
                return Err(anyhow!("计划第 {} 项的 step 不能为空", index + 1));
            }
            if !steps.insert(item.step.as_str()) {
                return Err(anyhow!("计划步骤重复：{}", item.step));
            }
            if item.status == PlanStatus::InProgress {
                in_progress += 1;
            }
        }
        if in_progress > 1 {
            return Err(anyhow!("计划最多只能有一个 in_progress 步骤"));
        }
        Ok(())
    }

    /// 校验计划是否值得长期跟踪，拒绝把单点任务包装成计划。
    ///
    /// 这是**使用策略**而不是状态不变量：激活恢复和运行结束收敛都不调用它，
    /// 因此升级前保存的小计划仍然可读、可完成，只是不再接受新的小计划。
    /// 空计划是显式清空，始终放行。
    fn ensure_worth_tracking(&self) -> Result<()> {
        if self.plan.is_empty() || self.plan.len() >= MIN_TRACKED_STEPS {
            return Ok(());
        }
        Err(anyhow!(
            "计划只有 {} 个步骤，不足以构成需要跟踪的任务。请直接完成该任务，不要为单点或窄范围工作创建计划；确实需要跟踪时，至少拆出 {MIN_TRACKED_STEPS} 个可独立验证的步骤，并始终提交包含已完成项在内的完整计划。",
            self.plan.len()
        ))
    }
}

/// 默认计划状态结构版本。
fn default_schema_version() -> u32 {
    PLAN_SCHEMA_VERSION
}

/// 管理当前 Agent 计划，并提供紧凑进度架与完整计划视图。
#[derive(Default)]
struct PlanPlugin {
    /// 当前插件实例持有的计划快照。
    state: PlanState,
    /// 生成插件实例内唯一的导航请求 ID。
    navigation_sequence: u64,
}

impl AgentPlugin for PlanPlugin {
    /// 恢复 Host 中同一插件实例的状态、注册计划提示，并拒绝不兼容或损坏的数据。
    fn activate(&mut self, host: &dyn PluginHostApi, _context: ActivationContext) -> Result<()> {
        if let Some(value) = host.get_state(PLAN_STATE_KEY)? {
            let state: PlanState = serde_json::from_value(value).context("解析计划状态失败")?;
            state.validate()?;
            self.state = state;
        } else {
            host.set_state(PLAN_STATE_KEY, &serde_json::to_value(&self.state)?)?;
        }
        Ok(())
    }

    /// 保留持久化计划状态供下次激活恢复；插件不注册常驻提示，使用时机由工具描述承载。
    fn deactivate(&mut self, _host: &dyn PluginHostApi) -> Result<()> {
        Ok(())
    }

    /// 返回更新和读取当前计划的模型工具。
    fn list_tools(&self) -> Vec<ToolSpec> {
        vec![update_plan_tool(), get_plan_tool()]
    }

    /// 正常运行结束时收敛当前执行项，避免最终回复与计划面板状态不一致。
    ///
    /// 取消运行时保留原状态，防止把中断任务误标为完成。本插件只订阅
    /// `core.run.finish`，不参与任何准入决策。
    fn on_event(&mut self, _host: &dyn PluginHostApi, event: &mut PluginEvent) -> EventOutcome {
        if event.name() != core_events::RUN_FINISH {
            return EventOutcome::ready();
        }
        let Ok(data) = event.parse::<RunFinishEventData>() else {
            return EventOutcome::ready();
        };
        if data.cancelled {
            return EventOutcome::ready();
        }
        let mut changed = false;
        for item in &mut self.state.plan {
            if item.status == PlanStatus::InProgress {
                item.status = PlanStatus::Completed;
                changed = true;
            }
        }
        if changed {
            self.state.revision = self.state.revision.saturating_add(1);
        }
        EventOutcome::ready()
    }

    /// 校验并原子替换计划，或返回当前只读快照。
    fn call_tool_with_host(
        &mut self,
        host: &dyn PluginHostApi,
        call: ToolCall,
    ) -> Result<ToolResult> {
        match call.name.as_str() {
            UPDATE_PLAN_TOOL => self.update_plan(host, call),
            GET_PLAN_TOOL => self.get_plan(call),
            _ => Ok(ToolResult::error(
                call.id,
                call.name,
                "Plan 插件收到未知工具调用",
            )),
        }
    }

    /// 声明输入区上方的紧凑计划架和按需打开的完整计划子视图。
    fn describe_ui(&self) -> Vec<UiDeclaration> {
        vec![
            UiDeclaration {
                plugin_id: String::new(),
                view_id: PLAN_SHELF_VIEW.into(),
                title: "Plan".into(),
                placement: UiPlacement::ComposerShelf,
                size: UiSize {
                    width: None,
                    height: Some(PLAN_SHELF_HEIGHT),
                },
                focusable: true,
                input_triggers: Vec::new(),
            },
            UiDeclaration {
                plugin_id: String::new(),
                view_id: PLAN_DETAIL_VIEW.into(),
                title: "Plan details".into(),
                placement: UiPlacement::Subview,
                size: UiSize::default(),
                focusable: true,
                input_triggers: Vec::new(),
            },
        ]
    }

    /// 按宿主分配的宽高渲染紧凑进度或完整步骤列表。
    ///
    /// 没有计划或全部步骤完成时返回隐藏帧，使宿主自动收回布局空间。
    fn render_ui(&mut self, request: UiRenderRequest) -> Option<UiFrame> {
        let completed = self
            .state
            .plan
            .iter()
            .filter(|item| item.status == PlanStatus::Completed)
            .count();
        let mut lines = match request.view_id.as_str() {
            PLAN_SHELF_VIEW => self.render_shelf(completed, request.width as usize),
            PLAN_DETAIL_VIEW => self.render_detail(completed, request.width as usize),
            _ => return None,
        };
        lines.truncate(request.height as usize);

        Some(UiFrame {
            view_id: request.view_id,
            visible: self
                .state
                .plan
                .iter()
                .any(|item| item.status != PlanStatus::Completed),
            lines,
            cursor: None,
        })
    }

    /// 在紧凑计划架上响应 Enter 或鼠标按下，打开完整计划子视图。
    fn on_ui_input_with_host(&mut self, host: &dyn PluginHostApi, input: UiInput) {
        if input.view_id != PLAN_SHELF_VIEW {
            return;
        }
        let open = matches!(input.event, UiInputEvent::Key { ref code, .. } if code == "enter")
            || matches!(input.event, UiInputEvent::Mouse { ref kind, .. } if kind.starts_with("down_"));
        if !open {
            return;
        }
        self.navigation_sequence = self.navigation_sequence.saturating_add(1);
        let _ = host.navigate_view(UiNavigationRequest {
            request_id: format!("plan-open-{}", self.navigation_sequence),
            action: UiNavigationAction::Push {
                view: UiViewInstance {
                    view_id: PLAN_DETAIL_VIEW.into(),
                    instance_id: "current".into(),
                    title: Some("Plan details".into()),
                },
            },
        });
    }
}

impl PlanPlugin {
    /// 构造输入区上方的紧凑进度架，最多展示五行并用汇总行收起其余任务。
    fn render_shelf(&self, completed: usize, width: usize) -> Vec<UiLine> {
        let mut lines = vec![styled_line(
            truncate_to_width(
                &format!("Plan  {completed}/{} completed", self.state.plan.len()),
                width,
            ),
            Some(UiColor::Cyan),
            true,
        )];
        let ordered = self.ordered_plan_items().collect::<Vec<_>>();
        let task_capacity = PLAN_SHELF_CONTENT_LINES.saturating_sub(1);
        let visible_tasks = if ordered.len() > task_capacity {
            task_capacity.saturating_sub(1)
        } else {
            ordered.len()
        };
        lines.extend(
            ordered
                .iter()
                .take(visible_tasks)
                .map(|item| render_plan_item(item, width)),
        );
        let hidden = ordered.len().saturating_sub(visible_tasks);
        if hidden > 0 {
            lines.push(styled_line(
                truncate_to_width(&format!("+{hidden} Task"), width),
                Some(UiColor::Gray),
                false,
            ));
        }
        lines
    }

    /// 构造完整计划子视图，展示说明和全部步骤状态。
    fn render_detail(&self, completed: usize, width: usize) -> Vec<UiLine> {
        let mut lines = vec![styled_line(
            truncate_to_width(
                &format!("Progress  {completed} / {}", self.state.plan.len()),
                width,
            ),
            Some(UiColor::Cyan),
            true,
        )];
        if let Some(explanation) = &self.state.explanation {
            lines.push(styled_line(
                truncate_to_width(&format!("Note: {explanation}"), width),
                Some(UiColor::Gray),
                false,
            ));
            lines.push(styled_line(String::new(), None, false));
        }
        lines.extend(
            self.ordered_plan_items()
                .map(|item| render_plan_item(item, width)),
        );
        lines
    }

    /// 按显示顺序遍历计划项：未完成项保持原顺序，已完成项稳定移动到末尾。
    fn ordered_plan_items(&self) -> impl Iterator<Item = &PlanItem> {
        self.state
            .plan
            .iter()
            .filter(|item| item.status != PlanStatus::Completed)
            .chain(
                self.state
                    .plan
                    .iter()
                    .filter(|item| item.status == PlanStatus::Completed),
            )
    }

    /// 解析、校验并保存一次完整计划更新；Host 写入失败时保留旧状态。
    fn update_plan(&mut self, host: &dyn PluginHostApi, call: ToolCall) -> Result<ToolResult> {
        let args: UpdatePlanArgs = match call.args_as() {
            Ok(args) => args,
            Err(error) => {
                return Ok(ToolResult::error(
                    call.id,
                    call.name,
                    format!("计划参数无效：{error}"),
                ));
            }
        };
        let next = match PlanState::from_update(args, self.state.revision.saturating_add(1)) {
            Ok(state) => state,
            Err(error) => {
                return Ok(ToolResult::error(call.id, call.name, error.to_string()));
            }
        };
        host.set_state(PLAN_STATE_KEY, &serde_json::to_value(&next)?)?;
        self.state = next;
        Ok(ToolResult::success(
            call.id,
            call.name,
            serde_json::to_value(&self.state)?,
        ))
    }

    /// 返回当前计划快照，并严格拒绝意外参数。
    fn get_plan(&self, call: ToolCall) -> Result<ToolResult> {
        if let Err(error) = call.args_as::<EmptyArgs>() {
            return Ok(ToolResult::error(
                call.id,
                call.name,
                format!("查询参数无效：{error}"),
            ));
        }
        Ok(ToolResult::success(
            call.id,
            call.name,
            serde_json::to_value(&self.state)?,
        ))
    }
}

/// 构建整体替换当前计划的工具定义。
///
/// 描述是使用时机的主要约束：计划只服务困难或较大的任务，普通单点任务直接执行。
/// 这里刻意写出可判断的准入条件与反例，避免模型仅因为工具存在就为每个任务开计划。
/// 最小步骤数直接取自 [`MIN_TRACKED_STEPS`]，描述与准入策略不会各自漂移。
fn update_plan_tool() -> ToolSpec {
    let description = format!(
        "仅为困难或较大的任务创建或更新计划；默认不要创建。满足以下任一条件才使用：改动跨多个模块或多个文件且顺序有依赖；任务能拆成 {MIN_TRACKED_STEPS} 个以上可独立验证的阶段；预计多轮长时执行，可能中途暂停或需要持续向用户汇报进度；用户明确要求维护计划。以下情况不要创建计划：直接问答与代码解释、代码阅读与初步探索、单文件小改动、单条命令或单次测试、格式化与重命名、窄范围修复，以及一次连续执行就能完成的任务。计划至少 {MIN_TRACKED_STEPS} 个步骤，每步必须是独立可验证的成果，不要把同一个动作拆成多步凑数；使用后及时把完成项标记为 completed，始终提交包含已完成项的完整计划，且最多一个步骤为 in_progress。"
    );
    ToolSpec::new(
        UPDATE_PLAN_TOOL,
        description,
        json!({
            "type": "object",
            "properties": {
                "explanation": {
                    "type": "string",
                    "description": "可选的计划调整原因或进度说明。"
                },
                "plan": {
                    "type": "array",
                    "description": format!("按执行顺序排列的完整计划，至少 {MIN_TRACKED_STEPS} 步；空数组用于清空计划。"),
                    "items": {
                        "type": "object",
                        "properties": {
                            "step": {
                                "type": "string",
                                "minLength": 1,
                                "description": "具体且可验证的步骤描述。"
                            },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"]
                            }
                        },
                        "required": ["step", "status"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["plan"],
            "additionalProperties": false
        }),
    )
    .with_permission_scope(ToolPermissionScope::Control)
}

/// 构建读取当前计划快照的无参数工具定义。
fn get_plan_tool() -> ToolSpec {
    ToolSpec::new(
        GET_PLAN_TOOL,
        "读取当前任务计划、步骤状态和修订号。仅在需要确认现有计划且上下文中没有最新快照时调用。",
        ToolSpec::empty_object_schema(),
    )
    .with_permission_scope(ToolPermissionScope::Control)
}

/// 创建带统一终端样式的单行文本。
fn styled_line(text: String, foreground: Option<UiColor>, bold: bool) -> UiLine {
    UiLine {
        spans: vec![UiSpan {
            text,
            style: UiStyle {
                foreground,
                bold,
                ..UiStyle::default()
            },
        }],
    }
}

/// 按计划状态渲染单个任务行，并限制其终端显示宽度。
///
/// 待处理项使用正常文本色，进行中项使用黄色强调，已完成项使用灰色弱化，
/// 避免完成态比尚未处理的任务更抢眼。
fn render_plan_item(item: &PlanItem, width: usize) -> UiLine {
    let (marker, color) = match item.status {
        PlanStatus::Pending => ("[ ]", UiColor::White),
        PlanStatus::InProgress => ("[>]", UiColor::Yellow),
        PlanStatus::Completed => ("[x]", UiColor::Gray),
    };
    styled_line(
        truncate_to_width(&format!("{marker} {}", item.step), width),
        Some(color),
        item.status == PlanStatus::InProgress,
    )
}

/// 按终端显示宽度截断文本，确保中英文内容不会越过面板边界。
fn truncate_to_width(text: &str, max_width: usize) -> String {
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

export_plugin!(PlanPlugin);

#[cfg(test)]
mod tests {
    use super::*;
    use agent_plugin::{NoopHostApi, PluginEventEnvelope};

    /// Plan 工具只修改插件内计划状态，必须声明为无需资源审批的控制面 scope。
    #[test]
    fn plan_tools_declare_control_scope() {
        let tools = PlanPlugin::default().list_tools();
        assert_eq!(tools.len(), 2);
        assert!(tools
            .iter()
            .all(|tool| tool.permission_scope == ToolPermissionScope::Control));
    }

    /// 工具描述必须把计划限定为困难或较大的任务，并给出可判断的反例与最小步骤数。
    #[test]
    fn update_plan_description_requires_real_task_complexity() {
        let description = update_plan_tool().description;

        assert!(description.contains("默认不要创建"));
        assert!(description.contains("跨多个模块或多个文件"));
        assert!(description.contains("3 个以上可独立验证的阶段"));
        assert!(description.contains("用户明确要求维护计划"));
        assert!(description.contains("直接问答"));
        assert!(description.contains("单文件小改动"));
        assert!(description.contains("一次连续执行就能完成"));
        assert!(description.contains("计划至少 3 个步骤"));
        assert!(description.contains("不要把同一个动作拆成多步凑数"));
    }

    /// 不足三步的计划会被拒绝，并明确提示直接执行任务。
    #[test]
    fn update_rejects_plans_too_small_to_track() {
        for plan in [
            vec![item("修改一个常量", PlanStatus::InProgress)],
            vec![
                item("修改实现", PlanStatus::InProgress),
                item("运行测试", PlanStatus::Pending),
            ],
        ] {
            let error = PlanState::from_update(
                UpdatePlanArgs {
                    explanation: None,
                    plan,
                },
                1,
            )
            .expect_err("少于三步的计划必须被拒绝");
            assert!(error.to_string().contains("不足以构成需要跟踪的任务"));
            assert!(error.to_string().contains("至少拆出 3 个"));
        }
    }

    /// 三步及以上的计划仍可正常创建，避免准入策略误伤真实的大型任务。
    #[test]
    fn update_accepts_plan_with_enough_steps() {
        let state = PlanState::from_update(
            UpdatePlanArgs {
                explanation: None,
                plan: vec![
                    item("定位跨模块调用链", PlanStatus::InProgress),
                    item("修改两层接口", PlanStatus::Pending),
                    item("补集成测试", PlanStatus::Pending),
                ],
            },
            1,
        )
        .expect("三步计划应通过校验");

        assert_eq!(state.plan.len(), MIN_TRACKED_STEPS);
    }

    /// 准入策略只是使用策略：升级前保存的小计划仍可恢复、读取并完成。
    #[test]
    fn small_legacy_plan_remains_readable() {
        let legacy = PlanState {
            revision: 4,
            plan: vec![item("检查实现", PlanStatus::InProgress)],
            ..PlanState::default()
        };

        legacy.validate().expect("旧的小计划仍是合法状态");
        assert!(legacy.ensure_worth_tracking().is_err());
    }

    /// 工具调用被准入策略拒绝时不写入 Host 状态，也不覆盖已有计划。
    #[test]
    fn rejected_update_keeps_existing_plan() {
        let mut plugin = PlanPlugin::default();
        plugin.state.plan = vec![
            item("定位问题", PlanStatus::Completed),
            item("修复实现", PlanStatus::InProgress),
            item("补充测试", PlanStatus::Pending),
        ];
        let before = plugin.state.clone();

        let result = plugin
            .call_tool_with_host(
                &NoopHostApi,
                ToolCall::new(
                    "call-small-plan",
                    UPDATE_PLAN_TOOL,
                    json!({"plan": [{"step": "只做一件小事", "status": "in_progress"}]}),
                ),
            )
            .expect("准入策略应返回工具结果而不是插件错误");

        assert!(result.is_error, "小计划必须以工具错误形式返回");
        assert_eq!(plugin.state, before, "被拒绝的更新不能改变当前计划");
    }

    /// 创建测试计划项，减少状态校验用例中的重复结构。
    fn item(step: &str, status: PlanStatus) -> PlanItem {
        PlanItem {
            step: step.into(),
            status,
        }
    }

    /// 验证更新会规范化文本并保留完整状态。
    #[test]
    fn update_normalizes_plan_text() {
        let state = PlanState::from_update(
            UpdatePlanArgs {
                explanation: Some("  调整执行顺序  ".into()),
                plan: vec![
                    item("  定位实现  ", PlanStatus::InProgress),
                    item("修改接口", PlanStatus::Pending),
                    item("补回归测试", PlanStatus::Pending),
                ],
            },
            3,
        )
        .expect("计划应通过校验");

        assert_eq!(state.revision, 3);
        assert_eq!(state.explanation.as_deref(), Some("调整执行顺序"));
        assert_eq!(state.plan[0].step, "定位实现");
    }

    /// 验证同时进行多个步骤会被拒绝。
    #[test]
    fn update_rejects_multiple_in_progress_items() {
        let error = PlanState::from_update(
            UpdatePlanArgs {
                explanation: None,
                plan: vec![
                    item("实现功能", PlanStatus::InProgress),
                    item("运行测试", PlanStatus::InProgress),
                    item("更新文档", PlanStatus::Pending),
                ],
            },
            1,
        )
        .expect_err("多个进行中步骤必须失败");

        assert!(error.to_string().contains("最多只能有一个"));
    }

    /// 验证空白和重复步骤都不会进入状态。
    #[test]
    fn update_rejects_invalid_steps() {
        let blank = PlanState::from_update(
            UpdatePlanArgs {
                explanation: None,
                plan: vec![item("  ", PlanStatus::Pending)],
            },
            1,
        )
        .expect_err("空白步骤必须失败");
        assert!(blank.to_string().contains("不能为空"));

        let duplicate = PlanState::from_update(
            UpdatePlanArgs {
                explanation: None,
                plan: vec![
                    item("运行测试", PlanStatus::Pending),
                    item("运行测试", PlanStatus::Completed),
                ],
            },
            1,
        )
        .expect_err("重复步骤必须失败");
        assert!(duplicate.to_string().contains("重复"));
    }

    /// 验证空计划是受支持的显式清空操作。
    #[test]
    fn update_accepts_empty_plan() {
        let state = PlanState::from_update(
            UpdatePlanArgs {
                explanation: Some("任务取消".into()),
                plan: Vec::new(),
            },
            2,
        )
        .expect("空计划应表示清空");

        assert!(state.plan.is_empty());
    }

    /// 空计划和已完成计划不应占用进度架，未完成计划仍应显示。
    #[test]
    fn panel_is_visible_only_while_plan_is_incomplete() {
        let request = || UiRenderRequest {
            plugin_id: "plan".into(),
            view_id: PLAN_SHELF_VIEW.into(),
            instance_id: None,
            width: 24,
            height: 8,
            focused: false,
            frame: 1,
        };
        let mut plugin = PlanPlugin::default();

        assert!(!plugin.render_ui(request()).expect("空计划应返回帧").visible);
        plugin.state.plan = vec![item("检查实现", PlanStatus::Pending)];
        assert!(
            plugin
                .render_ui(request())
                .expect("未完成计划应返回帧")
                .visible
        );
        plugin.state.plan[0].status = PlanStatus::Completed;
        assert!(
            !plugin
                .render_ui(request())
                .expect("完成计划应返回帧")
                .visible
        );
    }

    /// 紧凑计划架最多展示五行，超出项汇总显示，已完成任务稳定沉到末尾。
    #[test]
    fn shelf_limits_rows_and_sinks_completed_items() {
        let mut plugin = PlanPlugin::default();
        plugin.state.plan = vec![
            item("已完成一", PlanStatus::Completed),
            item("待处理", PlanStatus::Pending),
            item("已完成二", PlanStatus::Completed),
            item("处理中", PlanStatus::InProgress),
        ];

        let lines = plugin.render_shelf(2, 80);
        let text = lines
            .iter()
            .map(|line| line.spans[0].text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(lines.len(), PLAN_SHELF_CONTENT_LINES);
        assert_eq!(
            text,
            vec![
                "Plan  2/4 completed",
                "[ ] 待处理",
                "[>] 处理中",
                "[x] 已完成一",
                "[x] 已完成二",
            ]
        );
        assert_eq!(lines[1].spans[0].style.foreground, Some(UiColor::White));
        assert_eq!(lines[2].spans[0].style.foreground, Some(UiColor::Yellow));
        assert_eq!(lines[3].spans[0].style.foreground, Some(UiColor::Gray));
        assert_eq!(lines[4].spans[0].style.foreground, Some(UiColor::Gray));

        plugin.state.plan = (1..=7)
            .map(|index| item(&format!("任务 {index}"), PlanStatus::Pending))
            .collect();
        let overflow = plugin.render_shelf(0, 80);
        assert_eq!(overflow.len(), PLAN_SHELF_CONTENT_LINES);
        assert_eq!(overflow[4].spans[0].text, "+4 Task");
        assert_eq!(plugin.describe_ui()[0].size.height, Some(PLAN_SHELF_HEIGHT));
    }

    /// 正常完成应收敛执行中步骤，取消运行必须保留原状态。
    #[test]
    fn run_finished_completes_only_successful_active_step() {
        let event = |cancelled| {
            PluginEvent::new(PluginEventEnvelope {
                event: core_events::RUN_FINISH.into(),
                cancelled: false,
                canceller: None,
                subscriber: "plan".into(),
                data: json!({
                    "run_id": "run",
                    "steps_used": 1,
                    "cancelled": cancelled,
                }),
            })
        };
        let mut plugin = PlanPlugin::default();
        plugin.state.plan = vec![item("汇总评价", PlanStatus::InProgress)];

        plugin.on_event(&NoopHostApi, &mut event(true));
        assert_eq!(plugin.state.plan[0].status, PlanStatus::InProgress);
        plugin.on_event(&NoopHostApi, &mut event(false));
        assert_eq!(plugin.state.plan[0].status, PlanStatus::Completed);
        assert_eq!(plugin.state.revision, 1);
    }

    /// 验证计划面板遵守 Host 分配的宽高限制。
    #[test]
    fn panel_respects_render_bounds() {
        let mut plugin = PlanPlugin {
            state: PlanState {
                revision: 1,
                plan: vec![item(
                    "实现一个包含很长中文描述的计划插件步骤",
                    PlanStatus::InProgress,
                )],
                ..PlanState::default()
            },
            ..PlanPlugin::default()
        };
        let frame = plugin
            .render_ui(UiRenderRequest {
                plugin_id: "plan".into(),
                view_id: PLAN_DETAIL_VIEW.into(),
                instance_id: None,
                width: 16,
                height: 2,
                focused: false,
                frame: 1,
            })
            .expect("计划视图应返回帧");

        assert_eq!(frame.lines.len(), 2);
        assert!(frame.lines.iter().all(|line| {
            let text = &line.spans[0].text;
            UnicodeWidthStr::width(text.as_str()) <= 16
        }));
    }

    /// 验证模型可见工具名称和输入 schema 保持稳定。
    #[test]
    fn exposes_plan_tools() {
        let tools = PlanPlugin::default().list_tools();

        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, UPDATE_PLAN_TOOL);
        assert_eq!(tools[1].name, GET_PLAN_TOOL);
        assert_eq!(tools[0].input_schema["required"], json!(["plan"]));
    }
}

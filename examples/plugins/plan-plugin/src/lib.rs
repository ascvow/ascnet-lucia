//! 为 Agent 提供结构化计划管理和声明式 TUI 状态面板。

use agent_plugin::{
    export_plugin, ActivationContext, AgentEvent, AgentEventKind, AgentPlugin, PluginHostApi,
    PromptContribution, Result, ToolCall, ToolResult, ToolSpec, UiColor, UiDeclaration, UiFrame,
    UiLine, UiPlacement, UiRenderRequest, UiSize, UiSpan, UiStyle,
};
use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashSet;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const UPDATE_PLAN_TOOL: &str = "update_plan";
const GET_PLAN_TOOL: &str = "get_plan";
const PLAN_STATE_KEY: &str = "current_plan";
const PLAN_VIEW: &str = "plan-status";
const PLAN_SCHEMA_VERSION: u32 = 1;
/// 引导主 Agent 使用结构化计划的 developer 提示 ID。
const PLAN_MANAGEMENT_PROMPT_ID: &str = "plan-management";
/// 引导主 Agent 在复杂任务中维护结构化计划的规则。
const PLAN_MANAGEMENT_PROMPT: &str = "面对多步骤、持续时间较长，或需要向用户清晰呈现进度的任务时，先使用 update_plan 建立简短、可验证的计划。执行过程中及时将已完成步骤标记为 completed，并保持至多一个 in_progress 步骤。简单的一步问答或无需追踪的任务不要创建计划。";

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
}

/// 默认计划状态结构版本。
fn default_schema_version() -> u32 {
    PLAN_SCHEMA_VERSION
}

/// 管理当前 Agent 计划并向 TUI 提供只读状态面板。
#[derive(Default)]
struct PlanPlugin {
    /// 当前插件实例持有的计划快照。
    state: PlanState,
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
        host.upsert_prompt(&PromptContribution {
            id: PLAN_MANAGEMENT_PROMPT_ID.into(),
            content: PLAN_MANAGEMENT_PROMPT.into(),
            priority: 110,
        })?;
        Ok(())
    }

    /// 删除本插件注册的计划提示，保留持久化计划状态供下次激活恢复。
    fn deactivate(&mut self, host: &dyn PluginHostApi) -> Result<()> {
        host.remove_prompt(PLAN_MANAGEMENT_PROMPT_ID)
    }

    /// 返回更新和读取当前计划的模型工具。
    fn list_tools(&self) -> Vec<ToolSpec> {
        vec![update_plan_tool(), get_plan_tool()]
    }

    /// 正常运行结束时收敛当前执行项，避免最终回复与计划面板状态不一致。
    ///
    /// 取消运行时保留原状态，防止把中断任务误标为完成。
    fn on_event(&mut self, event: AgentEvent) {
        if event.kind != AgentEventKind::RunFinished
            || event.payload["cancelled"].as_bool().unwrap_or(false)
        {
            return;
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

    /// 声明只读的右侧计划状态面板。
    fn describe_ui(&self) -> Vec<UiDeclaration> {
        vec![UiDeclaration {
            plugin_id: String::new(),
            view_id: PLAN_VIEW.into(),
            title: "计划".into(),
            placement: UiPlacement::Right,
            size: UiSize {
                width: Some(38),
                height: None,
            },
            focusable: false,
            input_triggers: Vec::new(),
        }]
    }

    /// 按宿主分配的宽高渲染完成进度、更新说明和步骤状态。
    ///
    /// 没有计划或全部步骤完成时返回隐藏帧，使宿主自动收回右侧面板空间。
    fn render_ui(&mut self, request: UiRenderRequest) -> Option<UiFrame> {
        if request.view_id != PLAN_VIEW {
            return None;
        }

        let completed = self
            .state
            .plan
            .iter()
            .filter(|item| item.status == PlanStatus::Completed)
            .count();
        let mut lines = vec![styled_line(
            truncate_to_width(
                &format!("{completed} / {} 已完成", self.state.plan.len()),
                request.width as usize,
            ),
            Some(UiColor::Cyan),
            true,
        )];

        if let Some(explanation) = &self.state.explanation {
            lines.push(styled_line(
                truncate_to_width(&format!("说明：{explanation}"), request.width as usize),
                Some(UiColor::Gray),
                false,
            ));
        }

        if self.state.plan.is_empty() {
            lines.push(styled_line("暂无计划".into(), Some(UiColor::Gray), false));
        } else {
            for item in &self.state.plan {
                let (marker, color) = match item.status {
                    PlanStatus::Pending => ("[ ]", UiColor::Gray),
                    PlanStatus::InProgress => ("[>]", UiColor::Yellow),
                    PlanStatus::Completed => ("[x]", UiColor::Green),
                };
                lines.push(styled_line(
                    truncate_to_width(&format!("{marker} {}", item.step), request.width as usize),
                    Some(color),
                    item.status == PlanStatus::InProgress,
                ));
            }
        }
        lines.truncate(request.height as usize);

        Some(UiFrame {
            view_id: request.view_id,
            visible: self
                .state
                .plan
                .iter()
                .any(|item| item.status != PlanStatus::Completed),
            lines,
        })
    }
}

impl PlanPlugin {
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
fn update_plan_tool() -> ToolSpec {
    ToolSpec::new(
        UPDATE_PLAN_TOOL,
        "创建或更新当前任务计划。复杂任务开始时设置步骤，执行过程中及时更新状态；始终提交完整计划，且最多一个步骤为 in_progress。",
        json!({
            "type": "object",
            "properties": {
                "explanation": {
                    "type": "string",
                    "description": "可选的计划调整原因或进度说明。"
                },
                "plan": {
                    "type": "array",
                    "description": "按执行顺序排列的完整计划；空数组用于清空计划。",
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
}

/// 构建读取当前计划快照的无参数工具定义。
fn get_plan_tool() -> ToolSpec {
    ToolSpec::new(
        GET_PLAN_TOOL,
        "读取当前任务计划、步骤状态和修订号。仅在需要确认现有计划且上下文中没有最新快照时调用。",
        ToolSpec::empty_object_schema(),
    )
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
                plan: vec![item("  定位实现  ", PlanStatus::InProgress)],
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

    /// 空计划和已完成计划不应占用右侧面板，未完成计划仍应显示。
    #[test]
    fn panel_is_visible_only_while_plan_is_incomplete() {
        let request = || UiRenderRequest {
            plugin_id: "plan".into(),
            view_id: PLAN_VIEW.into(),
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

    /// 正常完成应收敛执行中步骤，取消运行必须保留原状态。
    #[test]
    fn run_finished_completes_only_successful_active_step() {
        let event = |cancelled| AgentEvent {
            id: "event".into(),
            run_id: "run".into(),
            timestamp_ms: 1,
            kind: AgentEventKind::RunFinished,
            step: 1,
            payload: json!({"cancelled": cancelled}),
        };
        let mut plugin = PlanPlugin::default();
        plugin.state.plan = vec![item("汇总评价", PlanStatus::InProgress)];

        plugin.on_event(event(true));
        assert_eq!(plugin.state.plan[0].status, PlanStatus::InProgress);
        plugin.on_event(event(false));
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
        };
        let frame = plugin
            .render_ui(UiRenderRequest {
                plugin_id: "plan".into(),
                view_id: PLAN_VIEW.into(),
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

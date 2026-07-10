//! Lucia 官方 Skill 插件。
//!
//! 插件扫描 `SKILL.md`，只把名称和描述注入模型上下文；完整指令由模型通过工具按需读取。

use agent_plugin::{
    export_plugin, ActivationContext, AgentPlugin, EventPresentation, EventPresentationTone,
    ExtensionEvent, PluginHostApi, PromptContribution, ToolCall, ToolResult, ToolSpec,
};
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::json;
use std::{collections::BTreeMap, path::Path};

/// 插件内部动态工具 ID。
const READ_TOOL_LOCAL_NAME: &str = "read";
/// 注入 Agent 的 Skill 索引提示 ID。
const SKILL_PROMPT_ID: &str = "available-skills";
/// 单个 Skill 文件允许的最大字节数。
const MAX_SKILL_BYTES: usize = 1024 * 1024;
/// 一次激活允许扫描的最大目录项数。
const MAX_SCAN_ENTRIES: usize = 4096;
/// Skill 根目录下允许的最大递归深度。
const MAX_SCAN_DEPTH: usize = 8;

/// 已解析并可按需读取的 Skill 描述。
#[derive(Debug, Clone, PartialEq, Eq)]
struct SkillDescriptor {
    /// Skill 的稳定名称。
    name: String,
    /// 注入 Agent 索引提示的简短说明。
    description: String,
    /// 相对于插件目录的 `SKILL.md` 路径。
    path: String,
}

/// `skill_read` 工具参数。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadSkillArgs {
    /// 要读取的 Skill 名称。
    name: String,
}

/// 扫描目录并向 Agent 提供 Skill 索引与读取工具的官方插件。
#[derive(Default)]
struct SkillPlugin {
    /// 按名称排序的已发现 Skill。
    skills: BTreeMap<String, SkillDescriptor>,
    /// Host 分配给动态读取工具的公开名称。
    public_tool_name: Option<String>,
}

impl AgentPlugin for SkillPlugin {
    /// 扫描 manifest 配置的目录并注册 Skill 索引和读取工具。
    fn activate(&mut self, host: &dyn PluginHostApi, context: ActivationContext) -> Result<()> {
        let skills_dir = context
            .metadata
            .get("skills_dir")
            .map(String::as_str)
            .unwrap_or("skills");
        let paths = discover_skill_paths(host, skills_dir)?;

        self.skills.clear();
        for path in paths {
            let source = host
                .read_file(&path)
                .with_context(|| format!("读取 Skill 失败：{path}"))?;
            let skill = parse_skill(&path, &source)?;
            if self.skills.insert(skill.name.clone(), skill).is_some() {
                return Err(anyhow!("Skill 名称重复：{path}"));
            }
        }

        let public_tool_name = host.upsert_tool(READ_TOOL_LOCAL_NAME, &skill_read_tool())?;
        let prompt = build_skill_prompt(&self.skills, &public_tool_name);
        host.upsert_prompt(&PromptContribution {
            id: SKILL_PROMPT_ID.into(),
            content: prompt,
            priority: 120,
        })?;
        host.set_state(
            "skills",
            &json!(self.skills.keys().cloned().collect::<Vec<_>>()),
        )?;
        host.emit_event(&ExtensionEvent {
            name: "skills.discovered".into(),
            data: json!({
                "count": self.skills.len(),
                "skills": self.skills.keys().cloned().collect::<Vec<_>>(),
                "text": format!("已加载 {} 个 Skill", self.skills.len()),
            }),
            presentation: Some(EventPresentation::divider(
                format!("已加载 {} 个 Skill", self.skills.len()),
                EventPresentationTone::Info,
            )),
        })?;
        self.public_tool_name = Some(public_tool_name);
        Ok(())
    }

    /// 移除动态贡献；Host 仍会在实例卸载时执行兜底清理。
    fn deactivate(&mut self, host: &dyn PluginHostApi) -> Result<()> {
        if let Some(public_name) = self.public_tool_name.take() {
            host.remove_tool(&public_name)?;
        }
        host.remove_prompt(SKILL_PROMPT_ID)?;
        host.remove_state("skills")?;
        self.skills.clear();
        Ok(())
    }

    /// 静态工具列表为空，读取工具在激活完成后通过 Host 动态注册。
    fn list_tools(&self) -> Vec<ToolSpec> {
        Vec::new()
    }

    /// 使用 Host 受控文件 API 返回模型选中的完整 Skill 指令。
    fn call_tool_with_host(
        &mut self,
        host: &dyn PluginHostApi,
        call: ToolCall,
    ) -> Result<ToolResult> {
        if call.name != READ_TOOL_LOCAL_NAME {
            return Ok(ToolResult::error(
                call.id,
                call.name,
                "Skill 插件收到未知工具调用",
            ));
        }
        let args: ReadSkillArgs = match call.args_as() {
            Ok(args) => args,
            Err(error) => {
                return Ok(ToolResult::error(
                    call.id,
                    call.name,
                    format!("Skill 参数无效：{error}"),
                ));
            }
        };
        let Some(skill) = self.skills.get(&args.name) else {
            return Ok(ToolResult::error(
                call.id,
                call.name,
                format!("未知 Skill：{}", args.name),
            ));
        };
        let content = match host.read_file(&skill.path) {
            Ok(content) => content,
            Err(error) => {
                return Ok(ToolResult::error(
                    call.id,
                    call.name,
                    format!("读取 Skill `{}` 失败：{error}", skill.name),
                ));
            }
        };
        host.emit_event(&ExtensionEvent {
            name: "skill.loaded".into(),
            data: json!({
                "name": skill.name,
                "path": skill.path,
                "text": format!("加载 Skill：{}", skill.name),
            }),
            presentation: Some(EventPresentation::divider(
                format!("加载 Skill：{}", skill.name),
                EventPresentationTone::Info,
            )),
        })?;
        Ok(ToolResult::success(
            call.id,
            call.name,
            json!({
                "name": skill.name,
                "description": skill.description,
                "content": content,
            }),
        ))
    }
}

/// 构建模型可调用的 Skill 完整指令读取工具。
fn skill_read_tool() -> ToolSpec {
    ToolSpec::new(
        "skill_read",
        "按名称读取一个已发现 Skill 的完整指令。仅在任务与 Skill 描述匹配时调用。",
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Skill 索引中列出的准确名称"
                }
            },
            "required": ["name"],
            "additionalProperties": false
        }),
    )
}

/// 生成只包含 Skill 名称和描述的 developer prompt。
fn build_skill_prompt(
    skills: &BTreeMap<String, SkillDescriptor>,
    public_tool_name: &str,
) -> String {
    if skills.is_empty() {
        return "当前没有发现可用 Skill。".to_string();
    }
    let index = skills
        .values()
        .map(|skill| {
            format!(
                "- `{}`：{}",
                skill.name,
                skill.description.replace('\n', " ")
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "当前可用 Skill：\n{index}\n当任务与某项描述匹配时，先调用 `{public_tool_name}` 读取完整指令，再按指令执行。不要在不匹配时加载 Skill。"
    )
}

/// 在受控目录内递归发现 `SKILL.md`，并限制扫描规模与深度。
fn discover_skill_paths(host: &dyn PluginHostApi, root: &str) -> Result<Vec<String>> {
    let mut pending = vec![(root.to_string(), 0usize)];
    let mut paths = Vec::new();
    let mut entries_seen = 0usize;

    while let Some((directory, depth)) = pending.pop() {
        let mut entries = host
            .list_dir(&directory)
            .with_context(|| format!("扫描 Skill 目录失败：{directory}"))?;
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        for entry in entries {
            entries_seen += 1;
            if entries_seen > MAX_SCAN_ENTRIES {
                return Err(anyhow!("Skill 扫描超过目录项上限 {MAX_SCAN_ENTRIES}"));
            }
            if entry.is_dir {
                if depth >= MAX_SCAN_DEPTH {
                    return Err(anyhow!("Skill 扫描超过递归深度上限 {MAX_SCAN_DEPTH}"));
                }
                pending.push((entry.path, depth + 1));
            } else if Path::new(&entry.path)
                .file_name()
                .and_then(|name| name.to_str())
                == Some("SKILL.md")
            {
                paths.push(entry.path);
            }
        }
    }
    paths.sort();
    Ok(paths)
}

/// 解析一个带 YAML frontmatter 的 `SKILL.md`。
///
/// frontmatter 支持普通字符串、单双引号字符串以及 `|`、`>` 多行标量；其他字段允许
/// 存在，但不会影响当前插件行为。
fn parse_skill(path: &str, source: &str) -> Result<SkillDescriptor> {
    if source.len() > MAX_SKILL_BYTES {
        return Err(anyhow!(
            "Skill 文件超过大小上限 {MAX_SKILL_BYTES} 字节：{path}"
        ));
    }
    let lines = source.lines().collect::<Vec<_>>();
    if lines.first().map(|line| line.trim()) != Some("---") {
        return Err(anyhow!("Skill 缺少 YAML frontmatter：{path}"));
    }
    let closing = lines
        .iter()
        .enumerate()
        .skip(1)
        .find_map(|(index, line)| (line.trim() == "---").then_some(index))
        .ok_or_else(|| anyhow!("Skill frontmatter 未闭合：{path}"))?;
    let fields = parse_frontmatter(&lines[1..closing])
        .with_context(|| format!("解析 Skill frontmatter 失败：{path}"))?;
    if !lines[closing + 1..]
        .iter()
        .any(|line| !line.trim().is_empty())
    {
        return Err(anyhow!("Skill 正文不能为空：{path}"));
    }

    let name = required_field(&fields, "name", path)?;
    validate_skill_name(&name).with_context(|| format!("Skill 名称无效：{path}"))?;
    let description = required_field(&fields, "description", path)?;
    if description.chars().count() > 1024 {
        return Err(anyhow!("Skill description 超过 1024 个字符：{path}"));
    }
    Ok(SkillDescriptor {
        name,
        description,
        path: path.to_string(),
    })
}

/// 解析 Skill frontmatter 中的字符串字段。
fn parse_frontmatter(lines: &[&str]) -> Result<BTreeMap<String, String>> {
    let mut fields = BTreeMap::new();
    let mut index = 0usize;
    while index < lines.len() {
        let line = lines[index].trim_end_matches('\r');
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            index += 1;
            continue;
        }
        if line.starts_with([' ', '\t']) {
            return Err(anyhow!("字段必须从行首开始：{line}"));
        }
        let (key, raw_value) = line
            .split_once(':')
            .ok_or_else(|| anyhow!("字段缺少冒号：{line}"))?;
        let key = key.trim();
        if key.is_empty() {
            return Err(anyhow!("frontmatter 字段名不能为空"));
        }
        let raw_value = raw_value.trim();
        let value = if raw_value.starts_with('|') || raw_value.starts_with('>') {
            let folded = raw_value.starts_with('>');
            index += 1;
            let mut block = Vec::new();
            while index < lines.len() {
                let block_line = lines[index].trim_end_matches('\r');
                if !block_line.trim().is_empty() && !block_line.starts_with([' ', '\t']) {
                    break;
                }
                block.push(block_line.trim_start().to_string());
                index += 1;
            }
            if folded {
                fold_block(&block)
            } else {
                block.join("\n").trim().to_string()
            }
        } else {
            index += 1;
            parse_scalar(raw_value)?
        };
        if fields.insert(key.to_string(), value).is_some() {
            return Err(anyhow!("frontmatter 字段重复：{key}"));
        }
    }
    Ok(fields)
}

/// 解析普通、单引号或双引号字符串标量。
fn parse_scalar(value: &str) -> Result<String> {
    if value.starts_with('"') {
        return serde_json::from_str(value).context("双引号字符串无效");
    }
    if value.starts_with('\'') {
        if value.len() < 2 || !value.ends_with('\'') {
            return Err(anyhow!("单引号字符串未闭合"));
        }
        return Ok(value[1..value.len() - 1].replace("''", "'"));
    }
    Ok(value.trim().to_string())
}

/// 按 YAML folded block 的常用语义折叠描述行，并保留空行分段。
fn fold_block(lines: &[String]) -> String {
    let mut output = String::new();
    for line in lines {
        if line.is_empty() {
            if !output.ends_with('\n') {
                output.push('\n');
            }
        } else {
            if !output.is_empty() && !output.ends_with('\n') {
                output.push(' ');
            }
            output.push_str(line);
        }
    }
    output.trim().to_string()
}

/// 读取并校验必填的非空 frontmatter 字段。
fn required_field(fields: &BTreeMap<String, String>, key: &str, path: &str) -> Result<String> {
    fields
        .get(key)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("Skill 缺少非空字段 `{key}`：{path}"))
}

/// 限制 Skill 名称为可稳定展示和作为工具参数传递的单行值。
fn validate_skill_name(name: &str) -> Result<()> {
    if name.chars().count() > 128 {
        return Err(anyhow!("名称超过 128 个字符"));
    }
    if matches!(name, "." | "..")
        || name
            .chars()
            .any(|character| character.is_control() || matches!(character, '/' | '\\'))
    {
        return Err(anyhow!("名称包含路径分隔符或控制字符"));
    }
    Ok(())
}

export_plugin!(SkillPlugin);

#[cfg(test)]
mod tests {
    use super::*;

    /// 验证标准 Skill frontmatter 只提取名称和描述。
    #[test]
    fn parses_standard_skill() {
        let skill = parse_skill(
            "skills/review/SKILL.md",
            "---\nname: code-review\ndescription: 审查代码风险。\n---\n\n# 审查\n\n检查行为回归。\n",
        )
        .expect("标准 Skill 应可解析");

        assert_eq!(skill.name, "code-review");
        assert_eq!(skill.description, "审查代码风险。");
        assert_eq!(skill.path, "skills/review/SKILL.md");
    }

    /// 验证 folded 多行描述与引号字符串可以稳定解析。
    #[test]
    fn parses_quoted_and_folded_metadata() {
        let skill = parse_skill(
            "skills/plugin/SKILL.md",
            "---\nname: 'plugin-dev'\ndescription: >-\n  开发 Lucia 插件时使用。\n  保持 crate 边界。\n---\n正文\n",
        )
        .expect("多行描述应可解析");

        assert_eq!(skill.name, "plugin-dev");
        assert_eq!(
            skill.description,
            "开发 Lucia 插件时使用。 保持 crate 边界。"
        );
    }

    /// 验证缺少 frontmatter、正文或必填字段时返回可定位错误。
    #[test]
    fn rejects_incomplete_skills() {
        assert!(parse_skill("SKILL.md", "# 无 frontmatter").is_err());
        assert!(parse_skill("SKILL.md", "---\nname: only-name\n---\n正文").is_err());
        assert!(parse_skill(
            "SKILL.md",
            "---\nname: empty-body\ndescription: 描述\n---\n"
        )
        .is_err());
    }
}

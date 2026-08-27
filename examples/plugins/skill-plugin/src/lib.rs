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
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::Path};

/// 插件内部动态工具 ID。
const READ_TOOL_LOCAL_NAME: &str = "read";
/// 注入 Agent 的 Skill 索引提示 ID。
const SKILL_PROMPT_ID: &str = "available-skills";
/// Evidence 装配层注入的版本化 Skill Set 元数据键。
const SKILL_SET_JSON_METADATA_KEY: &str = "skill_set_json";
/// 当前支持的 Genome Skill Set 信封版本。
const SKILL_SET_SCHEMA_VERSION: u32 = 1;
/// 当前支持的 SkillArtifact 版本。
const SKILL_ARTIFACT_SCHEMA_VERSION: u32 = 1;
/// 单次 Genome 最多装配的 Skill 数量。
const MAX_GENOME_SKILLS: usize = 256;
/// 单个 Skill 文件允许的最大字节数。
const MAX_SKILL_BYTES: usize = 1024 * 1024;
/// 一次激活允许扫描的最大目录项数。
const MAX_SCAN_ENTRIES: usize = 4096;
/// Skill 根目录下允许的最大递归深度。
const MAX_SCAN_DEPTH: usize = 8;

/// 已解析并可按需读取的 Skill 描述。
#[derive(Debug, Clone, PartialEq, Eq)]
struct SkillDescriptor {
    /// Genome 模式下的强类型稳定 ID；本地目录模式没有该字段。
    skill_id: Option<String>,
    /// Skill 的稳定名称。
    name: String,
    /// 注入 Agent 索引提示的简短说明。
    description: String,
    /// 完整指令的可信来源。
    source: SkillSource,
}

/// Skill 完整指令的来源，Genome 制品与兼容目录扫描互斥。
#[derive(Debug, Clone, PartialEq, Eq)]
enum SkillSource {
    /// 相对于插件目录的兼容 `SKILL.md` 路径。
    LocalFile { path: String },
    /// TUI 从真实 CAS 固定并由 Guest 复核摘要的制品正文。
    GenomeArtifact {
        /// 原 Candidate 的不可变 Genome Revision ID。
        genome_revision_id: String,
        /// 原 Candidate 的不可变 Genome 行为摘要。
        genome_digest: String,
        /// 规范 SkillArtifact JSON 的 SHA-256 摘要。
        artifact_digest: String,
        /// SkillArtifact 中的精确版本化指令。
        instructions: String,
    },
}

/// TUI 注入的版本化 Genome Skill Set 信封。
#[derive(Debug, Deserialize)]
struct GenomeSkillSetV1 {
    /// 信封结构版本。
    schema_version: u32,
    /// 原 Candidate 的不可变 Genome Revision ID。
    genome_revision_id: String,
    /// 原 Candidate 的不可变 Genome 行为摘要。
    genome_digest: String,
    /// 可信装配层固定的执行平面。
    execution_profile: String,
    /// 按 Skill ID 排序的精确制品引用。
    skills: Vec<InjectedSkillArtifactV1>,
}

/// 一项携带原始规范 JSON 的 Skill 制品。
#[derive(Debug, Deserialize)]
struct InjectedSkillArtifactV1 {
    /// Genome 与制品共同固定的 Skill ID。
    skill_id: String,
    /// 原始规范 JSON 的 SHA-256 摘要。
    artifact_digest: String,
    /// 来自 Artifact CAS 的原始规范 JSON。
    artifact_json: String,
}

/// Guest 执行所需的 SkillArtifact V1 兼容字段。
#[derive(Debug, Deserialize)]
struct SkillArtifactV1 {
    /// SkillArtifact 结构版本。
    schema_version: u32,
    /// 制品自身声明的稳定 Skill ID。
    skill_id: String,
    /// 面向模型的短名称。
    name: String,
    /// 用途说明。
    description: String,
    /// 按需读取时返回的完整指令。
    instructions: String,
    /// 完整状态链；只有终态 Active 可进入新运行。
    status_history: Vec<SkillStatusTransitionV1>,
}

/// Guest 只读取状态链终态所需的兼容记录。
#[derive(Debug, Deserialize)]
struct SkillStatusTransitionV1 {
    /// 小写下划线形式的生命周期状态。
    status: String,
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
    /// 优先消费可信 Genome Skill Set；未注入时扫描 manifest 配置目录以保持兼容。
    fn activate(&mut self, host: &dyn PluginHostApi, context: ActivationContext) -> Result<()> {
        self.skills =
            if let Some(skill_set_json) = context.metadata.get(SKILL_SET_JSON_METADATA_KEY) {
                parse_genome_skill_set(skill_set_json)?
            } else {
                let skills_dir = context
                    .metadata
                    .get("skills_dir")
                    .map(String::as_str)
                    .unwrap_or("skills");
                load_local_skills(host, skills_dir)?
            };

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

    /// 返回模型选中的完整 Skill 指令；Genome 模式不会再次读取插件目录。
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
        let (content, artifact_digest) = match &skill.source {
            SkillSource::LocalFile { path } => match host.read_file(path) {
                Ok(content) => (content, None),
                Err(error) => {
                    return Ok(ToolResult::error(
                        call.id,
                        call.name,
                        format!("读取 Skill `{}` 失败：{error}", skill.name),
                    ));
                }
            },
            SkillSource::GenomeArtifact {
                artifact_digest,
                instructions,
                ..
            } => (instructions.clone(), Some(artifact_digest.as_str())),
        };
        if let (Some(skill_id), Some(artifact_digest)) = (&skill.skill_id, artifact_digest) {
            let SkillSource::GenomeArtifact {
                genome_revision_id,
                genome_digest,
                ..
            } = &skill.source
            else {
                unreachable!("Genome Skill 必须携带 Genome 绑定")
            };
            host.emit_event(&ExtensionEvent {
                name: "skill.loaded.v1".into(),
                data: json!({
                    "schema_version": 1,
                    "skill_id": skill_id,
                    "artifact_digest": artifact_digest,
                    "genome_revision_id": genome_revision_id,
                    "genome_digest": genome_digest,
                    "call_id": call.id,
                    "text": format!("加载 Skill：{}", skill.name),
                }),
                presentation: Some(EventPresentation::divider(
                    format!("加载 Skill：{}", skill.name),
                    EventPresentationTone::Info,
                )),
            })?;
        } else {
            host.emit_event(&ExtensionEvent {
                name: "skill.loaded".into(),
                data: json!({
                    "name": skill.name,
                    "text": format!("加载 Skill：{}", skill.name),
                }),
                presentation: Some(EventPresentation::divider(
                    format!("加载 Skill：{}", skill.name),
                    EventPresentationTone::Info,
                )),
            })?;
        }
        Ok(ToolResult::success(
            call.id,
            call.name,
            json!({
                "skill_id": skill.skill_id,
                "artifact_digest": artifact_digest,
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

/// 从 Host 可信元数据解析 Genome Skill Set，并复核每项原始制品摘要与绑定关系。
///
/// # Errors
///
/// 信封版本或执行平面不受支持、Skill 未排序或重复、制品摘要不匹配、ID 错绑，或终态
/// 不允许进入当前执行平面时返回错误并阻止插件激活。
fn parse_genome_skill_set(source: &str) -> Result<BTreeMap<String, SkillDescriptor>> {
    let skill_set: GenomeSkillSetV1 =
        serde_json::from_str(source).context("解析 Genome Skill Set JSON 失败")?;
    if skill_set.schema_version != SKILL_SET_SCHEMA_VERSION {
        return Err(anyhow!(
            "Genome Skill Set 版本不受支持：{}",
            skill_set.schema_version
        ));
    }
    if skill_set.skills.len() > MAX_GENOME_SKILLS {
        return Err(anyhow!("Genome Skill Set 超过数量上限 {MAX_GENOME_SKILLS}"));
    }
    if !matches!(
        skill_set.execution_profile.as_str(),
        "serve" | "evaluation" | "mutation"
    ) {
        return Err(anyhow!(
            "Genome Skill Set 执行平面不受支持：{}",
            skill_set.execution_profile
        ));
    }
    if skill_set.genome_revision_id.trim().is_empty() {
        return Err(anyhow!("Genome Skill Set 缺少 genome_revision_id"));
    }
    validate_artifact_digest(&skill_set.genome_digest)
        .context("Genome Skill Set genome_digest 无效")?;

    let mut skills = BTreeMap::new();
    let mut previous_id: Option<&str> = None;
    for injected in &skill_set.skills {
        validate_skill_id(&injected.skill_id)?;
        if previous_id.is_some_and(|previous| previous >= injected.skill_id.as_str()) {
            return Err(anyhow!("Genome Skill Set 必须按 Skill ID 严格升序排列"));
        }
        validate_artifact_digest(&injected.artifact_digest)?;
        let actual_digest = format!("{:x}", Sha256::digest(injected.artifact_json.as_bytes()));
        if actual_digest != injected.artifact_digest {
            return Err(anyhow!(
                "Skill `{}` 制品摘要不匹配：期望 {}，实际 {actual_digest}",
                injected.skill_id,
                injected.artifact_digest
            ));
        }
        let artifact: SkillArtifactV1 = serde_json::from_str(&injected.artifact_json)
            .with_context(|| format!("解析 Skill `{}` 制品失败", injected.skill_id))?;
        if artifact.schema_version != SKILL_ARTIFACT_SCHEMA_VERSION {
            return Err(anyhow!(
                "Skill `{}` 制品版本不受支持：{}",
                injected.skill_id,
                artifact.schema_version
            ));
        }
        if artifact.skill_id != injected.skill_id {
            return Err(anyhow!(
                "Skill Set ID `{}` 与制品 ID `{}` 不一致",
                injected.skill_id,
                artifact.skill_id
            ));
        }
        let final_status = artifact
            .status_history
            .last()
            .map(|transition| transition.status.as_str());
        if !skill_status_is_loadable(&skill_set.execution_profile, final_status) {
            return Err(anyhow!(
                "Skill `{}` 终态 {final_status:?} 不能进入 {} 运行",
                injected.skill_id,
                skill_set.execution_profile
            ));
        }
        validate_skill_name(&artifact.name)
            .with_context(|| format!("Skill `{}` 名称无效", injected.skill_id))?;
        if artifact.description.trim().is_empty() || artifact.description.chars().count() > 4_096 {
            return Err(anyhow!("Skill `{}` description 无效", injected.skill_id));
        }
        if artifact.instructions.trim().is_empty() || artifact.instructions.len() > 65_536 {
            return Err(anyhow!("Skill `{}` instructions 无效", injected.skill_id));
        }

        let descriptor = SkillDescriptor {
            skill_id: Some(injected.skill_id.clone()),
            name: artifact.name,
            description: artifact.description,
            source: SkillSource::GenomeArtifact {
                genome_revision_id: skill_set.genome_revision_id.clone(),
                genome_digest: skill_set.genome_digest.clone(),
                artifact_digest: injected.artifact_digest.clone(),
                instructions: artifact.instructions,
            },
        };
        let name = descriptor.name.clone();
        if skills.insert(name.clone(), descriptor).is_some() {
            return Err(anyhow!("Genome Skill 名称重复：{name}"));
        }
        previous_id = Some(injected.skill_id.as_str());
    }
    Ok(skills)
}

/// 在 Guest 侧执行与可信装配层相同的 Skill 状态和运行平面门禁。
fn skill_status_is_loadable(profile: &str, status: Option<&str>) -> bool {
    match profile {
        "serve" => status == Some("active"),
        "evaluation" => matches!(status, Some("quarantined" | "evaluated" | "active")),
        "mutation" => false,
        _ => false,
    }
}

/// 在未注入 Genome Skill Set 时加载本地目录，保持既有安装模式兼容。
fn load_local_skills(
    host: &dyn PluginHostApi,
    skills_dir: &str,
) -> Result<BTreeMap<String, SkillDescriptor>> {
    let mut skills = BTreeMap::new();
    for path in discover_skill_paths(host, skills_dir)? {
        let source = host
            .read_file(&path)
            .with_context(|| format!("读取 Skill 失败：{path}"))?;
        let skill = parse_skill(&path, &source)?;
        if skills.insert(skill.name.clone(), skill).is_some() {
            return Err(anyhow!("Skill 名称重复：{path}"));
        }
    }
    Ok(skills)
}

/// 校验 Skill ID 的跨语言稳定形式。
fn validate_skill_id(skill_id: &str) -> Result<()> {
    let Some(body) = skill_id.strip_prefix("skill_") else {
        return Err(anyhow!("Skill ID 前缀无效：{skill_id}"));
    };
    if !(8..=64).contains(&body.len())
        || !body
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        return Err(anyhow!("Skill ID 格式无效：{skill_id}"));
    }
    Ok(())
}

/// 校验 ArtifactDigest 的小写 SHA-256 十六进制形式。
fn validate_artifact_digest(digest: &str) -> Result<()> {
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(anyhow!("Skill 制品摘要格式无效：{digest}"));
    }
    Ok(())
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
        skill_id: None,
        name,
        description,
        source: SkillSource::LocalFile {
            path: path.to_string(),
        },
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
        assert_eq!(
            skill.source,
            SkillSource::LocalFile {
                path: "skills/review/SKILL.md".into()
            }
        );
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

    /// Genome Skill Set 必须复核摘要、ID 和 Active 终态后才能提供精确指令。
    #[test]
    fn parses_verified_active_genome_skill_set() {
        let artifact_json = json!({
            "schema_version": 1,
            "skill_id": "skill_verified1",
            "name": "verified-skill",
            "description": "只使用固定制品。",
            "instructions": "来自真实 CAS 的精确指令。",
            "status_history": [{"status": "active"}]
        })
        .to_string();
        let digest = format!("{:x}", Sha256::digest(artifact_json.as_bytes()));
        let source = json!({
            "schema_version": 1,
            "genome_revision_id": "grev_verified1",
            "genome_digest": "1".repeat(64),
            "execution_profile": "serve",
            "skills": [{
                "skill_id": "skill_verified1",
                "artifact_digest": digest,
                "artifact_json": artifact_json
            }]
        })
        .to_string();

        let skills = parse_genome_skill_set(&source).expect("Active Skill Set 应通过校验");
        let skill = skills.get("verified-skill").expect("应装配固定 Skill");
        assert_eq!(skill.skill_id.as_deref(), Some("skill_verified1"));
        assert!(matches!(
            &skill.source,
            SkillSource::GenomeArtifact { instructions, .. }
                if instructions == "来自真实 CAS 的精确指令。"
        ));
    }

    /// Genome Skill Set 摘要不符或终态非 Active 时必须失败关闭。
    #[test]
    fn rejects_tampered_or_inactive_genome_skill_set() {
        let artifact_json = json!({
            "schema_version": 1,
            "skill_id": "skill_rejected1",
            "name": "rejected-skill",
            "description": "不应装配。",
            "instructions": "不应返回。",
            "status_history": [{"status": "quarantined"}]
        })
        .to_string();
        let digest = format!("{:x}", Sha256::digest(artifact_json.as_bytes()));
        let inactive = json!({
            "schema_version": 1,
            "genome_revision_id": "grev_rejected1",
            "genome_digest": "2".repeat(64),
            "execution_profile": "serve",
            "skills": [{
                "skill_id": "skill_rejected1",
                "artifact_digest": digest,
                "artifact_json": artifact_json
            }]
        })
        .to_string();
        assert!(parse_genome_skill_set(&inactive).is_err());

        let tampered = json!({
            "schema_version": 1,
            "genome_revision_id": "grev_rejected1",
            "genome_digest": "2".repeat(64),
            "execution_profile": "serve",
            "skills": [{
                "skill_id": "skill_rejected1",
                "artifact_digest": "0".repeat(64),
                "artifact_json": artifact_json
            }]
        })
        .to_string();
        assert!(parse_genome_skill_set(&tampered).is_err());
    }

    /// Evaluation 平面可装载隔离或已评测候选，但 Mutation 平面不运行 Skill。
    #[test]
    fn evaluation_allows_candidate_statuses_but_mutation_rejects_them() {
        for status in ["quarantined", "evaluated"] {
            let artifact_json = json!({
                "schema_version": 1,
                "skill_id": "skill_candidate1",
                "name": "candidate-skill",
                "description": "只用于隔离评测。",
                "instructions": "运行确定性评测指令。",
                "status_history": [{"status": status}]
            })
            .to_string();
            let digest = format!("{:x}", Sha256::digest(artifact_json.as_bytes()));
            let source = json!({
                "schema_version": 1,
                "genome_revision_id": "grev_candidate1",
                "genome_digest": "3".repeat(64),
                "execution_profile": "evaluation",
                "skills": [{
                    "skill_id": "skill_candidate1",
                    "artifact_digest": digest,
                    "artifact_json": artifact_json
                }]
            });
            assert!(parse_genome_skill_set(&source.to_string()).is_ok());

            let mut mutation = source;
            mutation["execution_profile"] = json!("mutation");
            assert!(parse_genome_skill_set(&mutation.to_string()).is_err());
        }
    }
}

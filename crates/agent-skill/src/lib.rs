//! Lucia 原生 Skill 发现、索引和按需读取能力。
//!
//! 本 crate 不依赖 Plugin Host。应用装配层负责提供可信本地目录或已经完成 CAS 校验的
//! Genome Skill；本 crate 负责稳定解析、名称去重、模型提示和原生工具执行。

#![deny(missing_docs)]

use agent_evolution_protocol::{ArtifactDigest, GenomeDigest, GenomeRevisionId, SkillArtifactV1};
use agent_tool::{Tool, ToolCall, ToolRegistry, ToolResult, ToolSpec};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

/// 原生 Skill 读取工具的稳定名称。
pub const SKILL_READ_TOOL_NAME: &str = "skill_read";

const MAX_SCAN_DEPTH: usize = 8;
const MAX_SCAN_ENTRIES: usize = 4_096;
const MAX_SKILL_BYTES: usize = 256 * 1024;

/// 一份由可信装配层校验并固定到 Genome 的 Skill。
#[derive(Debug, Clone)]
pub struct GenomeSkillBinding {
    /// Skill 的规范 CAS 摘要。
    pub artifact_digest: ArtifactDigest,
    /// 本次运行固定的 Genome 修订号。
    pub genome_revision_id: GenomeRevisionId,
    /// 本次运行固定的 Genome 内容摘要。
    pub genome_digest: GenomeDigest,
    /// 已从 CAS 读取并完成协议校验的 Skill 制品。
    pub artifact: SkillArtifactV1,
}

/// 原生 Skill 目录；名称唯一且按字典序稳定排列。
#[derive(Debug, Clone, Default)]
pub struct SkillCatalog {
    skills: BTreeMap<String, SkillDescriptor>,
}

impl SkillCatalog {
    /// 创建空 Skill 目录。
    pub fn new() -> Self {
        Self::default()
    }

    /// 扫描多个本地 Skill 根目录；不存在的根目录被视为空目录。
    ///
    /// 根目录按传入顺序扫描，但最终索引按名称排序。为避免目录逃逸，符号链接不会被读取；
    /// 任意文件无效、名称重复、扫描深度或目录项数量超限时返回错误且不返回部分目录。
    ///
    /// # Errors
    ///
    /// 目录无法读取、`SKILL.md` 格式无效或扫描边界被突破时返回错误。
    pub fn load_local(roots: impl IntoIterator<Item = PathBuf>) -> Result<Self> {
        let mut catalog = Self::new();
        for root in roots {
            for path in discover_skill_paths(&root)? {
                let source = fs::read_to_string(&path)
                    .with_context(|| format!("读取 Skill 失败：{}", path.display()))?;
                let descriptor = parse_skill(&path, &source)?;
                catalog.insert(descriptor)?;
            }
        }
        Ok(catalog)
    }

    /// 从已经由可信装配层绑定的 Genome Skill 构造目录。
    ///
    /// 本函数仍会复核制品自身协议、摘要和名称唯一性，防止调用方把错绑对象交给原生工具。
    ///
    /// # Errors
    ///
    /// 制品无效、摘要错绑或名称重复时返回错误。
    pub fn from_genome(bindings: impl IntoIterator<Item = GenomeSkillBinding>) -> Result<Self> {
        let mut catalog = Self::new();
        for binding in bindings {
            binding
                .artifact
                .validate()
                .map_err(|error| anyhow!("Genome Skill 制品无效：{error}"))?;
            let actual = binding
                .artifact
                .digest()
                .map_err(|error| anyhow!("计算 Genome Skill 摘要失败：{error}"))?;
            if actual != binding.artifact_digest {
                return Err(anyhow!(
                    "Genome Skill `{}` 摘要错绑：期望 {}，实际 {}",
                    binding.artifact.skill_id,
                    binding.artifact_digest,
                    actual
                ));
            }
            let descriptor = SkillDescriptor {
                skill_id: Some(binding.artifact.skill_id.to_string()),
                name: binding.artifact.name,
                description: binding.artifact.description,
                source: SkillSource::Genome {
                    instructions: binding.artifact.instructions,
                    artifact_digest: binding.artifact_digest,
                    genome_revision_id: binding.genome_revision_id,
                    genome_digest: binding.genome_digest,
                },
            };
            validate_skill_name(&descriptor.name)?;
            catalog.insert(descriptor)?;
        }
        Ok(catalog)
    }

    /// 返回目录中 Skill 的数量。
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// 判断目录是否为空。
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// 生成只包含名称和描述的模型提示；空目录不产生提示。
    pub fn prompt(&self) -> Option<String> {
        if self.skills.is_empty() {
            return None;
        }
        let index = self
            .skills
            .values()
            .map(|skill| format!("- {}：{}", skill.name, skill.description))
            .collect::<Vec<_>>()
            .join("\n");
        Some(format!(
            "当前可用 Skill：\n{index}\n当任务与某项描述匹配时，先调用 `{SKILL_READ_TOOL_NAME}` 读取完整指令，再按指令执行。不要在不匹配时加载 Skill。"
        ))
    }

    /// 把 `skill_read` 注册为宿主原生工具；空目录不注册工具。
    ///
    /// # Errors
    ///
    /// 注册表已经存在同名工具或工具定义无效时返回错误。
    pub fn register_tool(&self, registry: &mut ToolRegistry) -> Result<()> {
        if self.skills.is_empty() {
            return Ok(());
        }
        registry.register(SkillReadTool {
            skills: self.skills.clone(),
        })?;
        Ok(())
    }

    fn insert(&mut self, descriptor: SkillDescriptor) -> Result<()> {
        let name = descriptor.name.clone();
        if self.skills.insert(name.clone(), descriptor).is_some() {
            return Err(anyhow!("Skill 名称重复：{name}"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct SkillDescriptor {
    skill_id: Option<String>,
    name: String,
    description: String,
    source: SkillSource,
}

#[derive(Debug, Clone)]
enum SkillSource {
    LocalFile(PathBuf),
    Genome {
        instructions: String,
        artifact_digest: ArtifactDigest,
        genome_revision_id: GenomeRevisionId,
        genome_digest: GenomeDigest,
    },
}

#[derive(Debug, Deserialize)]
struct ReadSkillArgs {
    name: String,
}

#[derive(Debug, Clone)]
struct SkillReadTool {
    skills: BTreeMap<String, SkillDescriptor>,
}

#[async_trait]
impl Tool for SkillReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            SKILL_READ_TOOL_NAME,
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

    async fn call(&self, call: ToolCall) -> Result<ToolResult> {
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
        let (content, usage) = match &skill.source {
            SkillSource::LocalFile(path) => match fs::read_to_string(path) {
                Ok(content) => (content, None),
                Err(error) => {
                    return Ok(ToolResult::error(
                        call.id,
                        call.name,
                        format!("读取 Skill `{}` 失败：{error}", skill.name),
                    ));
                }
            },
            SkillSource::Genome {
                instructions,
                artifact_digest,
                genome_revision_id,
                genome_digest,
            } => (
                instructions.clone(),
                Some(json!({
                    "schema_version": 1,
                    "skill_id": skill.skill_id,
                    "artifact_digest": artifact_digest,
                    "genome_revision_id": genome_revision_id,
                    "genome_digest": genome_digest,
                })),
            ),
        };
        let mut result = ToolResult::success(
            call.id,
            call.name,
            json!({
                "skill_id": skill.skill_id,
                "name": skill.name,
                "description": skill.description,
                "content": content,
            }),
        );
        if let Some(usage) = usage {
            result = result.with_details(json!({ "skill_usage": usage }));
        }
        Ok(result)
    }
}

fn discover_skill_paths(root: &Path) -> Result<Vec<PathBuf>> {
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("读取 Skill 根目录失败：{}", root.display()))
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(anyhow!(
            "Skill 根目录必须是非符号链接目录：{}",
            root.display()
        ));
    }

    let mut pending = vec![(root.to_path_buf(), 0usize)];
    let mut paths = Vec::new();
    let mut entries_seen = 0usize;
    while let Some((directory, depth)) = pending.pop() {
        let mut entries = fs::read_dir(&directory)
            .with_context(|| format!("扫描 Skill 目录失败：{}", directory.display()))?
            .collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            entries_seen = entries_seen.saturating_add(1);
            if entries_seen > MAX_SCAN_ENTRIES {
                return Err(anyhow!("Skill 扫描超过目录项上限 {MAX_SCAN_ENTRIES}"));
            }
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                if depth >= MAX_SCAN_DEPTH {
                    return Err(anyhow!("Skill 扫描超过递归深度上限 {MAX_SCAN_DEPTH}"));
                }
                pending.push((entry.path(), depth + 1));
            } else if file_type.is_file() && entry.file_name() == "SKILL.md" {
                paths.push(entry.path());
            }
        }
    }
    paths.sort();
    Ok(paths)
}

fn parse_skill(path: &Path, source: &str) -> Result<SkillDescriptor> {
    if source.len() > MAX_SKILL_BYTES {
        return Err(anyhow!(
            "Skill 文件超过大小上限 {MAX_SKILL_BYTES} 字节：{}",
            path.display()
        ));
    }
    let lines = source.lines().collect::<Vec<_>>();
    if lines.first().map(|line| line.trim()) != Some("---") {
        return Err(anyhow!("Skill 缺少 YAML frontmatter：{}", path.display()));
    }
    let closing = lines
        .iter()
        .enumerate()
        .skip(1)
        .find_map(|(index, line)| (line.trim() == "---").then_some(index))
        .ok_or_else(|| anyhow!("Skill frontmatter 未闭合：{}", path.display()))?;
    if !lines[closing + 1..]
        .iter()
        .any(|line| !line.trim().is_empty())
    {
        return Err(anyhow!("Skill 正文不能为空：{}", path.display()));
    }
    let fields = parse_frontmatter(&lines[1..closing])?;
    let name = required_field(&fields, "name", path)?;
    validate_skill_name(&name)?;
    let description = required_field(&fields, "description", path)?;
    if description.chars().count() > 1_024 {
        return Err(anyhow!(
            "Skill description 超过 1024 个字符：{}",
            path.display()
        ));
    }
    Ok(SkillDescriptor {
        skill_id: None,
        name,
        description,
        source: SkillSource::LocalFile(path.to_path_buf()),
    })
}

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
            return Err(anyhow!("frontmatter 字段必须从行首开始：{line}"));
        }
        let (key, raw_value) = line
            .split_once(':')
            .ok_or_else(|| anyhow!("frontmatter 字段缺少冒号：{line}"))?;
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
                block.join(" ").trim().to_string()
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

fn parse_scalar(raw: &str) -> Result<String> {
    if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
        return serde_json::from_str(raw).context("解析双引号 frontmatter 字符串失败");
    }
    if raw.len() >= 2 && raw.starts_with('\'') && raw.ends_with('\'') {
        return Ok(raw[1..raw.len() - 1].replace("''", "'"));
    }
    Ok(raw.trim().to_string())
}

fn required_field(
    fields: &BTreeMap<String, String>,
    key: &'static str,
    path: &Path,
) -> Result<String> {
    fields
        .get(key)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("Skill 缺少 {key}：{}", path.display()))
}

fn validate_skill_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 128
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(anyhow!("Skill 名称无效：{name}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_tool::ToolCall;

    /// 本地 Skill 应进入提示并由原生工具按需读取完整正文。
    #[tokio::test]
    async fn loads_local_skill_and_reads_content() {
        let root = std::env::temp_dir().join(format!("lucia-native-skill-{}", std::process::id()));
        let skill_dir = root.join("review");
        fs::create_dir_all(&skill_dir).expect("应创建 Skill 测试目录");
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: review\ndescription: 审查代码时使用。\n---\n\n# 审查\n只报告证据。\n",
        )
        .expect("应写入 Skill 测试文件");

        let catalog = SkillCatalog::load_local([root.clone()]).expect("应加载本地 Skill");
        assert!(catalog.prompt().expect("应生成提示").contains("review"));
        let tool = SkillReadTool {
            skills: catalog.skills,
        };
        let result = tool
            .call(ToolCall::new(
                "call-1",
                SKILL_READ_TOOL_NAME,
                json!({"name": "review"}),
            ))
            .await
            .expect("原生 Skill 工具应执行");
        assert!(!result.is_error);
        assert!(result.content["content"]
            .as_str()
            .expect("应返回正文")
            .contains("只报告证据"));

        fs::remove_dir_all(root).expect("应清理 Skill 测试目录");
    }
}

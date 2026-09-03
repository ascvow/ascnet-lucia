//! 项目约定文件（`AGENTS.md`）的发现与加载。
//!
//! 约定写在仓库里而不是每次对话里复述：Agent 启动时把它们并入系统提示，模型因此在第一次
//! 请求就知道该项目的构建命令、代码风格和禁止事项。内容进入 system 段，也就一并进入提示
//! 缓存前缀，不会每轮重复计费。

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// 项目约定文件名。
const INSTRUCTIONS_FILE: &str = "AGENTS.md";

/// 单个约定文件允许注入的最大字节数。
///
/// 约定文件偶尔会被当成长文档使用；超出部分截断而不是整份丢弃，既守住上下文预算，也让
/// 用户仍能看到前半部分生效。
const MAX_FILE_BYTES: usize = 32 * 1024;

/// 全部约定文件合计允许注入的最大字节数。
const MAX_TOTAL_BYTES: usize = 64 * 1024;

/// 从当前目录向上回溯的最大层数，避免在深层路径下扫描过多目录。
const MAX_ANCESTOR_DEPTH: usize = 16;

/// 一份已加载的约定文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectInstructions {
    /// 来源路径，用于在提示中标注约定出处。
    pub(crate) path: PathBuf,
    /// 文件正文，可能已按上限截断。
    pub(crate) content: String,
}

/// 按优先级从低到高收集约定文件。
///
/// 顺序是「全局 → 外层目录 → 内层目录」：越靠近工作目录的约定越具体，排在后面，模型读到
/// 冲突指令时以最后出现的为准。`lucia_home` 下的约定对所有项目生效。
///
/// 找不到任何文件时返回空列表；单个文件读取失败会向上报错，因为静默忽略会让用户以为约定
/// 已经生效。
pub(crate) fn load_project_instructions(
    lucia_home: &Path,
    cwd: &Path,
) -> Result<Vec<ProjectInstructions>> {
    let mut candidates = Vec::new();
    let global = lucia_home.join(INSTRUCTIONS_FILE);
    if global.is_file() {
        candidates.push(global);
    }

    // 先自内向外收集祖先目录，再整体反转成自外向内，保证内层覆盖外层。
    let mut ancestors = Vec::new();
    for directory in cwd.ancestors().take(MAX_ANCESTOR_DEPTH) {
        let path = directory.join(INSTRUCTIONS_FILE);
        if path.is_file() {
            ancestors.push(path);
        }
    }
    ancestors.reverse();
    candidates.extend(ancestors);
    candidates.dedup();

    let mut loaded = Vec::new();
    let mut total = 0usize;
    for path in candidates {
        if total >= MAX_TOTAL_BYTES {
            break;
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("读取项目约定失败：{}", path.display()))?;
        if raw.trim().is_empty() {
            continue;
        }
        let remaining = MAX_TOTAL_BYTES - total;
        let content = truncate_on_char_boundary(&raw, MAX_FILE_BYTES.min(remaining));
        total += content.len();
        loaded.push(ProjectInstructions { path, content });
    }
    Ok(loaded)
}

/// 把约定渲染成追加到系统提示的段落；没有约定时返回 `None`。
///
/// 明确标注这些内容来自项目文件且优先级高于通用默认行为，同时说明它们是项目配置而不是
/// 用户在对话中下达的新指令。
pub(crate) fn render_instructions_prompt(instructions: &[ProjectInstructions]) -> Option<String> {
    if instructions.is_empty() {
        return None;
    }
    let mut prompt = String::from(
        "以下是当前项目的约定文件内容。它们由项目维护者提供，优先级高于你的通用默认做法；\
         多份约定按从外到内排列，靠后的更具体。请在本次会话中持续遵守。\n",
    );
    for entry in instructions {
        prompt.push_str(&format!(
            "\n<project-instructions path=\"{}\">\n{}\n</project-instructions>\n",
            entry.path.display(),
            entry.content.trim_end()
        ));
    }
    Some(prompt)
}

/// 按 UTF-8 字符边界截断到不超过给定字节数。
fn truncate_on_char_boundary(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…（约定文件过长，其余内容已省略）", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// 建立一个隔离的测试目录树。
    fn temp_root(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "lucia-instructions-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).expect("创建测试目录");
        base
    }

    /// 内层目录的约定排在外层之后，全局约定排在最前。
    #[test]
    fn instructions_are_ordered_from_global_to_innermost() {
        let root = temp_root("order");
        let home = root.join("home");
        let outer = root.join("project");
        let inner = outer.join("crates/app");
        fs::create_dir_all(&home).expect("创建 home");
        fs::create_dir_all(&inner).expect("创建内层目录");
        fs::write(home.join(INSTRUCTIONS_FILE), "全局约定").expect("写入全局约定");
        fs::write(outer.join(INSTRUCTIONS_FILE), "项目约定").expect("写入项目约定");
        fs::write(inner.join(INSTRUCTIONS_FILE), "子目录约定").expect("写入子目录约定");

        let loaded = load_project_instructions(&home, &inner).expect("加载约定应成功");

        let contents: Vec<&str> = loaded.iter().map(|entry| entry.content.as_str()).collect();
        assert_eq!(contents, vec!["全局约定", "项目约定", "子目录约定"]);
    }

    /// 没有约定文件时返回空列表，且不产生提示段落。
    #[test]
    fn missing_files_produce_no_prompt() {
        let root = temp_root("missing");
        let cwd = root.join("empty");
        fs::create_dir_all(&cwd).expect("创建工作目录");

        let loaded = load_project_instructions(&root, &cwd).expect("加载约定应成功");

        assert!(loaded.is_empty());
        assert!(render_instructions_prompt(&loaded).is_none());
    }

    /// 空白内容的约定文件不会占用提示预算。
    #[test]
    fn blank_files_are_skipped() {
        let root = temp_root("blank");
        let cwd = root.join("project");
        fs::create_dir_all(&cwd).expect("创建工作目录");
        fs::write(cwd.join(INSTRUCTIONS_FILE), "   \n\n").expect("写入空白约定");

        let loaded = load_project_instructions(&root, &cwd).expect("加载约定应成功");

        assert!(loaded.is_empty());
    }

    /// 超长约定按字符边界截断并给出说明，不会整份丢弃。
    #[test]
    fn oversized_file_is_truncated_at_char_boundary() {
        let root = temp_root("oversize");
        let cwd = root.join("project");
        fs::create_dir_all(&cwd).expect("创建工作目录");
        // 使用多字节字符确保截断点必须回退到字符边界。
        fs::write(cwd.join(INSTRUCTIONS_FILE), "约".repeat(MAX_FILE_BYTES)).expect("写入超长约定");

        let loaded = load_project_instructions(&root, &cwd).expect("加载约定应成功");

        let content = &loaded[0].content;
        assert!(content.len() <= MAX_FILE_BYTES + 64, "{}", content.len());
        assert!(content.ends_with("其余内容已省略）"), "{content}");
    }

    /// 渲染结果标注来源路径，并声明约定的优先级。
    #[test]
    fn prompt_marks_source_and_priority() {
        let instructions = vec![ProjectInstructions {
            path: PathBuf::from("/tmp/project/AGENTS.md"),
            content: "使用简体中文注释".into(),
        }];

        let prompt = render_instructions_prompt(&instructions).expect("应生成提示");

        assert!(prompt.contains("优先级高于你的通用默认做法"), "{prompt}");
        assert!(
            prompt.contains("path=\"/tmp/project/AGENTS.md\""),
            "{prompt}"
        );
        assert!(prompt.contains("使用简体中文注释"), "{prompt}");
    }
}

//! 统一 diff（unified diff）的行分类与配色。
//!
//! 工具预览（`ui::transcript`）与 Markdown 围栏代码块（`ui::markdown`）都要渲染 diff
//! 文本。若两边各自维护"哪一行算新增"的判断，规则迟早漂移成两套语义，同一个补丁在
//! 两种位置会出现不同的颜色。因此判定（[`classify`]、[`looks_like_diff`]）与配色
//! （[`style_of`]）集中在本模块，渲染层只负责排版：留白、行号列与整行底色的铺法。

use crate::theme::{
    COLOR_DIFF_ADD_BG, COLOR_DIFF_ADD_FG, COLOR_DIFF_DEL_BG, COLOR_DIFF_DEL_FG,
    COLOR_DIFF_FILE_HEADER, COLOR_DIFF_HUNK, COLOR_MUTED, COLOR_TEXT,
};
use ratatui::style::Color;

/// 统一 diff 中一行的语义类别。
///
/// 类别只描述"这一行在补丁里的角色"，与具体配色解耦：主题调整颜色时不需要改判定，
/// 渲染层新增卡片样式时也不需要重复匹配字符串前缀。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiffLineKind {
    /// 文件级头部：`diff --git`、`index`、`---`、`+++`、`new file mode` 等。
    FileHeader,
    /// hunk 头：`@@ -1,7 +1,9 @@`。
    HunkHeader,
    /// 新增行；不含 `+++` 这一文件头。
    Added,
    /// 删除行；不含 `---` 这一文件头。
    Removed,
    /// 上下文行，以及 hunk 内无法归类的其他行。
    Context,
    /// 补丁附注：例如 `\ No newline at end of file`。
    Meta,
}

/// 一类 diff 行的配色。
///
/// `bg` 为 `None` 表示沿用终端的默认背景：文件头、hunk 头与上下文行不铺底色，只有
/// 增删行形成整行色块，色块本身就是"这里改了东西"的信号。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DiffRowStyle {
    /// 文字颜色。
    pub(crate) fg: Color,
    /// 整行背景色；`None` 表示不铺底色。
    pub(crate) bg: Option<Color>,
    /// 是否加粗；只用于文件头与 hunk 头这类结构信息。
    pub(crate) bold: bool,
}

impl DiffRowStyle {
    /// 构造一条"不铺底色、不加粗"的配色。
    const fn plain(fg: Color) -> Self {
        Self {
            fg,
            bg: None,
            bold: false,
        }
    }
}

/// 查询某一类行的配色。
pub(crate) fn style_of(kind: DiffLineKind) -> DiffRowStyle {
    match kind {
        // 文件头用柔和的青蓝：读者需要它来定位"改的是哪个文件"，但它不是改动本身，
        // 因此不铺底色，只用加粗把层级抬起来。
        DiffLineKind::FileHeader => DiffRowStyle {
            fg: COLOR_DIFF_FILE_HEADER,
            bg: None,
            bold: true,
        },
        // hunk 头用青色并加粗：它是"跳到下一段改动"的锚点。
        DiffLineKind::HunkHeader => DiffRowStyle {
            fg: COLOR_DIFF_HUNK,
            bg: None,
            bold: true,
        },
        DiffLineKind::Added => DiffRowStyle {
            fg: COLOR_DIFF_ADD_FG,
            bg: Some(COLOR_DIFF_ADD_BG),
            bold: false,
        },
        DiffLineKind::Removed => DiffRowStyle {
            fg: COLOR_DIFF_DEL_FG,
            bg: Some(COLOR_DIFF_DEL_BG),
            bold: false,
        },
        DiffLineKind::Context => DiffRowStyle::plain(COLOR_TEXT),
        DiffLineKind::Meta => DiffRowStyle::plain(COLOR_MUTED),
    }
}

/// 判断一行是否是文件级头部。
///
/// 覆盖 git 信封（`diff --git`、`index`、`similarity index`、模式与重命名行）以及补丁
/// 自身的 `---`/`+++` 文件行。
fn is_file_header(row: &str) -> bool {
    const PREFIXES: [&str; 13] = [
        "diff --git ",
        "diff --cc ",
        "diff --combined ",
        "index ",
        "--- ",
        "+++ ",
        "new file mode ",
        "deleted file mode ",
        "old mode ",
        "new mode ",
        "similarity index ",
        "rename from ",
        "rename to ",
    ];
    PREFIXES.iter().any(|prefix| row.starts_with(prefix))
}

/// 判断一行是否是 hunk 头。
///
/// 形状是 `@@ -旧起点,旧行数 +新起点,新行数 @@`，`@@` 之后允许跟一段函数或类名上下文
/// （git 会写 `@@ -10,7 +10,9 @@ fn main() {`）。因此判据是：以 `@@ -` 开头，并且
/// 在范围段之后还能找到收尾的 ` @@`；只检查前缀会把正文里以 `@@` 开头的行也当成结构行。
fn is_hunk_header(row: &str) -> bool {
    let Some(rest) = row.strip_prefix("@@ ") else {
        return false;
    };
    let Some((ranges, _context)) = rest.split_once(" @@") else {
        return false;
    };
    // 范围段必须由 `-旧,+新` 两段数字构成，避免把正文里的 `@@ 说明 @@` 误判成 hunk 头。
    ranges.starts_with('-')
        && ranges.contains(" +")
        && ranges
            .chars()
            .all(|ch| ch.is_ascii_digit() || matches!(ch, '-' | '+' | ',' | ' '))
}

/// 逐行分类整段 diff，并自动维护"是否已进入 hunk"的状态。
///
/// 渲染层需要逐行取类别，而 hunk 状态必须跨行推进；把状态机放在这里，调用方就不必
/// 各自复制一遍"遇到 hunk 头要翻位"的逻辑——那正是两份实现漂移的起点。
pub(crate) fn classify_rows(rows: &[&str]) -> Vec<DiffLineKind> {
    let mut in_hunk = false;
    rows.iter()
        .map(|row| {
            let kind = classify(row, in_hunk);
            if kind == DiffLineKind::HunkHeader {
                in_hunk = true;
            }
            kind
        })
        .collect()
}

/// 对 diff 中的一行分类。
///
/// `in_hunk` 表示当前是否已经进入某个 hunk。这个状态是必要的：hunk 内部被删除的一行
/// 如果正文本身以 `--` 开头，渲染出来就是 `--- ...`，与补丁的 `--- a/file` 文件头
/// 逐字节相同，只有"是否已在 hunk 内"能区分二者。
pub(crate) fn classify(row: &str, in_hunk: bool) -> DiffLineKind {
    // hunk 之外才可能是文件头；hunk 内的 `---`/`+++` 属于内容而不是结构。
    if !in_hunk && is_file_header(row) {
        return DiffLineKind::FileHeader;
    }
    if is_hunk_header(row) {
        return DiffLineKind::HunkHeader;
    }
    if row.starts_with('+') {
        return DiffLineKind::Added;
    }
    if row.starts_with('-') {
        return DiffLineKind::Removed;
    }
    if row.starts_with('\\') {
        // `\ No newline at end of file` 一类的附注，不是改动内容。
        return DiffLineKind::Meta;
    }
    DiffLineKind::Context
}

/// 判断一段文本是否应按统一 diff 渲染。
///
/// 三条判据按可靠性排序，任何一条命中即认为是 diff：
///
/// 1. 同时出现 hunk 头与文件标记——这是 `git diff`/`git show` 的标准形状。旧实现要求
///    增删行占到一半，但真实的 `git diff` 头部行加上下文行往往远多于改动行，判据因此
///    几乎总是落空，整个补丁被当成普通正文渲染成单色。
/// 2. 首两行是 `--- `/`+++ ` 文件头——没有 git 信封的裸补丁。
/// 3. 没有结构行时退回"增删行占多数"的保守判据，覆盖 `apply_patch` 这类片段化输入。
pub(crate) fn looks_like_diff(rows: &[&str]) -> bool {
    let has_hunk = rows.iter().any(|row| is_hunk_header(row));
    let has_file_marker = rows.iter().any(|row| is_file_header(row));
    if has_hunk && has_file_marker {
        return true;
    }
    if rows.first().is_some_and(|row| row.starts_with("--- "))
        && rows.get(1).is_some_and(|row| row.starts_with("+++ "))
    {
        return true;
    }
    let marked = rows
        .iter()
        .filter(|row| {
            (row.starts_with('+') || row.starts_with('-'))
                && !row.starts_with("+++")
                && !row.starts_with("---")
        })
        .count();
    marked > 0 && marked * 2 >= rows.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实 `git diff` 的头部与上下文行远多于改动行，必须仍被识别为 diff。
    ///
    /// 这是渲染成单色的直接原因：旧判据要求增删行占一半，标准补丁永远达不到。
    #[test]
    fn git_diff_with_many_context_rows_is_recognized() {
        let rows = [
            "diff --git a/src/main.rs b/src/main.rs",
            "index 83f2a1b..9c31d4e 100644",
            "--- a/src/main.rs",
            "+++ b/src/main.rs",
            "@@ -10,7 +10,9 @@ fn main() {",
            "     let a = 1;",
            "     let b = 2;",
            "-    let c = 3;",
            "+    let c = 4;",
            "+    let c5 = 5;",
            "     let d = 6;",
            "     let e = 7;",
            "     let f = 8;",
        ];

        assert!(looks_like_diff(&rows));
    }

    /// 片段化的补丁没有结构行，退回"增删行占多数"的判据。
    #[test]
    fn fragment_without_headers_still_counts_as_diff() {
        assert!(looks_like_diff(&["-old line", "+new line", " context"]));
    }

    /// 普通输出里偶然出现的 `+`/`-` 前缀不应被当成 diff。
    #[test]
    fn plain_output_is_not_a_diff() {
        let rows = [
            "    Finished `dev` profile [unoptimized] target(s)",
            "     Running `target/debug/lucia`",
            "hello world",
            "+ plus one line",
        ];

        assert!(!looks_like_diff(&rows));
    }

    /// hunk 内被删除的 `--` 注释渲染成 `--- ...`，必须仍是删除行而不是文件头。
    #[test]
    fn removed_line_starting_with_dashes_is_not_a_file_header() {
        assert_eq!(
            classify("--- a/src/main.rs", false),
            DiffLineKind::FileHeader
        );
        assert_eq!(
            classify("--- 这是被删掉的注释", true),
            DiffLineKind::Removed
        );
    }

    /// 增删行与结构行的配色按语义区分：只有增删行铺整行底色。
    #[test]
    fn only_changed_rows_carry_a_background() {
        assert_eq!(style_of(DiffLineKind::Added).bg, Some(COLOR_DIFF_ADD_BG));
        assert_eq!(style_of(DiffLineKind::Removed).bg, Some(COLOR_DIFF_DEL_BG));
        for kind in [
            DiffLineKind::FileHeader,
            DiffLineKind::HunkHeader,
            DiffLineKind::Context,
            DiffLineKind::Meta,
        ] {
            assert_eq!(style_of(kind).bg, None, "{kind:?} 不应铺底色");
        }
        assert!(style_of(DiffLineKind::FileHeader).bold);
        assert!(style_of(DiffLineKind::HunkHeader).bold);
        assert!(!style_of(DiffLineKind::Added).bold);
    }
}

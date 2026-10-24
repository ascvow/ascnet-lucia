//! 附件识别、粘贴路径解析与剪贴板写入。

use anyhow::Result;
use std::io::Read;
use std::path::{Path, PathBuf};

/// 自动把粘贴路径转成附件时，允许内联的文本文件最大字节数。
///
/// 文本附件会把完整内容内联进模型请求，并随会话在后续每一轮重新发送。超过这个规模
/// 应当交给 `read_file` 按 `offset`/`limit` 分段读取，而不是一次性铺满上下文；显式
/// `/attach` 不受该上限约束，那是用户已经确认过的动作。
pub(crate) const MAX_AUTO_TEXT_ATTACHMENT_BYTES: u64 = 64 * 1024;

/// 单段路径文本允许的最大字节数。
///
/// 真实路径不会这么长；设上限是为了让"用户复制的是一整段代码或日志"这种情况直接
/// 走普通文本粘贴，不必为它做文件系统探测。
const MAX_PASTED_PATH_BYTES: usize = 4096;

/// 一次多路径粘贴允许处理的总体积与条目数。
///
/// 多选复制通常只有几条到几十条路径；上限用于避免把一整篇日志当成分词输入，为每个
/// 形似路径的词都做一次文件系统探测。
const MAX_MULTI_PASTE_BYTES: usize = 64 * 1024;
const MAX_MULTI_PASTE_PATHS: usize = 256;

/// 从 `/attach` 参数解析本地文件路径。
///
/// 支持引号包裹与反斜杠转义（macOS 终端拖入文件的两种形态）以及 `~/` 前缀；
/// 相对路径按 `base`（当前项目工作目录）解析，便于用户写 `./图.png`
/// 或裸文件名。仅接受指向普通文件的路径，其余内容视为解析失败。
pub(crate) fn pasted_file_path(pasted: &str, base: &Path) -> Option<PathBuf> {
    let path = resolve_pasted_text(pasted, base)?;
    std::fs::metadata(&path)
        .ok()
        .filter(std::fs::Metadata::is_file)
        .map(|_| path)
}

/// 把一段粘贴文本解析为已存在的本地路径（文件或目录）。
///
/// 只接受**形如路径**的单段文本：含目录分隔符、以 `~` 开头或以 `.`/`..` 相对段
/// 开头。裸文件名不在此列——正文里粘贴一个词恰好与工作区文件同名时，把它变成附件
/// 是误判；文件管理器复制文件走剪贴板文件引用，不依赖这条规则。
pub(crate) fn pasted_path(pasted: &str, base: &Path) -> Option<PathBuf> {
    if !looks_like_explicit_path(pasted.trim()) {
        return None;
    }
    let path = resolve_pasted_text(pasted, base)?;
    std::fs::metadata(&path).ok().map(|_| path)
}

/// 解析一次粘贴里的多条本地路径。
///
/// 支持两种粘贴形态：每个非空行是一条路径（文件管理器多选后的「拷贝为路径名」），
/// 以及终端把多个拖入文件用空白分隔并按 shell 规则引用或转义的形态。所有条目都必须
/// 是**形如路径**且真实存在，否则整体返回 `None`：粘贴一段普通多行文本或命令时，
/// 不能因为其中几个词恰好同名文件就把整段收成附件。
pub(crate) fn pasted_paths(pasted: &str, base: &Path) -> Option<Vec<PathBuf>> {
    let trimmed = pasted.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_MULTI_PASTE_BYTES || trimmed.starts_with("file://")
    {
        return None;
    }
    // 先按行整体解析：一行一条路径时，路径内部未转义的空白属于路径本身。
    let lines: Vec<&str> = trimmed
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if lines.len() > MAX_MULTI_PASTE_PATHS {
        return None;
    }
    if lines.len() > 1 {
        let paths: Option<Vec<PathBuf>> =
            lines.iter().map(|line| pasted_path(line, base)).collect();
        if let Some(paths) = paths {
            return Some(paths);
        }
    }
    // 单行或按行解析失败时改用 shell 词法：终端拖入多个文件会用转义空格分隔路径。
    let words = split_shell_words(trimmed)?;
    if words.len() < 2 || words.len() > MAX_MULTI_PASTE_PATHS {
        return None;
    }
    words
        .iter()
        .map(|word| {
            if !looks_like_explicit_path(word) {
                return None;
            }
            absolutize_local_path(word, base).filter(|path| path.exists())
        })
        .collect()
}

/// 判断粘贴文本是否是一条指向本地路径、但当前工作目录下找不到的单条写法。
///
/// 用于粘贴路径未命中时给出提示：引号包裹的路径允许包含空白，其余情况只接受单段，
/// 避免把包含路径的普通正文误报成"文件不存在"。`file://` URI 有自己的处理分支，
/// 不在此列。
pub(crate) fn looks_like_single_pasted_path(pasted: &str) -> bool {
    let trimmed = pasted.trim();
    if trimmed.starts_with("file://") || !looks_like_explicit_path(trimmed) {
        return false;
    }
    match strip_path_wrapping(trimmed) {
        Some(inner) => !inner.trim().is_empty(),
        None => !trimmed.chars().any(char::is_whitespace),
    }
}

/// 解析终端拖入文件时给出的 `file://` URI 列表。
///
/// 部分终端（kitty、VS Code 等）把拖入的文件按 `text/uri-list` 插入输入框，而不是
/// 转义后的路径。所有非空行都必须是 URI 且目标存在才接受，避免把正文里的普通链接
/// 当成附件。
pub(crate) fn pasted_file_uris(text: &str) -> Option<Vec<PathBuf>> {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if lines.is_empty() || lines.iter().any(|line| !line.starts_with("file://")) {
        return None;
    }
    let paths: Vec<PathBuf> = lines
        .iter()
        .map(|line| decode_file_uri(line.trim_start_matches("file://")))
        .collect();
    paths.iter().all(|path| path.exists()).then_some(paths)
}

/// 判断单段文本是否是明确的路径写法，而不是恰好存在的文件名。
fn looks_like_explicit_path(text: &str) -> bool {
    !text.is_empty()
        && !text.contains('\n')
        && text.len() <= MAX_PASTED_PATH_BYTES
        && (text.contains('/')
            || text.contains('\\')
            || text.starts_with('~')
            || text.starts_with('.'))
}

/// 去掉引号与终端转义，把路径文本按 `base` 绝对化。
///
/// `/attach`、单路径粘贴与正文路径扫描共用这一步，避免三处各自实现 `~/` 展开与
/// 相对路径规则而产生分歧。
fn resolve_pasted_text(pasted: &str, base: &Path) -> Option<PathBuf> {
    let trimmed = pasted.trim();
    if trimmed.is_empty() || trimmed.contains('\n') {
        return None;
    }
    let unquoted = strip_path_wrapping(trimmed).unwrap_or_else(|| unescape_shell_path(trimmed));
    absolutize_local_path(&unquoted, base)
}

/// 去掉包住整段路径的配对引号，返回引号内部的文本。
///
/// 支持终端拖入路径常见的双引号、单引号，以及从 Markdown 正文里复制的反引号；
/// 只有首尾引号成对且类型一致时才剥离，半截引号保留原样交给存在性校验。
fn strip_path_wrapping(text: &str) -> Option<String> {
    for quote in ['"', '\'', '`'] {
        if let Some(inner) = text
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            return Some(inner.to_string());
        }
    }
    None
}

/// 按 shell 的常见引用规则把粘贴文本拆成空白分隔的词。
///
/// 只实现路径粘贴需要的最小集合：单引号内原样保留，双引号与裸文本中的反斜杠转义
/// 下一个字符。引号未闭合时返回 `None`，调用方退回普通文本粘贴。
fn split_shell_words(input: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut has_word = false;
    let mut quote: Option<char> = None;
    let mut chars = input.chars();
    while let Some(ch) = chars.next() {
        match quote {
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                } else {
                    current.push(ch);
                }
                has_word = true;
            }
            Some('"') => {
                if ch == '"' {
                    quote = None;
                } else if ch == '\\' {
                    if let Some(escaped) = chars.next() {
                        current.push(escaped);
                    }
                } else {
                    current.push(ch);
                }
                has_word = true;
            }
            Some(_) => return None,
            None if ch.is_whitespace() => {
                if has_word {
                    words.push(std::mem::take(&mut current));
                    has_word = false;
                }
            }
            None if ch == '\'' || ch == '"' => {
                quote = Some(ch);
                has_word = true;
            }
            None if ch == '\\' => {
                if let Some(escaped) = chars.next() {
                    current.push(escaped);
                }
                has_word = true;
            }
            None => {
                current.push(ch);
                has_word = true;
            }
        }
    }
    if quote.is_some() {
        return None;
    }
    if has_word {
        words.push(current);
    }
    Some(words)
}

/// 把已经去掉引号的路径文本解析为绝对路径，不校验存在性。
fn absolutize_local_path(path_text: &str, base: &Path) -> Option<PathBuf> {
    let path = if let Some(rest) = path_text.strip_prefix("~/") {
        match std::env::var("HOME") {
            Ok(home) if !home.is_empty() => PathBuf::from(home).join(rest),
            _ => return None,
        }
    } else if Path::new(path_text).is_absolute() {
        PathBuf::from(path_text)
    } else {
        base.join(path_text)
    };
    path.is_absolute().then_some(path)
}

/// 把路径文本解析为绝对路径，并确认目标真实存在。
///
/// 正文路径扫描使用这一步：目录也要返回，调用方会根据内容类型决定附加还是授权。
fn resolve_existing_path(path_text: &str, base: &Path) -> Option<PathBuf> {
    let path = absolutize_local_path(path_text, base)?;
    path.exists().then_some(path)
}

/// 直接粘贴为附件时，目标路径按模型能否消费划分的类别。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PastedPathKind {
    /// 图片或 PDF：模型原生消费二进制内容，路径文本本身没有意义。
    Media,
    /// 可解码为 UTF-8 的文本：内容会被内联进模型请求。
    Text,
    /// 音频或视频：当前模型协议尚未实现对应内容块。
    AudioVideo,
    /// 其他二进制：内联只会得到一条"内容已省略"的占位说明。
    Binary,
    /// 目录：附件无法表达，应由工具按需列举。
    Directory,
}

/// 判断一个已存在的路径属于哪一类附件目标。
///
/// 已知扩展名直接查表，不读文件；未知扩展名只嗅探开头 8 KiB，避免为判断类型而把
/// 一个大文件整体读进内存。文件不存在或无法读取时返回 IO 错误。
pub(crate) fn pasted_path_kind(path: &Path) -> std::io::Result<PastedPathKind> {
    let metadata = std::fs::metadata(path)?;
    if metadata.is_dir() {
        return Ok(PastedPathKind::Directory);
    }
    let extension = path
        .extension()
        .map(|ext| ext.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if let Some(media) = known_media_type(&extension) {
        return Ok(
            if media.starts_with("image/") || media == "application/pdf" {
                PastedPathKind::Media
            } else if media.starts_with("audio/") || media.starts_with("video/") {
                PastedPathKind::AudioVideo
            } else {
                PastedPathKind::Text
            },
        );
    }
    let mut prefix = Vec::new();
    std::fs::File::open(path)?
        .take(8 * 1024)
        .read_to_end(&mut prefix)?;
    Ok(if looks_like_text(&prefix) {
        PastedPathKind::Text
    } else {
        PastedPathKind::Binary
    })
}

/// 判断一段字节前缀是否像 UTF-8 文本。
///
/// 截断在最后一个多字节字符中间不算二进制（`error_len` 为 `None`）；含 NUL 的
/// 内容按二进制处理，避免把压缩包、可执行文件当成可以内联的文本。
fn looks_like_text(bytes: &[u8]) -> bool {
    if bytes.contains(&0) {
        return false;
    }
    match std::str::from_utf8(bytes) {
        Ok(_) => true,
        Err(error) => error.error_len().is_none(),
    }
}

/// 从系统剪贴板读取被复制的文件引用。
///
/// 文件管理器复制文件时，剪贴板的**文本**表示只有文件名，终端 bracketed paste
/// 只能送出这份文本；真实位置必须回查文件引用。返回值只包含仍存在的路径，顺序与
/// 剪贴板一致，目录也会返回（由调用方决定如何提示）。
///
/// macOS 用 `NSPasteboard` 读取全部 `NSURL`，因此多选复制同样可用；Linux 读取
/// `text/uri-list`。其他平台返回空列表，由调用方按普通文本粘贴继续处理。
pub(crate) fn clipboard_files() -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        macos_clipboard_files()
    }
    #[cfg(target_os = "linux")]
    {
        linux_clipboard_files()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Vec::new()
    }
}

/// 判断粘贴文本是否只是文件名，值得回查一次剪贴板。
///
/// 含目录分隔符或 `~` 的文本一定不是文件管理器给出的文件名表示，可以跳过回查：
/// 粘贴长文本或路径时不必每次等待一个系统进程。
pub(crate) fn may_be_clipboard_file_names(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= MAX_PASTED_PATH_BYTES
        && !text.contains('/')
        && !text.contains('\\')
        && !text.starts_with('~')
}

/// 判断一次粘贴的文本是否恰好就是剪贴板里那些文件的名字。
///
/// 文件管理器复制文件时同时提供文件名文本与文件 URL，两者一一对应。这里要求行数
/// 相等、逐行都是某个引用的文件名（且不带目录信息），因此粘贴普通文本、路径文本或
/// 数量对不上的文本都不会命中。文件名允许包含空格——它来自剪贴板本身，不存在
/// "按空白猜测"的问题。
pub(crate) fn is_clipboard_file_names(text: &str, files: &[PathBuf]) -> bool {
    if files.is_empty() {
        return false;
    }
    let names: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if names.len() != files.len() {
        return false;
    }
    let mut unmatched: Vec<&Path> = files.iter().map(PathBuf::as_path).collect();
    for name in names {
        if name.contains('/') || name.contains('\\') || name.starts_with('~') {
            return false;
        }
        let Some(index) = unmatched.iter().position(|path| {
            path.file_name()
                .is_some_and(|file_name| file_name.to_string_lossy() == name)
        }) else {
            return false;
        };
        unmatched.swap_remove(index);
    }
    true
}

/// 解析 `NSPasteboard` 或 `text/uri-list` 输出的路径列表。
///
/// 只有 macOS/Linux 的剪贴板读取与单元测试会调用它；其他平台编译时不必保留实现细节。
///
/// 逐行解析并只保留绝对路径；macOS 会输出 POSIX 路径，Linux 输出 `file://` URI。
/// 不检查存在性，由 [`clipboard_files`] 过滤。
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
fn parse_clipboard_paths(output: &str) -> Vec<PathBuf> {
    output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            line.strip_prefix("file://")
                .map(decode_file_uri)
                .or_else(|| Path::new(line).is_absolute().then(|| PathBuf::from(line)))
        })
        .collect()
}

/// 解码 `file://` URI 的百分号转义与主机段。
///
/// `file:///a%20b` → `/a b`；`file://localhost/a` 的主机段被丢弃。非法转义保留原
/// 字符，避免整条 URI 因一个坏字节被丢弃。
fn decode_file_uri(uri: &str) -> PathBuf {
    let path = uri.strip_prefix("localhost").unwrap_or(uri);
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok();
            if let Some(byte) = hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                decoded.push(byte);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    PathBuf::from(String::from_utf8_lossy(&decoded).into_owned())
}

/// 用 `NSPasteboard` 读出剪贴板里的全部文件 URL。
#[cfg(target_os = "macos")]
fn macos_clipboard_files() -> Vec<PathBuf> {
    /// 读取 `NSURL` 数组并逐行输出 POSIX 路径；纯文本剪贴板返回空串。
    const SCRIPT: &str = r#"ObjC.import("AppKit");
const pasteboard = $.NSPasteboard.generalPasteboard;
const urls = pasteboard.readObjectsForClassesOptions($([$.NSURL]), $({}));
(urls ? urls.js.map(url => url.path.js) : []).join("\n");"#;
    let output = std::process::Command::new("/usr/bin/osascript")
        .args(["-l", "JavaScript", "-e", SCRIPT])
        .stderr(std::process::Stdio::null())
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    existing_clipboard_paths(&String::from_utf8_lossy(&output.stdout))
}

/// 读取 Linux 剪贴板里的 `text/uri-list`。
///
/// Wayland 与 X11 的剪贴板工具都要显式指定目标类型；任一命令失败或环境没有该目标
/// 都视为"没有文件引用"，退回按文本粘贴处理。
#[cfg(target_os = "linux")]
fn linux_clipboard_files() -> Vec<PathBuf> {
    let candidates: &[&[&str]] = &[
        &["wl-paste", "--type", "text/uri-list"],
        &[
            "xclip",
            "-selection",
            "clipboard",
            "-t",
            "text/uri-list",
            "-o",
        ],
    ];
    for candidate in candidates {
        let Ok(output) = std::process::Command::new(candidate[0])
            .args(&candidate[1..])
            .stderr(std::process::Stdio::null())
            .output()
        else {
            continue;
        };
        if output.status.success() {
            return existing_clipboard_paths(&String::from_utf8_lossy(&output.stdout));
        }
    }
    Vec::new()
}

/// 过滤出仍然存在的剪贴板路径。
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
fn existing_clipboard_paths(output: &str) -> Vec<PathBuf> {
    parse_clipboard_paths(output)
        .into_iter()
        .filter(|path| path.exists())
        .collect()
}

/// 正文路径左侧允许出现的引号与开括号；它们属于句子结构，不是路径的一部分。
const LEADING_BOUNDARY_CHARS: &[char] = &[
    '"', '\'', '“', '”', '‘', '’', '(', '[', '{', '（', '【', '《', '「', '『',
];

/// 正文路径右侧允许剔除的句读标点与闭合括号。
///
/// `.`、`-`、`_` 等可能属于文件名，因此不在剔除集合里；`/` 与空格不可能的结尾
/// 位置也无需处理。
const TRAILING_BOUNDARY_CHARS: &[char] = &[
    '"', '\'', '“', '”', '‘', '’', '(', ')', '[', ']', '{', '}', '（', '）', '【', '】', '《',
    '》', '「', '」', '『', '』', ',', '.', ';', ':', '!', '?', '，', '。', '、', '；', '：', '！',
    '？',
];

/// 判断字符是否只是包裹路径的句子结构（空白、引号、句读或括号）。
///
/// 正文扫描用它剔除路径两侧的标点；自动附加文本时也用它判断路径是否独占一行。
pub(crate) fn is_path_boundary_char(ch: char) -> bool {
    ch.is_whitespace()
        || LEADING_BOUNDARY_CHARS.contains(&ch)
        || TRAILING_BOUNDARY_CHARS.contains(&ch)
}

/// 返回与闭合括号配对的左侧字符。
fn matching_open(closer: char) -> Option<char> {
    Some(match closer {
        ')' => '(',
        ']' => '[',
        '}' => '{',
        '）' => '（',
        '】' => '【',
        '》' => '《',
        '」' => '「',
        '』' => '『',
        _ => return None,
    })
}

/// 从一段正文中找出所有指向真实本地文件或目录的路径，返回字节区间与实际路径。
///
/// 路径可以出现在句读、括号或引号之中任一位置，不必与前面的句子用空格分开；引号内
/// 的空白属于路径，终端拖入的 `\ ` 转义同样保留。路径两侧的句子标点会被剔除，
/// 闭合括号只有在同一片段里不成对时才丢弃，以免吃掉文件名自带的 `(...)`。
/// 返回按出现顺序排列，同一路径只保留第一次出现，便于调用方按区间改写正文或去重。
pub(crate) fn mentioned_paths(text: &str, base: &Path) -> Vec<(std::ops::Range<usize>, PathBuf)> {
    let mut found: Vec<(std::ops::Range<usize>, PathBuf)> = Vec::new();
    let mut cursor = 0;
    // 只有片段起点才值得尝试解析；片段内部字符不可能是路径的开头。
    let mut at_fragment_start = true;
    while cursor < text.len() {
        let Some(first) = text[cursor..].chars().next() else {
            break;
        };
        let closing = closing_quote(first);
        if !at_fragment_start {
            at_fragment_start = is_fragment_boundary(first);
            cursor += first.len_utf8();
            continue;
        }
        // 引号本身是候选起点，其余片段边界只是分隔符。
        if closing.is_none() && is_fragment_boundary(first) {
            cursor += first.len_utf8();
            continue;
        }
        let token_start = cursor;
        cursor += first.len_utf8();
        while let Some(inner) = text[cursor..].chars().next() {
            cursor += inner.len_utf8();
            if let Some(closing) = closing {
                // 引号内允许空白：`"My Screenshot.png"` 是一个片段。
                if inner == closing {
                    break;
                }
                continue;
            }
            if inner == '\\' {
                // 反斜杠转义的拖入路径：被转义字符（含空格）仍属于同一片段。
                if let Some(escaped) = text[cursor..].chars().next() {
                    cursor += escaped.len_utf8();
                }
                continue;
            }
            if is_fragment_boundary(inner) {
                // 边界字符本身不属于当前片段，留给下一轮作为分隔符消费。
                cursor -= inner.len_utf8();
                break;
            }
        }
        at_fragment_start = false;

        let (leading, token) = trim_path_token(&text[token_start..cursor]);
        if token.is_empty() {
            continue;
        }
        let start = token_start + leading;
        let span = start..start + token.len();
        if !may_be_path(token) {
            continue;
        }
        let path = resolve_existing_path(token, base).or_else(|| {
            token
                .contains('\\')
                .then(|| unescape_shell_path(token))
                .and_then(|unescaped| resolve_existing_path(&unescaped, base))
        });
        let Some(path) = path else {
            continue;
        };
        if found.iter().any(|(_, seen)| seen == &path) {
            continue;
        }
        found.push((span, path));
    }
    found
}

/// 判断字符能否把正文切成互不相干的片段。
///
/// 句读、引号与括号不可能出现在路径里（`(`、`)` 是例外，它们是文件名的常见组成），
/// 因此可以当作可靠的片段边界。
fn is_fragment_boundary(ch: char) -> bool {
    ch.is_whitespace()
        || matches!(
            ch,
            ',' | ';'
                | ':'
                | '!'
                | '?'
                | '"'
                | '\''
                | '“'
                | '”'
                | '‘'
                | '’'
                | '，'
                | '。'
                | '、'
                | '；'
                | '：'
                | '！'
                | '？'
                | '（'
                | '）'
                | '【'
                | '】'
                | '《'
                | '》'
                | '「'
                | '」'
                | '『'
                | '』'
                | '['
                | ']'
                | '{'
                | '}'
        )
}

/// 返回与开引号配对的闭引号；非引号字符返回 `None`。
fn closing_quote(opener: char) -> Option<char> {
    Some(match opener {
        '"' => '"',
        '\'' => '\'',
        '“' => '”',
        '‘' => '’',
        _ => return None,
    })
}

/// 去掉路径片段两侧的句子标点，返回左侧被剔除的字节数与剩余片段。
fn trim_path_token(raw: &str) -> (usize, &str) {
    let mut start = 0;
    while let Some(ch) = raw[start..].chars().next() {
        if !LEADING_BOUNDARY_CHARS.contains(&ch) {
            break;
        }
        start += ch.len_utf8();
    }
    let mut end = raw.len();
    while let Some(ch) = raw[..end].chars().next_back() {
        if !TRAILING_BOUNDARY_CHARS.contains(&ch) {
            break;
        }
        if let Some(open) = matching_open(ch) {
            let segment = &raw[start..end];
            let closes = segment.chars().filter(|c| *c == ch).count();
            let opens = segment.chars().filter(|c| *c == open).count();
            // 闭合符多于开括号时才判定为句子括号，否则是文件名自身的一部分。
            if closes <= opens {
                break;
            }
        }
        end -= ch.len_utf8();
    }
    let end = end.max(start);
    (start, &raw[start..end])
}

/// 判断片段是否值得去文件系统里查一次。
///
/// 正文里的普通词语也满足“片段”的定义，若每个词都 `stat` 一次，长消息会在提交瞬间
/// 产生上百次系统调用。带目录分隔符、`~`/`.` 前缀或已知媒体扩展名的片段才可能是路径。
fn may_be_path(token: &str) -> bool {
    if token.contains('/')
        || token.contains('\\')
        || token.starts_with('~')
        || token.starts_with('.')
    {
        return true;
    }
    token
        .rsplit('.')
        .next()
        .map(|extension| extension.to_ascii_lowercase())
        .is_some_and(|extension| known_media_type(&extension).is_some())
}

/// 按扩展名查询已知附件 MIME 类型；未知扩展名返回 `None`。
///
/// 表内只列出必须按内容类型发给模型的附件。文本类扩展名留在 [`attachment_media_type`]
/// 的回落分支：那里的判定依赖内容是否为合法 UTF-8，不能只看扩展名。
fn known_media_type(extension: &str) -> Option<&'static str> {
    Some(match extension {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "m4a" => "audio/mp4",
        "ogg" => "audio/ogg",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "json" => "application/json",
        _ => return None,
    })
}

/// 去掉终端拖拽路径中的反斜杠转义（如 `\ ` 与 `\(`）。
pub(crate) fn unescape_shell_path(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// 根据扩展名与内容推断附件 MIME 类型，返回 `(media_type, 是否图片)`。
///
/// 图片仅识别主流模型接口支持的四种格式；未知扩展名按内容判断：
/// 可解码为 UTF-8 视为文本，否则视为二进制。
pub(crate) fn attachment_media_type(path: &Path, bytes: &[u8]) -> (String, bool) {
    let extension = path
        .extension()
        .map(|ext| ext.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let media_type = known_media_type(&extension).unwrap_or_else(|| {
        if std::str::from_utf8(bytes).is_ok() {
            "text/plain"
        } else {
            "application/octet-stream"
        }
    });
    (media_type.to_string(), media_type.starts_with("image/"))
}

/// 将附件 MIME 类型映射为 models.dev 输入模态；普通文件依赖文本降级能力。
pub(crate) fn attachment_modality(
    media_type: &str,
) -> crate::config::model_metadata::ModelModality {
    use crate::config::model_metadata::ModelModality;

    if media_type.starts_with("image/") {
        ModelModality::Image
    } else if media_type == "application/pdf" {
        ModelModality::Pdf
    } else if media_type.starts_with("audio/") {
        ModelModality::Audio
    } else if media_type.starts_with("video/") {
        ModelModality::Video
    } else {
        ModelModality::Text
    }
}

/// 将文本复制到系统剪贴板。
///
/// 优先调用平台剪贴板命令（pbcopy/wl-copy/xclip/xsel/clip）；命令不可用时
/// 回退为 OSC 52 转义序列，支持 SSH 远程终端。
pub(crate) fn copy_to_clipboard(text: &str) -> Result<()> {
    use base64::Engine;
    use std::io::Write;

    if copy_via_command(text) {
        return Ok(());
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let mut stdout = std::io::stdout();
    write!(stdout, "\x1b]52;c;{encoded}\x07")?;
    stdout.flush()?;
    Ok(())
}

/// 尝试通过平台剪贴板命令写入文本，返回是否成功。
pub(crate) fn copy_via_command(text: &str) -> bool {
    use std::io::Write;
    use std::process::{Command, Stdio};

    #[cfg(target_os = "macos")]
    let candidates: &[&[&str]] = &[&["/usr/bin/pbcopy"]];
    #[cfg(target_os = "windows")]
    let candidates: &[&[&str]] = &[&["clip"]];
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let candidates: &[&[&str]] = &[
        &["wl-copy"],
        &["xclip", "-selection", "clipboard"],
        &["xsel", "--clipboard", "--input"],
    ];

    for candidate in candidates {
        let Ok(mut child) = Command::new(candidate[0])
            .args(&candidate[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        let written = child
            .stdin
            .take()
            .is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
        if written && child.wait().is_ok_and(|status| status.success()) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 写入一个唯一命名的测试文件，返回路径；测试结束由调用方删除。
    fn write_temp_file(name: &str, bytes: &[u8]) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("系统时间应晚于 Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "lucia-mentioned-test-{}-{unique}-{name}",
            std::process::id()
        ));
        std::fs::write(&path, bytes).expect("写入测试文件");
        path
    }

    /// 正文里的本地路径能被正确切分，句读标点与句子括号不进入路径。
    #[test]
    fn mentioned_paths_finds_paths_in_prose() {
        let spaced = write_temp_file("句读 图.png", b"\x89PNG");
        let plain = write_temp_file("正文图.png", b"\x89PNG");
        let base = std::env::temp_dir();
        let spaced_display = spaced.display().to_string();
        let plain_display = plain.display().to_string();

        // 中文句读与不成对的句子括号都不属于路径；文件名里的空格由引号包起来。
        let text = format!("具体看图，（\"{spaced_display}\"）。");
        let found = mentioned_paths(&text, &base);
        assert_eq!(found.len(), 1, "正文中的图片路径应被识别：{text}");
        assert_eq!(found[0].1, spaced);
        assert_eq!(
            &text[found[0].0.clone()],
            spaced_display,
            "区间应只覆盖路径本身"
        );

        // 单引号引包同样识别；无空格的路径不需要引号。
        let quoted = format!("看 '{spaced_display}' 这张");
        assert_eq!(mentioned_paths(&quoted, &base).len(), 1);
        let bare = plain.file_name().unwrap().to_string_lossy().into_owned();
        let relative = mentioned_paths(&format!("看 {bare}"), &base);
        assert_eq!(relative.len(), 1, "裸文件名应按基准目录解析");
        assert_eq!(relative[0].1, plain);
        let parenthesized = mentioned_paths(&format!("看 ({plain_display})"), &base);
        assert_eq!(parenthesized.len(), 1, "不成对的句子括号应被剔除");
        assert_eq!(parenthesized[0].1, plain);

        // 终端拖入的转义空格路径也能识别。
        let escaped = format!("看 {}", spaced_display.replace(' ', "\\ "));
        assert_eq!(mentioned_paths(&escaped, &base).len(), 1);

        std::fs::remove_file(&spaced).ok();
        std::fs::remove_file(&plain).ok();
    }

    /// 文件名自带的成对括号不能被当成句子标点剔除。
    #[test]
    fn mentioned_paths_keeps_balanced_brackets() {
        let path = write_temp_file("截图(1).png", b"\x89PNG");
        let display = path.display().to_string();
        let base = std::env::temp_dir();

        // 外层的 `(...)` 是句子括号，路径自身的 `(1)` 必须保留。
        let found = mentioned_paths(&format!("看 ({display})"), &base);
        assert_eq!(found.len(), 1, "成对括号应保留在路径里：{display}");
        assert_eq!(found[0].1, path);

        std::fs::remove_file(&path).ok();
    }

    /// 正文扫描同时返回文本文件与目录，供调用方决定附加还是授权。
    #[test]
    fn mentioned_paths_includes_text_files_and_directories() {
        let base = std::env::temp_dir();
        let text = write_temp_file("提到的文本.txt", b"hi");
        let directory = base.join(format!(
            "lucia-mentioned-dir-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("系统时间应晚于 Unix epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).expect("应创建测试目录");

        let text_display = text.display().to_string();
        let found = mentioned_paths(&format!("看 {text_display}"), &base);
        assert_eq!(found.len(), 1, "文本文件应被正文扫描发现");
        assert_eq!(found[0].1, text);

        let directory_display = directory.display().to_string();
        let found = mentioned_paths(&directory_display, &base);
        assert_eq!(found.len(), 1, "目录应被正文扫描发现");
        assert_eq!(found[0].1, directory);

        std::fs::remove_file(&text).ok();
        std::fs::remove_dir_all(&directory).ok();
    }

    /// 缺失路径不会被收录，同一路径重复出现只保留一次。
    #[test]
    fn mentioned_paths_ignores_missing_paths_and_duplicates() {
        let image = write_temp_file("重复.png", b"\x89PNG");
        let text = write_temp_file("笔记.txt", b"hi");
        let base = std::env::temp_dir();
        let display = image.display().to_string();

        let prose = format!("{display} 和 {} 还有 {display}", text.display());
        let found = mentioned_paths(&prose, &base);
        assert_eq!(found.len(), 2, "图片与文本各一次，重复路径只保留一次");
        assert!(found.iter().any(|(_, path)| path == &image));
        assert!(found.iter().any(|(_, path)| path == &text));

        assert!(mentioned_paths("没有路径的一段话", &base).is_empty());
        assert!(mentioned_paths("/nonexistent/lucia.png", &base).is_empty());
        // 目录同样会被收录，由调用方决定只读授权而不是附加。
        assert_eq!(mentioned_paths(&base.display().to_string(), &base).len(), 1);

        std::fs::remove_file(&image).ok();
        std::fs::remove_file(&text).ok();
    }

    /// 剪贴板输出只解析绝对路径与 `file://` URI，并过滤掉已经消失的条目。
    #[test]
    fn clipboard_paths_parse_absolute_paths_and_uris() {
        let path = std::env::temp_dir().join(format!(
            "lucia-clipboard-test-{}-{}.png",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("系统时间应晚于 Unix epoch")
                .as_nanos()
        ));
        std::fs::write(&path, b"\x89PNG").expect("写入剪贴板测试文件");

        // macOS 输出 POSIX 路径，目录也保留（由调用方决定如何提示）。
        assert_eq!(
            existing_clipboard_paths(&format!("{}\n", path.display())),
            vec![path.clone()]
        );
        assert_eq!(
            existing_clipboard_paths("/tmp\n"),
            vec![PathBuf::from("/tmp")]
        );

        // Linux 输出 `file://` URI：百分号转义要还原，主机段要丢弃。
        assert_eq!(
            existing_clipboard_paths(&format!(
                "file://{}\nfile://localhost{}\n",
                path.display(),
                path.display()
            )),
            vec![path.clone(), path.clone()]
        );
        assert_eq!(
            parse_clipboard_paths("file:///tmp/a%20b.png"),
            vec![PathBuf::from("/tmp/a b.png")]
        );

        // 相对路径与不存在的路径都不进入结果；空输出得到空列表。
        assert!(parse_clipboard_paths("relative.png").is_empty());
        assert!(existing_clipboard_paths("/nonexistent/lucia.png").is_empty());
        assert!(existing_clipboard_paths("").is_empty());

        std::fs::remove_file(&path).ok();
    }

    /// 粘贴路径必须先像路径：裸文件名不当附件，文件与目录都能解析。
    #[test]
    fn pasted_path_requires_path_shape() {
        let base = std::env::temp_dir();
        let path = write_temp_file("粘贴的 路径.txt", b"hello");
        let name = path.file_name().unwrap().to_string_lossy().into_owned();

        assert_eq!(
            pasted_path(&path.display().to_string(), &base),
            Some(path.clone())
        );
        assert_eq!(pasted_path(&format!("./{name}"), &base), Some(path.clone()));
        // 终端拖入的转义空格路径与引号包裹路径同样成立。
        assert_eq!(
            pasted_path(&path.display().to_string().replace(' ', "\\ "), &base),
            Some(path.clone())
        );
        assert_eq!(
            pasted_path(&format!("'{}'", path.display()), &base),
            Some(path.clone())
        );
        // Markdown 正文里复制的反引号包裹路径同样剥掉包装。
        assert_eq!(
            pasted_path(&format!("`{}`", path.display()), &base),
            Some(path.clone())
        );
        // 只有前引号时不能静默吞掉它，否则半截路径会被当成文件。
        assert_eq!(pasted_path(&format!("\"{}", path.display()), &base), None);

        // 裸文件名可能只是正文，不当作附件；不存在的路径同样返回 `None`。
        assert_eq!(pasted_path(&name, &base), None);
        assert_eq!(pasted_path("/nonexistent/lucia.txt", &base), None);

        // 目录可以解析出来，由调用方提示"目录不能作为附件"。
        assert_eq!(pasted_path("/tmp", &base), Some(PathBuf::from("/tmp")));

        std::fs::remove_file(&path).ok();
    }

    /// 一次粘贴多条路径时，按行或按 shell 转义拆分；任何条目缺失都整体拒绝。
    #[test]
    fn pasted_paths_accepts_line_and_shell_forms() {
        let base = std::env::temp_dir();
        let first = write_temp_file("多路径 一.png", b"\x89PNG");
        let second = write_temp_file("多路径二.txt", b"hi");
        let expected = vec![first.clone(), second.clone()];

        // 文件管理器多选「拷贝为路径名」：一行一条，路径里的空格无需转义。
        let lines = format!("{}\n{}", first.display(), second.display());
        assert_eq!(pasted_paths(&lines, &base), Some(expected.clone()));

        // 终端拖入多个文件：空白分隔，路径里的空格用反斜杠转义。
        let escaped = format!(
            "{} {}",
            first.display().to_string().replace(' ', "\\ "),
            second.display()
        );
        assert_eq!(pasted_paths(&escaped, &base), Some(expected));

        // 单条路径、正文、命令与任一缺失条目都不能被当成多路径附件。
        assert_eq!(pasted_paths(&second.display().to_string(), &base), None);
        assert_eq!(pasted_paths("随便说两句\n没有路径", &base), None);
        assert_eq!(pasted_paths("cp /tmp/a.png /tmp/b.png", &base), None);
        assert_eq!(
            pasted_paths(
                &format!("{} /nonexistent/lucia.png", second.display()),
                &base
            ),
            None
        );
        // 引号未闭合时退回普通文本，不能静默吞掉半截路径。
        assert_eq!(
            pasted_paths(&format!("'{} {}", first.display(), second.display()), &base),
            None
        );

        std::fs::remove_file(&first).ok();
        std::fs::remove_file(&second).ok();
    }

    /// 只有明确指向本地文件的单条写法才会在未命中时提示，普通正文不受影响。
    #[test]
    fn missing_path_hint_requires_single_path_shape() {
        assert!(looks_like_single_pasted_path("/Users/me/图片.png"));
        assert!(looks_like_single_pasted_path("~/图片.png"));
        assert!(looks_like_single_pasted_path("./图片.png"));
        assert!(looks_like_single_pasted_path("'/Users/me/我的 图.png'"));
        assert!(looks_like_single_pasted_path("`/Users/me/图.png`"));

        assert!(!looks_like_single_pasted_path("看 /tmp/不存在.png 这张"));
        assert!(!looks_like_single_pasted_path("file:///tmp/不存在.png"));
        assert!(!looks_like_single_pasted_path("图片.png"));
        assert!(!looks_like_single_pasted_path(""));
    }

    /// 路径分类决定"直接粘贴"能否变成附件：文本可内联，二进制与目录不行。
    #[test]
    fn pasted_path_kind_classifies_content() {
        let text = write_temp_file("分类.txt", "内容".as_bytes());
        let unknown_text = write_temp_file("分类.unknown", "内容".as_bytes());
        let binary = write_temp_file("分类.bin", &[0x00, 0xff, 0xfe]);
        let image = write_temp_file("分类.png", b"\x89PNG");
        let audio = write_temp_file("分类.mp3", b"ID3");
        let json = write_temp_file("分类.json", b"{}");

        assert_eq!(
            pasted_path_kind(&text).expect("文本应可分类"),
            PastedPathKind::Text
        );
        assert_eq!(
            pasted_path_kind(&unknown_text).expect("未知扩展名应可分类"),
            PastedPathKind::Text
        );
        assert_eq!(
            pasted_path_kind(&binary).expect("二进制应可分类"),
            PastedPathKind::Binary
        );
        assert_eq!(
            pasted_path_kind(&image).expect("图片应可分类"),
            PastedPathKind::Media
        );
        assert_eq!(
            pasted_path_kind(&audio).expect("音频应可分类"),
            PastedPathKind::AudioVideo
        );
        assert_eq!(
            pasted_path_kind(&json).expect("JSON 应可分类"),
            PastedPathKind::Text
        );
        assert_eq!(
            pasted_path_kind(Path::new("/tmp")).expect("目录应可分类"),
            PastedPathKind::Directory
        );
        assert!(pasted_path_kind(Path::new("/nonexistent/lucia")).is_err());

        for path in [text, unknown_text, binary, image, audio, json] {
            std::fs::remove_file(&path).ok();
        }
    }

    /// 从中间截断的多字节字符不算二进制：嗅探前缀不能把正常文本判成二进制。
    #[test]
    fn text_prefix_sniffing_accepts_truncated_multibyte_tail() {
        let mut bytes = "中文".as_bytes().to_vec();
        bytes.pop();
        assert!(looks_like_text(&bytes), "截断在字符中间仍应视为文本");
        assert!(
            !looks_like_text(&[b'a', 0x00, b'b']),
            "含 NUL 的内容是二进制"
        );
        assert!(!looks_like_text(&[0xff, 0xfe]), "非法 UTF-8 是二进制");
    }

    /// 只有"粘贴的文件名与剪贴板文件引用一一对应"才认定为复制文件；任何扩展名都接受。
    #[test]
    fn clipboard_file_names_match_exactly() {
        let dir = Path::new("/tmp/attachments");
        let reference = dir.join("e5331c60-8bc4-4f0d-8327-c2a33b0820dc.png");
        assert!(is_clipboard_file_names(
            "e5331c60-8bc4-4f0d-8327-c2a33b0820dc.png",
            std::slice::from_ref(&reference)
        ));

        // 非媒体类型、无扩展名、带空格的名字同样是文件：文件管理器能复制的都应能附加。
        for name in [
            "报告.pdf",
            "photo.JPEG",
            "notes.txt",
            "main.swift",
            "archive.xyz",
            "Makefile",
            ".env",
            "我的 截图.png",
        ] {
            assert!(
                is_clipboard_file_names(name, &[dir.join(name)]),
                "`{name}` 应被认定为复制文件"
            );
        }

        // 多选复制：文本按行给出文件名，顺序与剪贴板引用一致即可。
        let multiple = [dir.join("a.png"), dir.join("b.txt")];
        assert!(is_clipboard_file_names("a.png\nb.txt", &multiple));
        assert!(
            !is_clipboard_file_names("a.png", &multiple),
            "数量对不上不是一次文件复制"
        );
        assert!(
            !is_clipboard_file_names("a.png\nc.png", &multiple),
            "名字必须逐一命中剪贴板引用"
        );

        // 路径文本、普通句子与空文本都不应命中。
        let single = std::slice::from_ref(&reference);
        assert!(!is_clipboard_file_names("/Users/me/图.png", single));
        assert!(!is_clipboard_file_names("~/图.png", single));
        assert!(!is_clipboard_file_names("./图.png", single));
        assert!(!is_clipboard_file_names("普通粘贴文本", single));
        assert!(!is_clipboard_file_names("", single));
        assert!(!is_clipboard_file_names("任意文本", &[]));

        // 含目录分隔或 `~` 的文本不必回查剪贴板，避免每次粘贴都启动系统进程。
        assert!(!may_be_clipboard_file_names("/Users/me/图.png"));
        assert!(!may_be_clipboard_file_names("~/图.png"));
        assert!(!may_be_clipboard_file_names(
            &"长".repeat(MAX_PASTED_PATH_BYTES)
        ));
        assert!(may_be_clipboard_file_names("图.png"));
        assert!(may_be_clipboard_file_names("我的 截图.png\n笔记.txt"));
    }

    /// 已知识别表与媒体类型判断保持原语义：未知扩展名按内容嗅探。
    #[test]
    fn media_type_keeps_content_sniffing_fallback() {
        assert_eq!(known_media_type("png"), Some("image/png"));
        assert_eq!(known_media_type("xyz"), None);

        let text = attachment_media_type(Path::new("a.unknown"), "内容".as_bytes());
        assert_eq!(text.0, "text/plain");
        let binary = attachment_media_type(Path::new("a.unknown"), &[0xff, 0xfe, 0x00]);
        assert_eq!(binary.0, "application/octet-stream");
    }

    /// 真实剪贴板验收：确认回查不会返回非法路径，并在剪贴板持有文件引用时能取到它。
    ///
    /// 结果取决于运行者当前剪贴板内容，因此默认忽略；本机验收用
    /// `cargo test -p lucia --features plugins -- --ignored --nocapture clipboard_file_reference`。
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "结果取决于当前系统剪贴板内容，仅在本机验收时运行"]
    fn macos_clipboard_file_reference_resolves_real_file() {
        let files = clipboard_files();
        if files.is_empty() {
            println!("剪贴板当前不持有文件引用（纯文本或空）");
            return;
        }
        for path in files {
            assert!(
                path.is_absolute(),
                "回查必须得到绝对路径：{}",
                path.display()
            );
            assert!(path.exists(), "回查必须得到存在的路径：{}", path.display());
            println!("剪贴板文件引用：{}", path.display());
        }
    }

    /// 测试结束或断言失败时恢复原始 macOS 剪贴板内容，避免诊断污染用户数据。
    #[cfg(target_os = "macos")]
    struct MacosClipboardRestore(Vec<u8>);

    #[cfg(target_os = "macos")]
    impl Drop for MacosClipboardRestore {
        fn drop(&mut self) {
            use std::io::Write;
            use std::process::{Command, Stdio};

            let Ok(mut child) = Command::new("/usr/bin/pbcopy")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            else {
                return;
            };
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(&self.0);
            }
            let _ = child.wait();
        }
    }

    /// 真实 macOS 剪贴板测试必须写入唯一文本、读回比较，并在结束时恢复原内容。
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "会短暂修改当前用户的系统剪贴板，仅在本机验收时运行"]
    fn macos_clipboard_command_round_trip() {
        use std::process::Command;

        let original = Command::new("/usr/bin/pbpaste")
            .output()
            .expect("应读取当前 macOS 剪贴板");
        assert!(original.status.success(), "读取 macOS 剪贴板失败");
        let _restore = MacosClipboardRestore(original.stdout);
        let expected = format!(
            "lucia-copy-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("系统时间应晚于 Unix epoch")
                .as_nanos()
        );

        assert!(copy_via_command(&expected), "pbcopy 应成功写入测试文本");
        let actual = Command::new("/usr/bin/pbpaste")
            .output()
            .expect("应读回 macOS 剪贴板");
        assert!(actual.status.success(), "读回 macOS 剪贴板失败");
        assert_eq!(actual.stdout, expected.as_bytes());
    }
}

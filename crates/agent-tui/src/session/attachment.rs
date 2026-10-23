//! 附件识别、粘贴路径解析与剪贴板写入。

use anyhow::Result;
use std::path::{Path, PathBuf};
/// 从粘贴内容解析拖拽或输入的文件路径。
///
/// 支持引号包裹与反斜杠转义（macOS 终端拖入文件的两种形态）以及 `~/` 前缀；
/// 相对路径按 `base`（当前项目工作目录）解析，便于用户在项目内粘贴 `./图.png`
/// 或裸文件名。仅接受指向普通文件的路径，其余内容按普通文本粘贴处理。
pub(crate) fn pasted_file_path(pasted: &str, base: &Path) -> Option<PathBuf> {
    let trimmed = pasted.trim();
    if trimmed.is_empty() || trimmed.contains('\n') {
        return None;
    }
    let unquoted = trimmed
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| {
            trimmed
                .strip_prefix('\'')
                .and_then(|s| s.strip_suffix('\''))
        })
        .map(str::to_string)
        .unwrap_or_else(|| unescape_shell_path(trimmed));
    let path = if let Some(rest) = unquoted.strip_prefix("~/") {
        match std::env::var("HOME") {
            Ok(home) if !home.is_empty() => PathBuf::from(home).join(rest),
            _ => return None,
        }
    } else if Path::new(&unquoted).is_absolute() {
        PathBuf::from(&unquoted)
    } else {
        base.join(&unquoted)
    };
    if !path.is_absolute() {
        return None;
    }
    std::fs::metadata(&path)
        .ok()
        .filter(std::fs::Metadata::is_file)
        .map(|_| path)
}

/// 从系统剪贴板读取被复制文件的绝对路径。
///
/// Finder 复制文件时，剪贴板的**文本**表示只有文件名、不含目录，终端 bracketed paste
/// 只能送出这份文本；此时必须回查剪贴板中的文件引用才能拿到真实位置。
///
/// 仅在 macOS 实现：该平台以文件 URL 保存被复制文件。其他平台的终端拖拽通常已给出
/// 完整路径，因此返回 `None`，由调用方按普通文本粘贴继续处理。
pub(crate) fn clipboard_file_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("/usr/bin/osascript")
            .args(["-e", "POSIX path of (the clipboard as «class furl»)"])
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        return parse_clipboard_path(&String::from_utf8_lossy(&output.stdout));
    }
    #[cfg(not(target_os = "macos"))]
    None
}

/// 校验 `osascript` 输出的 POSIX 路径，只接受真实存在的普通文件。
///
/// 剪贴板里是纯文本时 AppleScript 仍会把文本当作相对路径解析并成功退出（例如文本
/// `plain text` 得到 `/plain text`），因此不能只看退出码。
fn parse_clipboard_path(output: &str) -> Option<PathBuf> {
    let path = PathBuf::from(output.trim());
    if !path.is_absolute() {
        return None;
    }
    std::fs::metadata(&path)
        .ok()
        .filter(std::fs::Metadata::is_file)
        .map(|_| path)
}

/// 判断粘贴文本是否"看起来是一个附件路径"。
///
/// 用于决定是否值得回查剪贴板文件引用：单行、不含空白，且扩展名在
/// [`known_media_type`] 的已知表内。命中的文本若既不能解析为路径、剪贴板也没有文件
/// 引用，应报错提示而不是静默折叠成文本附件。
pub(crate) fn looks_like_file_path(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.contains('\n') || trimmed.chars().any(char::is_whitespace) {
        return false;
    }
    let extension = Path::new(trimmed)
        .extension()
        .map(|ext| ext.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    known_media_type(&extension).is_some()
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

    /// 剪贴板文件引用只接受绝对路径且真实存在的普通文件。
    ///
    /// 剪贴板是纯文本时 AppleScript 仍会把文本当相对路径解析并成功输出
    /// （如 `/plain text`），这里断言这类输出必须被拒绝。
    #[test]
    fn clipboard_path_requires_existing_file() {
        let path = std::env::temp_dir().join(format!(
            "lucia-clipboard-test-{}-{}.png",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("系统时间应晚于 Unix epoch")
                .as_nanos()
        ));
        std::fs::write(&path, b"\x89PNG").expect("写入剪贴板测试文件");

        assert_eq!(
            parse_clipboard_path(&format!("{}\n", path.display())),
            Some(path.clone())
        );
        // 纯文本被 AppleScript 解析出的伪路径与不存在的绝对路径都必须拒绝。
        assert_eq!(parse_clipboard_path("/plain text"), None);
        assert_eq!(parse_clipboard_path("/nonexistent/lucia.png"), None);
        // 目录不是可读取的附件。
        assert_eq!(parse_clipboard_path("/tmp"), None);
        assert_eq!(parse_clipboard_path(""), None);

        std::fs::remove_file(&path).ok();
    }

    /// 只有单行、无空白且扩展名在已知附件表内的文本才会触发剪贴板回查。
    #[test]
    fn looks_like_file_path_only_accepts_attachment_names() {
        assert!(looks_like_file_path(
            "e5331c60-8bc4-4f0d-8327-c2a33b0820dc.png"
        ));
        assert!(looks_like_file_path("  报告.pdf  "));
        assert!(looks_like_file_path("photo.JPEG"));

        // 普通文本、多行、含空白的句子都不应触发回查。
        assert!(!looks_like_file_path("普通粘贴文本"));
        assert!(!looks_like_file_path("第一行\n第二行"));
        assert!(!looks_like_file_path("带 空格.png"));
        assert!(!looks_like_file_path(""));
        // 文本类与未知扩展名留给内容嗅探，不在此处拦截。
        assert!(!looks_like_file_path("notes.txt"));
        assert!(!looks_like_file_path("archive.xyz"));
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
        match clipboard_file_path() {
            Some(path) => {
                assert!(
                    path.is_absolute(),
                    "回查必须得到绝对路径：{}",
                    path.display()
                );
                assert!(path.is_file(), "回查必须得到普通文件：{}", path.display());
                println!("剪贴板文件引用：{}", path.display());
            }
            None => println!("剪贴板当前不持有文件引用（纯文本或空）"),
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

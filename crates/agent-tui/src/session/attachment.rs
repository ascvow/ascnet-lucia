//! 附件识别、粘贴路径解析与剪贴板写入。

use anyhow::Result;
use std::path::{Path, PathBuf};
/// 从粘贴内容解析拖拽的单个文件路径。
///
/// 支持引号包裹与反斜杠转义（macOS 终端拖入文件的两种形态）以及 `~/` 前缀；
/// 仅接受指向普通文件的绝对路径，其余内容按普通文本粘贴处理。
pub(crate) fn pasted_file_path(pasted: &str) -> Option<PathBuf> {
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
    } else {
        PathBuf::from(&unquoted)
    };
    if !path.is_absolute() {
        return None;
    }
    std::fs::metadata(&path)
        .ok()
        .filter(std::fs::Metadata::is_file)
        .map(|_| path)
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
    let media_type = match extension.as_str() {
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
        _ => {
            if std::str::from_utf8(bytes).is_ok() {
                "text/plain"
            } else {
                "application/octet-stream"
            }
        }
    };
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

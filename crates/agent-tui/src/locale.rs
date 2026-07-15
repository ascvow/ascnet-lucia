//! TUI 系统语言解析与中英文文案选择。

use std::{env, process::Command};

/// Lucia 运行时界面支持的语言。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Locale {
    /// 英文界面，也是未知系统语言的稳定回退。
    #[default]
    English,
    /// 简体中文界面。
    SimplifiedChinese,
}

impl Locale {
    /// 按 POSIX locale 优先级检测系统语言；macOS 缺失环境变量时读取 AppleLocale。
    pub(crate) fn detect() -> Self {
        for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
            if let Ok(locale) = env::var(key) {
                if !locale.trim().is_empty() {
                    return Self::from_tag(&locale);
                }
            }
        }
        #[cfg(target_os = "macos")]
        if let Ok(output) = Command::new("/usr/bin/defaults")
            .args(["read", "-g", "AppleLocale"])
            .output()
        {
            if output.status.success() {
                if let Ok(locale) = String::from_utf8(output.stdout) {
                    return Self::from_tag(&locale);
                }
            }
        }
        Self::English
    }

    /// 从 BCP 47 或 POSIX locale 标签解析支持的语言。
    pub(crate) fn from_tag(locale: &str) -> Self {
        let normalized = locale.trim().replace('_', "-").to_ascii_lowercase();
        if normalized == "zh" || normalized.starts_with("zh-") {
            Self::SimplifiedChinese
        } else {
            Self::English
        }
    }

    /// 返回注入插件激活上下文的规范化 locale。
    #[cfg(feature = "plugins")]
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::English => "en",
            Self::SimplifiedChinese => "zh-CN",
        }
    }

    /// 在英文和简体中文文案之间选择当前语言对应的值。
    pub(crate) fn select<'a>(self, english: &'a str, simplified_chinese: &'a str) -> &'a str {
        match self {
            Self::English => english,
            Self::SimplifiedChinese => simplified_chinese,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 常见中文 locale 应归一化为简体中文，其他标签稳定回退到英文。
    #[test]
    fn parses_supported_system_locales() {
        for locale in ["zh", "zh_CN.UTF-8", "zh-Hans-CN", "zh-TW"] {
            assert_eq!(Locale::from_tag(locale), Locale::SimplifiedChinese);
        }
        for locale in ["en_US.UTF-8", "ja-JP", "C"] {
            assert_eq!(Locale::from_tag(locale), Locale::English);
        }
    }
}

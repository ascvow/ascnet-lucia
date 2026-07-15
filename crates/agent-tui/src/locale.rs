//! TUI 系统语言解析与 vue-i18n 风格的键值文案查找。
//!
//! 文案以 `locales/*.toml` 语言包形式按需嵌入二进制：英文包始终嵌入作为回退基线，
//! 其他语言包由 Cargo feature 控制。查找按「当前语言 → 英文 → key 本身」逐级回退，
//! 因此语言包未打包或解析失败时界面仍能正常显示。

use std::{collections::HashMap, env, process::Command, sync::OnceLock};

/// 英文语言包原文，作为回退基线始终嵌入二进制。
const EN_MESSAGES: &str = include_str!("../locales/en.toml");

/// 简体中文语言包原文，仅在启用 `lang-zh-cn` feature 时嵌入。
#[cfg(feature = "lang-zh-cn")]
const ZH_CN_MESSAGES: &str = include_str!("../locales/zh-CN.toml");

/// 解析 TOML 语言包为扁平键值表；解析失败按空表处理，让查找回退英文。
fn parse_messages(raw: &str) -> HashMap<String, String> {
    toml::from_str(raw).unwrap_or_default()
}

/// 英文语言包，进程内只解析一次。
fn english_messages() -> &'static HashMap<String, String> {
    static CATALOG: OnceLock<HashMap<String, String>> = OnceLock::new();
    CATALOG.get_or_init(|| parse_messages(EN_MESSAGES))
}

/// 简体中文语言包；未打包对应 feature 时为空表，查找自动回退英文。
fn simplified_chinese_messages() -> &'static HashMap<String, String> {
    static CATALOG: OnceLock<HashMap<String, String>> = OnceLock::new();
    CATALOG.get_or_init(|| {
        #[cfg(feature = "lang-zh-cn")]
        {
            parse_messages(ZH_CN_MESSAGES)
        }
        #[cfg(not(feature = "lang-zh-cn"))]
        {
            HashMap::new()
        }
    })
}

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

    /// 当前语言的语言包；对应语言包未打包时为空表。
    fn messages(self) -> &'static HashMap<String, String> {
        match self {
            Self::English => english_messages(),
            Self::SimplifiedChinese => simplified_chinese_messages(),
        }
    }

    /// 查找 key 对应的文案：当前语言缺失时回退英文，仍缺失时返回 key 本身。
    pub(crate) fn t(self, key: &'static str) -> &'static str {
        self.messages()
            .get(key)
            .or_else(|| english_messages().get(key))
            .map(String::as_str)
            .unwrap_or(key)
    }

    /// 查找文案并把 `{name}` 形式的占位符替换为对应参数值。
    pub(crate) fn t_args(self, key: &'static str, args: &[(&str, &str)]) -> String {
        let mut text = self.t(key).to_string();
        for (name, value) in args {
            text = text.replace(&format!("{{{name}}}"), value);
        }
        text
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

    /// 英文语言包必须可解析且非空，否则整条回退链失效。
    #[test]
    fn english_catalog_parses_and_is_non_empty() {
        assert!(!english_messages().is_empty());
    }

    /// 中文语言包打包时必须与英文包 key 一一对应，防止漏译或遗留死 key。
    #[cfg(feature = "lang-zh-cn")]
    #[test]
    fn simplified_chinese_catalog_matches_english_keys() {
        let english = english_messages();
        let chinese = parse_messages(ZH_CN_MESSAGES);
        assert!(!chinese.is_empty(), "中文语言包解析失败或为空");
        for key in english.keys() {
            assert!(chinese.contains_key(key), "中文语言包缺少 key：{key}");
        }
        for key in chinese.keys() {
            assert!(english.contains_key(key), "英文语言包缺少 key：{key}");
        }
    }

    /// 语言包缺失 key 时回退英文，英文也缺失时返回 key 本身，保证界面始终有内容。
    #[test]
    fn lookup_falls_back_to_english_then_key() {
        assert_eq!(
            Locale::English.t("copy.failed"),
            english_messages()["copy.failed"]
        );
        assert_eq!(
            Locale::SimplifiedChinese.t("nonexistent.key"),
            "nonexistent.key"
        );
    }

    /// 占位符替换应命中全部参数，且不破坏文案其余部分。
    #[test]
    fn interpolates_named_placeholders() {
        let text = Locale::English.t_args(
            "chat.tool_render_failed",
            &[("call_id", "call-1"), ("error", "boom")],
        );
        assert_eq!(text, "Failed to render tool message `call-1`: boom");
    }
}

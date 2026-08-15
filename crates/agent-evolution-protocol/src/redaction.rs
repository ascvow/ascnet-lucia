//! 确定性脱敏。
//!
//! 脱敏在**持久化之前**执行，因此原始凭据不会先落盘再被清理。所有规则都是纯函数：
//! 相同输入必然得到相同输出与相同的命中集合，便于在测试中固定断言。
//!
//! 规则按固定顺序应用，先处理结构化位置（URL 凭据、请求头、键值对），
//! 再处理独立出现的令牌字面量，最后处理私有路径。顺序固定是确定性的前提。

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, sync::OnceLock};

/// 当前脱敏规则集版本。
///
/// 规则发生任何行为变化时必须递增。Episode 记录该版本，使后续可以判断某条证据
/// 是用哪一版规则清洗的。
pub const REDACTION_RULES_VERSION: &str = "1";

/// 替换后的占位文本，保持稳定以确保输出可比较。
const PLACEHOLDER: &str = "[REDACTED]";

/// 单条脱敏规则。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RedactionRule {
    /// URL 中的 `user:password@` 凭据。
    UrlCredentials,
    /// `Authorization` 请求头。
    AuthorizationHeader,
    /// `Cookie` 与 `Set-Cookie` 请求头。
    CookieHeader,
    /// 键名暗示凭据的键值对，例如 `api_key=...`。
    KeyValueSecret,
    /// `Bearer <token>` 形式的令牌。
    BearerToken,
    /// 服务商令牌字面量，例如 `sk-`、`ghp_`、`AKIA`。
    ProviderToken,
    /// JSON Web Token。
    JsonWebToken,
    /// 用户主目录等私有路径。
    PrivatePath,
}

/// 一次脱敏的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactionOutcome {
    /// 脱敏后的文本。
    pub text: String,
    /// 生成该结果的规则集版本。
    pub rules_version: &'static str,
    /// 实际命中的规则；使用有序集合保证遍历顺序稳定。
    pub applied: BTreeSet<RedactionRule>,
}

impl RedactionOutcome {
    /// 判断是否有任何规则命中。
    pub fn is_modified(&self) -> bool {
        !self.applied.is_empty()
    }
}

/// 已编译的规则表；每条规则一个正则，按固定顺序应用。
struct CompiledRules {
    ordered: Vec<(RedactionRule, Regex, &'static str)>,
    /// 键值对凭据单独处理：需要按值的形态决定是否替换，无法用固定替换串表达。
    key_value_secret: Regex,
    home_like: Regex,
}

/// 返回进程内共享的规则表。
///
/// 正则在首次使用时编译一次。表达式为编译期常量，编译失败属于程序缺陷，因此直接 panic。
fn rules() -> &'static CompiledRules {
    static RULES: OnceLock<CompiledRules> = OnceLock::new();
    RULES.get_or_init(|| {
        // 每项为：规则、匹配式、替换式（`$n` 引用捕获组）。
        let specs: Vec<(RedactionRule, &str, &str)> = vec![
            (
                RedactionRule::UrlCredentials,
                r"(?i)\b([a-z][a-z0-9+.-]*://)[^\s/:@]+:[^\s/@]*@",
                "${1}[REDACTED]@",
            ),
            (
                RedactionRule::AuthorizationHeader,
                r"(?im)^(\s*(?:proxy-)?authorization\s*:\s*).+$",
                "${1}[REDACTED]",
            ),
            (
                RedactionRule::CookieHeader,
                r"(?im)^(\s*(?:set-)?cookie\s*:\s*).+$",
                "${1}[REDACTED]",
            ),
            (
                RedactionRule::BearerToken,
                r"(?i)\b(bearer\s+)[A-Za-z0-9._~+/=-]{8,}",
                "${1}[REDACTED]",
            ),
            (
                RedactionRule::JsonWebToken,
                r"\beyJ[A-Za-z0-9_-]{4,}\.[A-Za-z0-9_-]{4,}\.[A-Za-z0-9_-]{4,}",
                PLACEHOLDER,
            ),
            (
                RedactionRule::ProviderToken,
                r"\b(?:sk-[A-Za-z0-9_-]{16,}|gh[pousr]_[A-Za-z0-9]{16,}|github_pat_[A-Za-z0-9_]{20,}|AKIA[0-9A-Z]{16}|xox[baprs]-[A-Za-z0-9-]{10,})",
                PLACEHOLDER,
            ),
        ];

        let ordered = specs
            .into_iter()
            .map(|(rule, pattern, replacement)| {
                let regex = Regex::new(pattern).expect("内置脱敏正则必须可编译");
                (rule, regex, replacement)
            })
            .collect();

        CompiledRules {
            ordered,
            key_value_secret: Regex::new(
                r#"(?i)\b([a-z0-9_.-]*(?:secret|token|password|passwd|api[_-]?key|access[_-]?key|private[_-]?key|credential)[a-z0-9_.-]*)(\s*[=:]\s*)"?([^"'\s,;}]+)"?"#,
            )
            .expect("内置键值凭据正则必须可编译"),
            home_like: Regex::new(r"(?:/Users|/home)/[^/\s:\x22']+")
                .expect("内置私有路径正则必须可编译"),
        }
    })
}

/// 确定性脱敏器。
///
/// 无内部可变状态，可安全共享。
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    /// 需要额外替换为 `~` 的具体主目录路径。
    home: Option<String>,
}

impl Redactor {
    /// 创建只使用内置规则的脱敏器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 指定宿主主目录，使该路径被替换为 `~`。
    ///
    /// 内置规则已覆盖 `/Users/<name>` 与 `/home/<name>` 形态；显式提供主目录可以
    /// 处理非常规位置，例如容器内的自定义 HOME。
    pub fn with_home(mut self, home: impl Into<String>) -> Self {
        self.home = Some(home.into());
        self
    }

    /// 对文本执行全部规则。
    ///
    /// 规则顺序固定，因此相同输入总是得到相同输出与相同命中集合。
    pub fn redact(&self, input: &str) -> RedactionOutcome {
        let compiled = rules();
        let mut text = input.to_string();
        let mut applied = BTreeSet::new();

        // 键值对凭据先处理，避免值中的令牌被其他规则替换后键名规则再次命中。
        if compiled.key_value_secret.is_match(&text) {
            let mut hit = false;
            let replaced = compiled
                .key_value_secret
                .replace_all(&text, |caps: &regex::Captures<'_>| {
                    // `total_tokens=1500` 这类指标键名同样含 "token"，但纯数字值不是凭据。
                    // 保留它们，否则 Usage 证据会被误删。
                    if caps[3].chars().all(|ch| ch.is_ascii_digit()) {
                        return caps[0].to_string();
                    }
                    hit = true;
                    format!("{}{}{PLACEHOLDER}", &caps[1], &caps[2])
                })
                .into_owned();
            if hit {
                text = replaced;
                applied.insert(RedactionRule::KeyValueSecret);
            }
        }

        for (rule, regex, replacement) in &compiled.ordered {
            if !regex.is_match(&text) {
                continue;
            }
            text = regex.replace_all(&text, *replacement).into_owned();
            applied.insert(*rule);
        }

        // 私有路径最后处理，避免先把路径改写成 `~` 而让前面的规则漏掉其中的凭据。
        if let Some(home) = &self.home {
            if !home.is_empty() && text.contains(home.as_str()) {
                text = text.replace(home.as_str(), "~");
                applied.insert(RedactionRule::PrivatePath);
            }
        }
        if compiled.home_like.is_match(&text) {
            text = compiled.home_like.replace_all(&text, "~").into_owned();
            applied.insert(RedactionRule::PrivatePath);
        }

        RedactionOutcome {
            text,
            rules_version: REDACTION_RULES_VERSION,
            applied,
        }
    }

    /// 递归脱敏 JSON 值中的所有字符串。
    ///
    /// 对象键本身不改写（键名通常是结构信息而非内容），但键名暗示凭据时其值会被
    /// 整体替换，避免值的形态不符合任何令牌正则却仍是凭据。
    pub fn redact_json(
        &self,
        value: &serde_json::Value,
    ) -> (serde_json::Value, BTreeSet<RedactionRule>) {
        let mut applied = BTreeSet::new();
        let redacted = self.redact_json_inner(value, &mut applied);
        (redacted, applied)
    }

    /// [`Redactor::redact_json`] 的递归实现，命中规则累积到 `applied`。
    fn redact_json_inner(
        &self,
        value: &serde_json::Value,
        applied: &mut BTreeSet<RedactionRule>,
    ) -> serde_json::Value {
        match value {
            serde_json::Value::String(text) => {
                let outcome = self.redact(text);
                applied.extend(outcome.applied.iter().copied());
                serde_json::Value::String(outcome.text)
            }
            serde_json::Value::Array(items) => serde_json::Value::Array(
                items
                    .iter()
                    .map(|item| self.redact_json_inner(item, applied))
                    .collect(),
            ),
            serde_json::Value::Object(entries) => {
                let mut output = serde_json::Map::new();
                for (key, item) in entries {
                    // 只有字符串值才可能是凭据。`total_tokens` 之类的键名同样含 "token"，
                    // 但其数值是必须保留的 Usage 证据。
                    if is_secret_key(key) && item.is_string() {
                        applied.insert(RedactionRule::KeyValueSecret);
                        output.insert(key.clone(), serde_json::Value::String(PLACEHOLDER.into()));
                    } else {
                        output.insert(key.clone(), self.redact_json_inner(item, applied));
                    }
                }
                serde_json::Value::Object(output)
            }
            other => other.clone(),
        }
    }
}

/// 判断 JSON 键名是否暗示其值为凭据。
fn is_secret_key(key: &str) -> bool {
    let lowered = key.to_ascii_lowercase();
    [
        "secret",
        "token",
        "password",
        "passwd",
        "api_key",
        "apikey",
        "access_key",
        "accesskey",
        "private_key",
        "privatekey",
        "credential",
        "authorization",
        "cookie",
    ]
    .iter()
    .any(|needle| lowered.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redacts_provider_tokens() {
        let outcome = Redactor::new().redact("key is sk-abcdefghijklmnopqrstuvwx here");

        assert!(!outcome.text.contains("sk-abcdefghijklmnopqrstuvwx"));
        assert!(outcome.text.contains(PLACEHOLDER));
        assert!(outcome.applied.contains(&RedactionRule::ProviderToken));
    }

    #[test]
    fn redacts_github_and_aws_tokens() {
        let redactor = Redactor::new();

        let gh = redactor.redact("ghp_0123456789abcdefghij");
        assert!(!gh.text.contains("ghp_0123456789abcdefghij"));

        let aws = redactor.redact("AKIAIOSFODNN7EXAMPLE");
        assert!(!aws.text.contains("AKIAIOSFODNN7EXAMPLE"));
    }

    #[test]
    fn redacts_authorization_and_cookie_headers() {
        let input = "Authorization: Bearer abcdefghijklmnop\nCookie: session=abc123\nAccept: */*";
        let outcome = Redactor::new().redact(input);

        assert!(!outcome.text.contains("abcdefghijklmnop"));
        assert!(!outcome.text.contains("session=abc123"));
        // 非敏感请求头必须原样保留，脱敏不应破坏证据的可读性。
        assert!(outcome.text.contains("Accept: */*"));
        assert!(outcome
            .applied
            .contains(&RedactionRule::AuthorizationHeader));
        assert!(outcome.applied.contains(&RedactionRule::CookieHeader));
    }

    #[test]
    fn redacts_url_credentials() {
        let outcome = Redactor::new().redact("clone https://alice:hunter2@example.com/repo.git");

        assert!(!outcome.text.contains("hunter2"));
        assert!(!outcome.text.contains("alice:"));
        assert!(outcome
            .text
            .contains("https://[REDACTED]@example.com/repo.git"));
    }

    #[test]
    fn redacts_key_value_secrets() {
        let outcome = Redactor::new().redact("OPENAI_API_KEY=abcd1234efgh5678 MODEL=gpt");

        assert!(!outcome.text.contains("abcd1234efgh5678"));
        // 非凭据键值必须保留。
        assert!(outcome.text.contains("MODEL=gpt"));
        assert!(outcome.applied.contains(&RedactionRule::KeyValueSecret));
    }

    #[test]
    fn redacts_json_web_tokens() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dBjftJeZ4CVPmB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let outcome = Redactor::new().redact(jwt);

        assert!(!outcome.text.contains("eyJhbGciOiJIUzI1NiJ9"));
        assert!(outcome.applied.contains(&RedactionRule::JsonWebToken));
    }

    #[test]
    fn redacts_private_paths() {
        let outcome = Redactor::new().redact("failed at /Users/teriri/projects/app/src/main.rs");

        assert!(!outcome.text.contains("teriri"));
        assert!(outcome.text.contains("~/projects/app/src/main.rs"));
        assert!(outcome.applied.contains(&RedactionRule::PrivatePath));
    }

    #[test]
    fn redacts_explicit_home_directory() {
        let outcome = Redactor::new()
            .with_home("/opt/custom-home")
            .redact("config at /opt/custom-home/.lucia/config.toml");

        assert!(outcome.text.contains("~/.lucia/config.toml"));
        assert!(outcome.applied.contains(&RedactionRule::PrivatePath));
    }

    #[test]
    fn leaves_clean_text_untouched() {
        let input = "构建成功，运行了 12 个测试，耗时 3.4 秒。";
        let outcome = Redactor::new().redact(input);

        assert_eq!(outcome.text, input);
        assert!(!outcome.is_modified());
    }

    /// 同一输入必须始终得到同一输出与同一命中集合。
    #[test]
    fn redaction_is_deterministic() {
        let input = "Authorization: Bearer abcdefghijklmnop\nkey=sk-abcdefghijklmnopqrstuvwx\n/Users/alice/x";
        let redactor = Redactor::new();

        let first = redactor.redact(input);
        let second = redactor.redact(input);

        assert_eq!(first, second);
        // 二次脱敏应当是幂等的：已脱敏文本不再变化。
        let third = redactor.redact(&first.text);
        assert_eq!(third.text, first.text);
    }

    #[test]
    fn redacts_json_values_and_secret_keys() {
        let value = json!({
            "api_key": "abcd1234efgh5678",
            "message": "token is sk-abcdefghijklmnopqrstuvwx",
            "nested": {"password": "hunter2", "count": 3},
            "items": ["/Users/bob/file.txt"]
        });
        let (redacted, applied) = Redactor::new().redact_json(&value);

        assert_eq!(redacted["api_key"], PLACEHOLDER);
        assert_eq!(redacted["nested"]["password"], PLACEHOLDER);
        // 非敏感字段保持原值和类型。
        assert_eq!(redacted["nested"]["count"], 3);
        assert!(!redacted["message"]
            .as_str()
            .expect("应为字符串")
            .contains("sk-abcd"));
        assert!(!redacted["items"][0]
            .as_str()
            .expect("应为字符串")
            .contains("bob"));
        assert!(applied.contains(&RedactionRule::KeyValueSecret));
    }

    /// Usage 指标的键名同样含 "token"，但必须保留，否则 Mutator 失去成本证据。
    #[test]
    fn keeps_numeric_usage_metrics() {
        let outcome = Redactor::new().redact("total_tokens=1500 prompt_tokens=1200");
        assert_eq!(outcome.text, "total_tokens=1500 prompt_tokens=1200");
        assert!(!outcome.is_modified());

        let (redacted, _) = Redactor::new().redact_json(&json!({
            "total_tokens": 1500,
            "api_token": "abcd1234efgh5678",
        }));
        assert_eq!(redacted["total_tokens"], 1500);
        assert_eq!(redacted["api_token"], PLACEHOLDER);
    }

    /// 脱敏结果中不得残留任何已知凭据样本。
    #[test]
    fn no_secret_sample_survives_redaction() {
        let samples = [
            "sk-abcdefghijklmnopqrstuvwx",
            "ghp_0123456789abcdefghij",
            "AKIAIOSFODNN7EXAMPLE",
            "xoxb-1234567890-abcdefghij",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.abcdefghij",
        ];
        let redactor = Redactor::new();

        for sample in samples {
            let outcome = redactor.redact(&format!("value: {sample}"));
            assert!(
                !outcome.text.contains(sample),
                "凭据样本未被脱敏：{sample} -> {}",
                outcome.text
            );
        }
    }
}

//! 自进化链路的强类型标识。
//!
//! 所有标识都是 newtype 而非裸 `String`，因此把 `EpisodeId` 传给需要 `RunId`
//! 的位置会在编译期失败。序列化形态是普通字符串，反序列化时同样执行校验，
//! 非法值不会静默进入系统。
//!
//! 两个家族：
//!
//! - **带前缀标识**：`<prefix>_<8-64 位小写字母或数字>`，由随机数生成，
//!   不含时间戳、路径或用户名，因此标识本身不泄漏任何内容。
//! - **内容摘要**：`sha256:<64 位小写十六进制>`，由内容决定，见 M1-03。
//!
//! 各类型的 `PATTERN` 常量是跨语言校验的唯一事实来源，
//! [`id_json_schema`] 据此生成可供 TypeScript 使用的 JSON Schema。

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use thiserror::Error;
use uuid::Uuid;

/// 带前缀标识的正文允许长度下限。
const BODY_MIN: usize = 8;
/// 带前缀标识的正文允许长度上限。
const BODY_MAX: usize = 64;
/// 内容摘要使用的哈希算法前缀。
const DIGEST_ALGORITHM: &str = "sha256";
/// SHA-256 十六进制表示的固定长度。
const DIGEST_HEX_LEN: usize = 64;
/// 错误信息中回显原值的最大长度，避免超长输入淹没日志。
const ECHO_LIMIT: usize = 48;

/// 标识解析失败。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum InvalidEvolutionId {
    /// 缺少该类型要求的前缀。
    #[error("{type_name} 必须以 `{expected}_` 开头，实际收到 `{got}`")]
    MissingPrefix {
        /// 目标类型名。
        type_name: &'static str,
        /// 期望的前缀。
        expected: &'static str,
        /// 回显的原始输入。
        got: String,
    },
    /// 正文长度或字符集非法。
    #[error("{type_name} 的正文非法：{reason}（收到 `{got}`）")]
    InvalidBody {
        /// 目标类型名。
        type_name: &'static str,
        /// 具体原因。
        reason: String,
        /// 回显的原始输入。
        got: String,
    },
    /// 摘要格式非法。
    #[error("{type_name} 必须形如 `sha256:<64 位小写十六进制>`：{reason}（收到 `{got}`）")]
    InvalidDigest {
        /// 目标类型名。
        type_name: &'static str,
        /// 具体原因。
        reason: String,
        /// 回显的原始输入。
        got: String,
    },
}

/// 截断过长输入，供错误信息回显。
fn echo(value: &str) -> String {
    if value.chars().count() <= ECHO_LIMIT {
        return value.to_string();
    }
    let head: String = value.chars().take(ECHO_LIMIT).collect();
    format!("{head}…")
}

/// 校验带前缀标识的格式。
///
/// # Errors
///
/// 前缀不匹配、正文长度越界或含非法字符时返回对应的 [`InvalidEvolutionId`]。
fn validate_prefixed(
    value: &str,
    prefix: &'static str,
    type_name: &'static str,
) -> Result<(), InvalidEvolutionId> {
    let Some(body) = value
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('_'))
    else {
        return Err(InvalidEvolutionId::MissingPrefix {
            type_name,
            expected: prefix,
            got: echo(value),
        });
    };

    let length = body.len();
    if !(BODY_MIN..=BODY_MAX).contains(&length) {
        return Err(InvalidEvolutionId::InvalidBody {
            type_name,
            reason: format!("长度必须在 {BODY_MIN} 到 {BODY_MAX} 之间，实际为 {length}"),
            got: echo(value),
        });
    }
    if let Some(bad) = body
        .chars()
        .find(|ch| !ch.is_ascii_digit() && !ch.is_ascii_lowercase())
    {
        return Err(InvalidEvolutionId::InvalidBody {
            type_name,
            reason: format!("只允许小写字母和数字，出现了 `{bad}`"),
            got: echo(value),
        });
    }
    Ok(())
}

/// 定义一个带前缀的随机标识类型。
macro_rules! prefixed_id {
    ($name:ident, $prefix:literal, $doc:literal) => {
        #[doc = $doc]
        #[doc = ""]
        #[doc = concat!("序列化形态：`", $prefix, "_<8-64 位小写字母或数字>`。")]
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            /// 该类型的固定前缀。
            pub const PREFIX: &'static str = $prefix;

            /// 该类型的正则表达式，是跨语言校验的事实来源。
            pub const PATTERN: &'static str = concat!("^", $prefix, "_[0-9a-z]{8,64}$");

            #[doc = concat!("生成随机 `", stringify!($name), "`。")]
            ///
            /// 正文取自 UUID v4 的 32 位十六进制形式，不含时间、路径或用户信息。
            pub fn generate() -> Self {
                Self(format!("{}_{}", $prefix, Uuid::new_v4().simple()))
            }

            /// 校验并创建标识。
            ///
            /// # Errors
            ///
            /// 前缀缺失、正文长度越界或含非法字符时返回 [`InvalidEvolutionId`]。
            pub fn new(value: impl Into<String>) -> Result<Self, InvalidEvolutionId> {
                let value = value.into();
                validate_prefixed(&value, $prefix, stringify!($name))?;
                Ok(Self(value))
            }

            /// 返回标识的字符串视图。
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl std::str::FromStr for $name {
            type Err = InvalidEvolutionId;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl TryFrom<String> for $name {
            type Error = InvalidEvolutionId;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl TryFrom<&str> for $name {
            type Error = InvalidEvolutionId;

            fn try_from(value: &str) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                // 经由 `new` 校验，非法标识无法通过反序列化进入系统。
                let raw = String::deserialize(deserializer)?;
                Self::new(raw).map_err(serde::de::Error::custom)
            }
        }
    };
}

/// 定义一个内容摘要类型。
macro_rules! digest_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[doc = ""]
        #[doc = "序列化形态：`sha256:<64 位小写十六进制>`。"]
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            /// 该类型的正则表达式，是跨语言校验的事实来源。
            pub const PATTERN: &'static str = "^sha256:[0-9a-f]{64}$";

            /// 由 SHA-256 的十六进制表示创建摘要。
            ///
            /// 入参只含十六进制部分，不含 `sha256:` 前缀。
            ///
            /// # Errors
            ///
            /// 长度不是 64 或含非小写十六进制字符时返回 [`InvalidEvolutionId`]。
            pub fn from_sha256_hex(hex: impl AsRef<str>) -> Result<Self, InvalidEvolutionId> {
                let hex = hex.as_ref();
                validate_digest_hex(hex, stringify!($name))?;
                Ok(Self(format!("{DIGEST_ALGORITHM}:{hex}")))
            }

            /// 校验并创建摘要，入参为含前缀的完整形态。
            ///
            /// # Errors
            ///
            /// 缺少 `sha256:` 前缀或十六进制部分非法时返回 [`InvalidEvolutionId`]。
            pub fn new(value: impl Into<String>) -> Result<Self, InvalidEvolutionId> {
                let value = value.into();
                let Some(hex) = value.strip_prefix(concat!("sha256", ":")) else {
                    return Err(InvalidEvolutionId::InvalidDigest {
                        type_name: stringify!($name),
                        reason: "缺少 `sha256:` 前缀".to_string(),
                        got: echo(&value),
                    });
                };
                validate_digest_hex(hex, stringify!($name))?;
                Ok(Self(value))
            }

            /// 返回完整摘要字符串，含算法前缀。
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// 返回不含算法前缀的十六进制部分。
            pub fn hex(&self) -> &str {
                // 构造时已校验前缀存在，此处必定成立。
                &self.0[DIGEST_ALGORITHM.len() + 1..]
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl std::str::FromStr for $name {
            type Err = InvalidEvolutionId;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl TryFrom<String> for $name {
            type Error = InvalidEvolutionId;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl TryFrom<&str> for $name {
            type Error = InvalidEvolutionId;

            fn try_from(value: &str) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let raw = String::deserialize(deserializer)?;
                Self::new(raw).map_err(serde::de::Error::custom)
            }
        }
    };
}

/// 校验摘要的十六进制部分。
///
/// # Errors
///
/// 长度不等于 64 或含非小写十六进制字符时返回 [`InvalidEvolutionId`]。
fn validate_digest_hex(hex: &str, type_name: &'static str) -> Result<(), InvalidEvolutionId> {
    if hex.len() != DIGEST_HEX_LEN {
        return Err(InvalidEvolutionId::InvalidDigest {
            type_name,
            reason: format!("十六进制长度必须为 {DIGEST_HEX_LEN}，实际为 {}", hex.len()),
            got: echo(hex),
        });
    }
    if let Some(bad) = hex
        .chars()
        .find(|ch| !ch.is_ascii_digit() && !matches!(ch, 'a'..='f'))
    {
        return Err(InvalidEvolutionId::InvalidDigest {
            type_name,
            reason: format!("只允许小写十六进制字符，出现了 `{bad}`"),
            got: echo(hex),
        });
    }
    Ok(())
}

prefixed_id!(RunId, "run", "一次 Agent 运行的标识。");
prefixed_id!(EpisodeId, "ep", "一条运行证据 Episode 的标识。");
prefixed_id!(
    GenomeRevisionId,
    "grev",
    "Genome 的一次具体修订；与内容摘要不同，同一摘要可能有多次登记。"
);
prefixed_id!(MutationId, "mut", "一次变异提案的标识。");
prefixed_id!(EvaluationRunId, "evrun", "一次评测运行的标识。");
prefixed_id!(EvaluationReportId, "evrep", "一份评测报告的标识。");
prefixed_id!(DatasetVersionId, "dsv", "数据集某个版本的标识。");
prefixed_id!(ReleaseId, "rel", "一次发布的标识。");
prefixed_id!(AuditRecordId, "aud", "一条审计记录的标识。");

digest_id!(
    GenomeDigest,
    "Genome 行为字段的内容摘要；行为不变则摘要不变。"
);
digest_id!(ArtifactDigest, "CAS 中单个制品的内容摘要。");

/// 返回覆盖全部标识类型的 JSON Schema。
///
/// 各条目的 `pattern` 直接取自对应类型的 `PATTERN` 常量，因此 Rust 校验与
/// TypeScript 校验不会漂移。生成结果已固化到仓库根的
/// `schemas/evolution-ids.schema.json`，供前端与工具链直接引用。
pub fn id_json_schema() -> serde_json::Value {
    /// 构造单个标识类型的 schema 条目。
    fn entry(pattern: &str, description: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "string",
            "pattern": pattern,
            "description": description,
        })
    }

    serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://ascnet.dev/lucia/schemas/evolution-ids.schema.json",
        "title": "Lucia Evolution Identifiers",
        "description": "Lucia 自进化链路的强类型标识格式；由 agent-evolution-protocol 生成，请勿手工编辑。",
        "$defs": {
            "RunId": entry(RunId::PATTERN, "一次 Agent 运行的标识"),
            "EpisodeId": entry(EpisodeId::PATTERN, "一条运行证据 Episode 的标识"),
            "GenomeRevisionId": entry(GenomeRevisionId::PATTERN, "Genome 的一次具体修订"),
            "MutationId": entry(MutationId::PATTERN, "一次变异提案的标识"),
            "EvaluationRunId": entry(EvaluationRunId::PATTERN, "一次评测运行的标识"),
            "EvaluationReportId": entry(EvaluationReportId::PATTERN, "一份评测报告的标识"),
            "DatasetVersionId": entry(DatasetVersionId::PATTERN, "数据集某个版本的标识"),
            "ReleaseId": entry(ReleaseId::PATTERN, "一次发布的标识"),
            "AuditRecordId": entry(AuditRecordId::PATTERN, "一条审计记录的标识"),
            "GenomeDigest": entry(GenomeDigest::PATTERN, "Genome 行为字段的内容摘要"),
            "ArtifactDigest": entry(ArtifactDigest::PATTERN, "CAS 中单个制品的内容摘要"),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use regex::Regex;
    use std::str::FromStr;

    /// 对单个带前缀类型执行通用契约检查。
    macro_rules! check_prefixed {
        ($name:ident) => {{
            let id = $name::generate();
            let pattern = Regex::new($name::PATTERN).expect("PATTERN 必须可编译");
            assert!(
                pattern.is_match(id.as_str()),
                "{} 生成值 `{}` 不匹配自身 PATTERN",
                stringify!($name),
                id
            );

            // 文本 round-trip。
            let parsed = $name::from_str(id.as_str()).expect("自身生成值必须可解析");
            assert_eq!(parsed, id);

            // JSON round-trip，且序列化形态为普通字符串。
            let encoded = serde_json::to_string(&id).expect("应可序列化");
            assert_eq!(encoded, format!("\"{}\"", id.as_str()));
            let decoded: $name = serde_json::from_str(&encoded).expect("应可反序列化");
            assert_eq!(decoded, id);

            // 前缀错误必须被拒绝。
            assert!($name::new("wrongprefix_abcdefgh").is_err());
            // 正文过短必须被拒绝。
            assert!($name::new(format!("{}_abc", $name::PREFIX)).is_err());
            // 大写字符必须被拒绝。
            assert!($name::new(format!("{}_ABCDEFGH", $name::PREFIX)).is_err());
            // 反序列化同样执行校验。
            assert!(serde_json::from_str::<$name>("\"not-an-id\"").is_err());
        }};
    }

    #[test]
    fn prefixed_ids_satisfy_their_contract() {
        check_prefixed!(RunId);
        check_prefixed!(EpisodeId);
        check_prefixed!(GenomeRevisionId);
        check_prefixed!(MutationId);
        check_prefixed!(EvaluationRunId);
        check_prefixed!(EvaluationReportId);
        check_prefixed!(DatasetVersionId);
        check_prefixed!(ReleaseId);
        check_prefixed!(AuditRecordId);
    }

    #[test]
    fn generated_ids_are_unique_and_leak_nothing() {
        let first = RunId::generate();
        let second = RunId::generate();
        assert_ne!(first, second);

        // 正文应为纯随机十六进制，不含用户名、路径或时间戳分隔符。
        let body = first.as_str().trim_start_matches("run_");
        assert_eq!(body.len(), 32);
        assert!(body.chars().all(|ch| ch.is_ascii_hexdigit()));
    }

    #[test]
    fn digests_round_trip_and_reject_malformed_input() {
        let hex = "a".repeat(64);
        let digest = GenomeDigest::from_sha256_hex(&hex).expect("合法摘要应被接受");

        assert_eq!(digest.as_str(), format!("sha256:{hex}"));
        assert_eq!(digest.hex(), hex);
        assert!(Regex::new(GenomeDigest::PATTERN)
            .expect("PATTERN 必须可编译")
            .is_match(digest.as_str()));

        let encoded = serde_json::to_string(&digest).expect("应可序列化");
        let decoded: GenomeDigest = serde_json::from_str(&encoded).expect("应可反序列化");
        assert_eq!(decoded, digest);

        // 缺前缀、长度不符、大写十六进制都必须被拒绝。
        assert!(GenomeDigest::new(hex.clone()).is_err());
        assert!(GenomeDigest::from_sha256_hex("abc").is_err());
        assert!(GenomeDigest::from_sha256_hex("A".repeat(64)).is_err());
        assert!(serde_json::from_str::<ArtifactDigest>("\"sha1:abc\"").is_err());
    }

    #[test]
    fn error_messages_name_the_type_and_expectation() {
        let error = EpisodeId::new("run_abcdefgh").expect_err("前缀不符应报错");
        let text = error.to_string();

        assert!(text.contains("EpisodeId"), "错误应指明类型：{text}");
        assert!(text.contains("ep_"), "错误应指明期望前缀：{text}");
    }

    #[test]
    fn overlong_input_is_truncated_in_errors() {
        let error = RunId::new("x".repeat(500)).expect_err("非法输入应报错");
        assert!(
            error.to_string().len() < 200,
            "错误信息不应回显超长输入：{error}"
        );
    }

    /// 固化的 JSON Schema 必须与 Rust 定义一致。
    ///
    /// 设置 `UPDATE_SCHEMA=1` 重新生成该文件。
    #[test]
    fn checked_in_schema_matches_rust_definition() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../schemas/evolution-ids.schema.json"
        );
        let generated = format!(
            "{}\n",
            serde_json::to_string_pretty(&id_json_schema()).expect("schema 应可序列化")
        );

        if std::env::var("UPDATE_SCHEMA").is_ok() {
            std::fs::write(path, &generated).expect("应能写入 schema 文件");
            return;
        }

        let checked_in = std::fs::read_to_string(path)
            .expect("schemas/evolution-ids.schema.json 缺失，请以 UPDATE_SCHEMA=1 重新生成");
        assert_eq!(
            checked_in, generated,
            "固化的 schema 已过期，请以 UPDATE_SCHEMA=1 重新生成"
        );
    }

    /// Schema 中的每个 pattern 都必须能接受对应类型的真实取值。
    #[test]
    fn schema_patterns_accept_real_values() {
        let schema = id_json_schema();
        let defs = schema["$defs"].as_object().expect("应有 $defs");
        assert_eq!(defs.len(), 11, "11 个标识类型都应出现在 schema 中");

        let run_pattern = defs["RunId"]["pattern"].as_str().expect("应有 pattern");
        assert!(Regex::new(run_pattern)
            .expect("pattern 必须可编译")
            .is_match(RunId::generate().as_str()));

        let digest_pattern = defs["ArtifactDigest"]["pattern"]
            .as_str()
            .expect("应有 pattern");
        let digest = ArtifactDigest::from_sha256_hex("0".repeat(64)).expect("应可创建");
        assert!(Regex::new(digest_pattern)
            .expect("pattern 必须可编译")
            .is_match(digest.as_str()));
    }
}

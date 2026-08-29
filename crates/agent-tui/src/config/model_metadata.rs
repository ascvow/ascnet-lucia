//! 打包内置的 models.dev 模型能力快照解析。

use agent_core::ReasoningLevel;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;

/// 编译时嵌入的模型能力快照，运行时不访问网络。
const MODELS_DEV_SNAPSHOT: &str = include_str!("../../assets/models-dev.json");

/// TUI 使用的模型能力，只保留请求与展示需要的稳定字段。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ModelMetadata {
    /// 模型上下文窗口 token 上限。
    pub(crate) context_window: Option<u64>,
    /// 模型单次输出 token 上限。
    pub(crate) max_output_tokens: Option<u32>,
    /// models.dev 声明允许选择的推理强度，顺序即 TUI 循环顺序。
    pub(crate) reasoning_levels: Vec<ReasoningLevel>,
    /// 模型接受的输入模态；未知值会保留，供诊断与后续协议扩展使用。
    pub(crate) input_modalities: Vec<ModelModality>,
    /// 模型能够生成的输出模态；TUI 只展示当前协议可以消费的类型。
    pub(crate) output_modalities: Vec<ModelModality>,
}

/// models.dev 声明的模型输入或输出类型。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ModelModality {
    /// UTF-8 文本或降级为文本的普通文件。
    Text,
    /// 图片内容块。
    Image,
    /// 音频内容；当前 Lucia 模型协议尚未实现。
    Audio,
    /// 视频内容；当前 Lucia 模型协议尚未实现。
    Video,
    /// 原生 PDF 文档内容块。
    Pdf,
    /// 快照新增但当前版本尚不认识的类型。
    Unknown(String),
}

impl ModelModality {
    /// 从 models.dev 字符串创建稳定类型，未知值不丢弃也不导致启动失败。
    fn from_models_dev(value: String) -> Self {
        match value.as_str() {
            "text" => Self::Text,
            "image" => Self::Image,
            "audio" => Self::Audio,
            "video" => Self::Video,
            "pdf" => Self::Pdf,
            _ => Self::Unknown(value),
        }
    }

    /// 返回用于 TUI 能力摘要和错误消息的简体中文名称。
    pub(crate) fn label(&self) -> &str {
        match self {
            Self::Text => "文本",
            Self::Image => "图片",
            Self::Audio => "音频",
            Self::Video => "视频",
            Self::Pdf => "PDF",
            Self::Unknown(value) => value,
        }
    }
}

#[derive(Deserialize)]
struct ProviderRecord {
    #[serde(default)]
    models: HashMap<String, ModelRecord>,
}

#[derive(Deserialize)]
struct ModelRecord {
    #[serde(default)]
    reasoning: bool,
    #[serde(default)]
    reasoning_options: Vec<ReasoningOption>,
    #[serde(default)]
    limit: ModelLimit,
    #[serde(default)]
    modalities: ModelModalities,
}

#[derive(Default, Deserialize)]
struct ModelModalities {
    #[serde(default)]
    input: Vec<String>,
    #[serde(default)]
    output: Vec<String>,
}

#[derive(Deserialize)]
struct ReasoningOption {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    values: Vec<Option<String>>,
}

#[derive(Default, Deserialize)]
struct ModelLimit {
    context: Option<u64>,
    output: Option<u64>,
}

/// 从内置快照按模型 ID 读取能力，并优先使用指定的 models.dev 服务商消歧。
///
/// 指定服务商未命中时会按模型 ID 扫描全部服务商；仅有唯一结果时返回该配置，重复 ID
/// 必须通过 `models_dev_provider` 显式消歧。快照损坏时返回错误并阻止启动，避免打包
/// 数据静默失效。输出上限超过 Core 的 `u32` 协议范围时忽略该字段。
pub(crate) fn find_model_metadata(
    provider_id: &str,
    model_id: &str,
) -> Result<Option<ModelMetadata>> {
    find_model_metadata_in(MODELS_DEV_SNAPSHOT, provider_id, model_id)
}

/// 解析指定 JSON，供打包快照和测试夹具共用同一条严格读取路径。
fn find_model_metadata_in(
    snapshot: &str,
    provider_id: &str,
    model_id: &str,
) -> Result<Option<ModelMetadata>> {
    let providers: HashMap<String, ProviderRecord> =
        serde_json::from_str(snapshot).context("解析内置 models.dev 模型快照失败")?;
    let exact = providers
        .get(provider_id)
        .and_then(|provider| provider.models.get(model_id));
    let model = if let Some(model) = exact {
        model
    } else {
        let mut matches = providers
            .values()
            .filter_map(|provider| provider.models.get(model_id));
        let Some(unique) = matches.next() else {
            return Ok(None);
        };
        if matches.next().is_some() {
            return Ok(None);
        }
        unique
    };
    Ok(Some(ModelMetadata {
        context_window: positive(model.limit.context),
        max_output_tokens: positive(model.limit.output).and_then(|value| u32::try_from(value).ok()),
        reasoning_levels: reasoning_levels(model),
        input_modalities: modalities(&model.modalities.input),
        output_modalities: modalities(&model.modalities.output),
    }))
}

/// 保持快照声明顺序并去重，避免同一能力在 TUI 中重复展示。
fn modalities(values: &[String]) -> Vec<ModelModality> {
    let mut modalities = Vec::new();
    for value in values {
        let modality = ModelModality::from_models_dev(value.clone());
        if !modalities.contains(&modality) {
            modalities.push(modality);
        }
    }
    modalities
}

/// 零值代表该模型不具备对应文本 token 能力，不作为有效上限暴露。
fn positive(value: Option<u64>) -> Option<u64> {
    value.filter(|value| *value > 0)
}

/// 将 models.dev 的 effort、toggle 与 budget_tokens 规范为 Core 推理等级。
fn reasoning_levels(model: &ModelRecord) -> Vec<ReasoningLevel> {
    if !model.reasoning {
        return vec![ReasoningLevel::Off];
    }
    let mut levels = vec![ReasoningLevel::Off];
    for option in &model.reasoning_options {
        match option.kind.as_str() {
            "effort" => {
                for value in option.values.iter().flatten() {
                    let level = match value.as_str() {
                        "none" => Some(ReasoningLevel::Off),
                        "minimal" => Some(ReasoningLevel::Minimal),
                        "low" => Some(ReasoningLevel::Low),
                        "default" | "medium" => Some(ReasoningLevel::Medium),
                        "high" => Some(ReasoningLevel::High),
                        "xhigh" => Some(ReasoningLevel::XHigh),
                        "max" => Some(ReasoningLevel::Max),
                        _ => None,
                    };
                    if let Some(level) = level {
                        push_unique(&mut levels, level);
                    }
                }
            }
            "toggle" => {
                push_unique(&mut levels, ReasoningLevel::Off);
                push_unique(&mut levels, ReasoningLevel::Medium);
            }
            "budget_tokens" => {
                for level in [
                    ReasoningLevel::Off,
                    ReasoningLevel::Low,
                    ReasoningLevel::Medium,
                    ReasoningLevel::High,
                ] {
                    push_unique(&mut levels, level);
                }
            }
            _ => {}
        }
    }
    if model.reasoning_options.is_empty() {
        push_unique(&mut levels, ReasoningLevel::Medium);
    }
    levels
}

/// 保持 models.dev 声明顺序并去除不同选项类型带来的重复档位。
fn push_unique(levels: &mut Vec<ReasoningLevel>, level: ReasoningLevel) {
    if !levels.contains(&level) {
        levels.push(level);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// effort 枚举必须保留超出旧版 low/medium/high 的全部已知等级。
    #[test]
    fn parses_effort_levels_and_limits() {
        let metadata = find_model_metadata_in(
            r#"{"openai":{"models":{"gpt-test":{"reasoning":true,"reasoning_options":[{"type":"effort","values":["minimal","low","medium","high","xhigh","max"]}],"modalities":{"input":["text","image","pdf"],"output":["text","image","future"]},"limit":{"context":200000,"output":100000}}}}}"#,
            "openai",
            "gpt-test",
        )
        .expect("夹具应可解析")
        .expect("模型应存在");
        assert_eq!(metadata.context_window, Some(200_000));
        assert_eq!(metadata.max_output_tokens, Some(100_000));
        assert_eq!(
            metadata.input_modalities,
            vec![
                ModelModality::Text,
                ModelModality::Image,
                ModelModality::Pdf
            ]
        );
        assert_eq!(
            metadata.output_modalities,
            vec![
                ModelModality::Text,
                ModelModality::Image,
                ModelModality::Unknown("future".to_string())
            ]
        );
        assert_eq!(
            metadata.reasoning_levels,
            vec![
                ReasoningLevel::Off,
                ReasoningLevel::Minimal,
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
                ReasoningLevel::XHigh,
                ReasoningLevel::Max,
            ]
        );
    }

    /// 不支持推理的模型只能关闭，零 token 上限不得进入运行配置。
    #[test]
    fn disables_reasoning_and_ignores_zero_limits() {
        let metadata = find_model_metadata_in(
            r#"{"openai":{"models":{"image":{"reasoning":false,"limit":{"context":0,"output":0}}}}}"#,
            "openai",
            "image",
        )
        .expect("夹具应可解析")
        .expect("模型应存在");
        assert_eq!(metadata.context_window, None);
        assert_eq!(metadata.max_output_tokens, None);
        assert_eq!(metadata.reasoning_levels, vec![ReasoningLevel::Off]);
    }

    /// 只声明 reasoning=true 的模型仍应提供开关语义，不能只留下关闭档。
    #[test]
    fn reasoning_without_options_supports_off_and_medium() {
        let metadata = find_model_metadata_in(
            r#"{"openai":{"models":{"reasoner":{"reasoning":true}}}}"#,
            "openai",
            "reasoner",
        )
        .expect("夹具应可解析")
        .expect("模型应存在");

        assert_eq!(
            metadata.reasoning_levels,
            vec![ReasoningLevel::Off, ReasoningLevel::Medium]
        );
    }

    /// 服务商别名未命中时仍应按唯一模型 ID 读取能力，兼容 OpenAI 协议代理。
    #[test]
    fn falls_back_to_unique_model_id_across_providers() {
        let metadata = find_model_metadata_in(
            r#"{"ai-router":{"models":{"gpt-luna":{"reasoning":true,"limit":{"context":1050000}}}}}"#,
            "openai",
            "gpt-luna",
        )
        .expect("夹具应可解析")
        .expect("唯一模型 ID 应命中");

        assert_eq!(metadata.context_window, Some(1_050_000));
        assert_eq!(
            metadata.reasoning_levels,
            vec![ReasoningLevel::Off, ReasoningLevel::Medium]
        );
    }

    /// 多个服务商声明同一模型 ID 时不得任意选择，必须要求配置显式消歧。
    #[test]
    fn rejects_ambiguous_model_id_fallback() {
        let metadata = find_model_metadata_in(
            r#"{"first":{"models":{"shared":{"reasoning":true}}},"second":{"models":{"shared":{"reasoning":false}}}}"#,
            "missing",
            "shared",
        )
        .expect("夹具应可解析");

        assert!(metadata.is_none());
    }
}

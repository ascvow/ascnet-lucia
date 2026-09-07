//! 配置模型：TOML 文件 + 命令行覆盖 + 校验。
//!
//! 所有默认值集中在此处，README 中的默认值必须与本文件一致。

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// 默认配置文件名（位于 workspace 根目录）。
pub const DEFAULT_CONFIG_FILE: &str = "codesearch.toml";

/// 默认数据目录名（位于 workspace 根目录下）。
pub const DEFAULT_DATA_DIR: &str = ".codesearch";

/// 顶层配置。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// 索引与扫描配置。
    pub index: IndexConfig,
    /// 分块配置。
    pub chunk: ChunkConfig,
    /// Embedding Provider 配置。
    pub embedding: EmbeddingConfig,
    /// 检索与融合配置。
    pub retrieval: RetrievalConfig,
    /// 文件监听配置。
    pub watch: WatchConfig,
}

/// 扫描与过滤配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IndexConfig {
    /// 仅索引匹配这些 glob 的路径；为空表示不限制。
    pub include: Vec<String>,
    /// 额外排除的 glob（在默认噪声规则之上叠加）。
    pub exclude: Vec<String>,
    /// 是否启用默认噪声排除规则（构建产物、依赖目录、锁文件等）。
    pub default_excludes: bool,
    /// 敏感文件规则（与噪声规则分开配置；默认规则不保证识别全部秘密）。
    pub sensitive_excludes: Vec<String>,
    /// 是否启用默认敏感文件规则。
    pub default_sensitive_excludes: bool,
    /// 单文件大小上限（字节），超过则跳过并记录原因。
    pub max_file_size_bytes: u64,
    /// 是否读取 `.gitignore`。
    pub respect_gitignore: bool,
    /// 解析并发线程数上限；0 表示按 CPU 数自动决定（最多 8）。
    pub parallelism: usize,
    /// 校验扫描间隔（秒）：serve 模式下定期对全部文件重新计算 Hash，0 表示关闭。
    pub verify_interval_secs: u64,
    /// 搜索前是否先做一次增量目录扫描（仅写模式生效）。
    pub refresh_on_search: bool,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            include: Vec::new(),
            exclude: Vec::new(),
            default_excludes: true,
            sensitive_excludes: Vec::new(),
            default_sensitive_excludes: true,
            max_file_size_bytes: 1024 * 1024,
            respect_gitignore: true,
            parallelism: 0,
            verify_interval_secs: 0,
            refresh_on_search: true,
        }
    }
}

/// 默认噪声排除规则（glob，相对 workspace 根）。
pub const DEFAULT_EXCLUDES: &[&str] = &[
    "**/.git",
    "**/.hg",
    "**/.svn",
    "**/.codesearch",
    "**/node_modules",
    "**/target",
    "**/dist",
    "**/build",
    "**/out",
    "**/.next",
    "**/.nuxt",
    "**/.turbo",
    "**/.cache",
    "**/.parcel-cache",
    "**/__pycache__",
    "**/.venv",
    "**/venv",
    "**/.tox",
    "**/.mypy_cache",
    "**/.pytest_cache",
    "**/.gradle",
    "**/.idea",
    "**/.vscode",
    "**/.DS_Store",
    "**/Cargo.lock",
    "**/package-lock.json",
    "**/yarn.lock",
    "**/pnpm-lock.yaml",
    "**/bun.lock",
    "**/bun.lockb",
    "**/*.min.js",
    "**/*.min.css",
    "**/*.map",
];

/// 默认敏感文件规则（glob）。这不是安全边界，只是降低误索引凭证的概率。
pub const DEFAULT_SENSITIVE_EXCLUDES: &[&str] = &[
    "**/.env",
    "**/.env.*",
    "!**/.env.example",
    "!**/.env.sample",
    "!**/.env.template",
    "**/.envrc",
    "**/*.pem",
    "**/*.key",
    "**/*.p12",
    "**/*.pfx",
    "**/*.jks",
    "**/*.keystore",
    "**/*.kdbx",
    "**/id_rsa",
    "**/id_rsa.*",
    "**/id_ed25519",
    "**/id_ed25519.*",
    "**/id_ecdsa",
    "**/id_ecdsa.*",
    "**/.npmrc",
    "**/.pypirc",
    "**/.netrc",
    "**/.htpasswd",
    "**/credentials",
    "**/credentials.json",
    "**/credentials.toml",
    "**/service-account*.json",
    "**/secrets.*",
    "**/*.secret",
];

/// 分块配置。token 数均为估算值（无精确 tokenizer）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChunkConfig {
    /// 目标块大小（估算 token）。
    pub target_tokens: usize,
    /// 普通块上限（估算 token），超过则沿语法结构拆分。
    pub max_tokens: usize,
    /// 小于该值的相邻同作用域声明允许合并。
    pub merge_below_tokens: usize,
    /// 单个块允许的最大字符数（保守硬上限，防止无 tokenizer 时块过大）。
    pub max_chars: usize,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        Self {
            target_tokens: 500,
            max_tokens: 1200,
            merge_below_tokens: 120,
            max_chars: 8000,
        }
    }
}

/// Embedding Provider 类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// 未配置模型：仅符号与全文检索。
    None,
    /// OpenAI-compatible `/embeddings` HTTP 协议（本地或远程服务均可）。
    OpenaiCompatible,
}

/// Embedding 配置。API Key 只从环境变量读取。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EmbeddingConfig {
    /// Provider 类型。
    pub provider: ProviderKind,
    /// 服务地址，例如 `https://api.openai.com/v1` 或 `http://127.0.0.1:8080/v1`。
    pub base_url: String,
    /// 模型名。
    pub model: String,
    /// 存放 API Key 的环境变量名；为空表示不发送 Authorization 头。
    pub api_key_env: String,
    /// 期望向量维度；为空时以首个成功响应的维度为准并固定到 embedding space。
    ///
    /// 只用于校验与避免探测请求，默认**不会**写进请求体，见 [`Self::send_dimensions`]。
    pub dimensions: Option<usize>,
    /// 是否把 `dimensions` 写进请求体。
    ///
    /// 只有支持按维度截断的服务（如 OpenAI `text-embedding-3-*`）才应开启。
    /// 多数服务（含硅基流动 `BAAI/bge-m3`）对未知参数直接返回 400，
    /// 因此默认关闭：配置 `dimensions` 用于校验，不代表要求服务改变输出维度。
    pub send_dimensions: bool,
    /// 单次请求超时（秒）。
    pub timeout_secs: u64,
    /// 每批文档数。
    pub batch_size: usize,
    /// 最大并发请求数。
    pub max_concurrency: usize,
    /// 失败重试次数上限（仅对 429/5xx/网络错误重试）。
    pub max_retries: usize,
    /// 模型修订或缓存命名空间；变更后旧向量不再复用。
    pub cache_namespace: String,
    /// 查询输入前缀（部分模型要求区分 query/document）。
    pub query_prefix: String,
    /// 文档输入前缀。
    pub document_prefix: String,
    /// 单个文档输入的最大字符数，超过则在行边界截断并标记。
    pub max_input_chars: usize,
    /// 每个 chunk 允许的最大失败次数，超过后标记为 failed 不再重试。
    pub max_chunk_attempts: u32,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            provider: ProviderKind::None,
            base_url: String::new(),
            model: String::new(),
            api_key_env: String::new(),
            dimensions: None,
            send_dimensions: false,
            timeout_secs: 60,
            batch_size: 32,
            max_concurrency: 2,
            max_retries: 3,
            cache_namespace: String::new(),
            query_prefix: String::new(),
            document_prefix: String::new(),
            max_input_chars: 16_000,
            max_chunk_attempts: 5,
        }
    }
}

/// 检索配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetrievalConfig {
    /// FTS5 召回候选数。
    pub fts_top_k: usize,
    /// 向量召回候选数。
    pub vector_top_k: usize,
    /// 向量召回 over-fetch 的最大倍数（作用域过滤后不足时逐步放大）。
    pub vector_overfetch_max: usize,
    /// 融合后保留的候选数。
    pub candidate_limit: usize,
    /// 默认返回结果数。
    pub max_results: usize,
    /// 允许的最大返回结果数上限。
    pub max_results_cap: usize,
    /// 默认 token 预算（估算）。
    pub token_budget: usize,
    /// token 预算上限。
    pub token_budget_cap: usize,
    /// RRF 常数 k。
    pub rrf_k: f64,
    /// 精确通道权重。
    ///
    /// 默认与其他通道相等。精确命中不被语义结果淹没，靠的是并列时"精确优先"的排序规则，
    /// 而不是调高本项：本项会连带放大符号后缀、路径片段这类弱精确匹配，
    /// 实测在自然语言查询上会降低 Recall@1。
    pub weight_exact: f64,
    /// FTS 通道权重。
    pub weight_fts: f64,
    /// 向量通道权重。
    pub weight_vector: f64,
    /// 精确完整符号命中至少保留的结果数（不会被近似结果淹没）。
    pub exact_reserve: usize,
    /// 字面量搜索最多扫描的文件数。
    pub literal_scan_limit: usize,
    /// 散文（Markdown/纯文本）在融合后的降权系数。
    ///
    /// 自然语言查询与散文的向量相似度天然高于与代码的相似度，不修正会让 README
    /// 稳定压过真实实现。默认值由 34 条带真值的查询扫参得到：1.0 时 hit@1 为
    /// 62%，0.85 升到 68%，继续压到 0.6 不再有收益且会把「文档本身就是答案」
    /// 的情形挤出前 5。取 1.0 关闭该修正。详见 `retrieval::prior`。
    pub prose_weight: f64,
}

impl Default for RetrievalConfig {
    fn default() -> Self {
        Self {
            fts_top_k: 50,
            vector_top_k: 80,
            vector_overfetch_max: 8,
            candidate_limit: 40,
            max_results: 10,
            max_results_cap: 50,
            token_budget: 6000,
            token_budget_cap: 60_000,
            rrf_k: 60.0,
            weight_exact: 1.0,
            weight_fts: 1.0,
            weight_vector: 1.0,
            exact_reserve: 3,
            literal_scan_limit: 5000,
            prose_weight: 0.85,
        }
    }
}

/// 文件监听配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WatchConfig {
    /// 去抖窗口（毫秒）。
    pub debounce_ms: u64,
    /// 待处理路径队列上限，超过后退化为整目录重扫。
    pub max_pending_paths: usize,
    /// 检查点最少间隔（秒）。
    pub checkpoint_interval_secs: u64,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            debounce_ms: 300,
            max_pending_paths: 4096,
            checkpoint_interval_secs: 30,
        }
    }
}

impl Config {
    /// 从 TOML 文本解析配置并校验。
    pub fn from_toml(text: &str) -> Result<Self> {
        let config: Config = toml::from_str(text)
            .map_err(|error| Error::Config(format!("TOML 解析失败: {error}")))?;
        config.validate()?;
        Ok(config)
    }

    /// 读取配置文件；文件不存在时返回默认配置。
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::from_toml(&text)
                .map_err(|error| Error::Config(format!("{}: {error}", path.display()))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let config = Config::default();
                config.validate()?;
                Ok(config)
            }
            Err(error) => Err(Error::io(format!("读取配置 {}", path.display()), error)),
        }
    }

    /// 校验配置取值范围；错误信息面向用户。
    pub fn validate(&self) -> Result<()> {
        if self.chunk.target_tokens == 0 || self.chunk.max_tokens == 0 {
            return Err(Error::Config(
                "chunk.target_tokens 与 chunk.max_tokens 必须大于 0".into(),
            ));
        }
        if self.chunk.max_tokens < self.chunk.target_tokens {
            return Err(Error::Config(
                "chunk.max_tokens 不能小于 chunk.target_tokens".into(),
            ));
        }
        if self.chunk.max_chars < 256 {
            return Err(Error::Config("chunk.max_chars 不能小于 256".into()));
        }
        if self.index.max_file_size_bytes == 0 {
            return Err(Error::Config("index.max_file_size_bytes 必须大于 0".into()));
        }
        if self.embedding.provider != ProviderKind::None {
            if self.embedding.base_url.trim().is_empty() {
                return Err(Error::Config("embedding.base_url 不能为空".into()));
            }
            if self.embedding.model.trim().is_empty() {
                return Err(Error::Config("embedding.model 不能为空".into()));
            }
            if self.embedding.batch_size == 0 {
                return Err(Error::Config("embedding.batch_size 必须大于 0".into()));
            }
            if self.embedding.max_concurrency == 0 {
                return Err(Error::Config("embedding.max_concurrency 必须大于 0".into()));
            }
            if self.embedding.timeout_secs == 0 {
                return Err(Error::Config("embedding.timeout_secs 必须大于 0".into()));
            }
            if self.embedding.max_input_chars < 512 {
                return Err(Error::Config(
                    "embedding.max_input_chars 不能小于 512".into(),
                ));
            }
            if matches!(self.embedding.dimensions, Some(0)) {
                return Err(Error::Config("embedding.dimensions 必须大于 0".into()));
            }
            if looks_like_secret(&self.embedding.api_key_env) {
                return Err(Error::Config(
                    "embedding.api_key_env 应为环境变量名，而不是密钥本身".into(),
                ));
            }
        }
        let retrieval = &self.retrieval;
        if retrieval.max_results == 0 || retrieval.max_results_cap == 0 {
            return Err(Error::Config("retrieval.max_results 必须大于 0".into()));
        }
        if retrieval.max_results > retrieval.max_results_cap {
            return Err(Error::Config(
                "retrieval.max_results 不能超过 max_results_cap".into(),
            ));
        }
        if retrieval.token_budget == 0 || retrieval.token_budget > retrieval.token_budget_cap {
            return Err(Error::Config(
                "retrieval.token_budget 必须在 (0, token_budget_cap] 内".into(),
            ));
        }
        if retrieval.rrf_k <= 0.0 {
            return Err(Error::Config("retrieval.rrf_k 必须大于 0".into()));
        }
        if retrieval.candidate_limit == 0 || retrieval.fts_top_k == 0 || retrieval.vector_top_k == 0
        {
            return Err(Error::Config("retrieval 候选数必须大于 0".into()));
        }
        if retrieval.vector_overfetch_max == 0 {
            return Err(Error::Config(
                "retrieval.vector_overfetch_max 必须大于 0".into(),
            ));
        }
        if self.watch.debounce_ms == 0 {
            return Err(Error::Config("watch.debounce_ms 必须大于 0".into()));
        }
        Ok(())
    }

    /// 实际使用的解析并发数。
    pub fn parallelism(&self) -> usize {
        if self.index.parallelism > 0 {
            return self.index.parallelism;
        }
        std::thread::available_parallelism()
            .map(|n| n.get().clamp(1, 8))
            .unwrap_or(2)
    }

    /// 合并后的噪声排除规则。
    pub fn effective_excludes(&self) -> Vec<String> {
        let mut patterns = Vec::new();
        if self.index.default_excludes {
            patterns.extend(DEFAULT_EXCLUDES.iter().map(|s| s.to_string()));
        }
        patterns.extend(self.index.exclude.iter().cloned());
        patterns
    }

    /// 合并后的敏感文件规则。
    pub fn effective_sensitive_excludes(&self) -> Vec<String> {
        let mut patterns = Vec::new();
        if self.index.default_sensitive_excludes {
            patterns.extend(DEFAULT_SENSITIVE_EXCLUDES.iter().map(|s| s.to_string()));
        }
        patterns.extend(self.index.sensitive_excludes.iter().cloned());
        patterns
    }
}

/// 粗略判断字符串是否像密钥本身而不是环境变量名。
fn looks_like_secret(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() {
        return false;
    }
    let valid_env_name = value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !value.chars().next().is_some_and(|c| c.is_ascii_digit());
    !valid_env_name || value.starts_with("sk-") || value.len() > 64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        Config::default().validate().expect("默认配置必须合法");
    }

    #[test]
    fn rejects_secret_in_api_key_env() {
        let text = r#"
[embedding]
provider = "openai_compatible"
base_url = "http://127.0.0.1:8080/v1"
model = "test"
api_key_env = "sk-abcdef0123456789"
"#;
        let error = Config::from_toml(text).expect_err("应拒绝把密钥写进配置");
        assert!(error.to_string().contains("api_key_env"));
    }

    #[test]
    fn rejects_unknown_fields() {
        let error = Config::from_toml("[index]\nfoo = 1\n").expect_err("未知字段应报错");
        assert!(matches!(error, Error::Config(_)));
    }

    #[test]
    fn parses_partial_config_with_defaults() {
        let config = Config::from_toml("[chunk]\ntarget_tokens = 300\n").expect("部分配置应可解析");
        assert_eq!(config.chunk.target_tokens, 300);
        assert_eq!(config.chunk.max_tokens, ChunkConfig::default().max_tokens);
        assert_eq!(config.embedding.provider, ProviderKind::None);
    }

    /// 示例配置必须始终可解析。
    ///
    /// `deny_unknown_fields` 意味着示例里任何过时字段都会让用户的配置直接报错，
    /// 因此这份文档必须随代码一起验证，而不是靠人工同步。
    #[test]
    fn example_config_file_stays_valid() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("codesearch.example.toml");
        let text = std::fs::read_to_string(&path).expect("读取示例配置");
        let config = Config::from_toml(&text).expect("示例配置必须可解析");
        config.validate().expect("示例配置必须合法");
        // 示例中只能出现环境变量名，不能出现真实凭证。
        assert!(!text.contains("sk-"), "示例配置不得包含真实凭证");
        assert_eq!(config.embedding.api_key_env, "SILICONFLOW_API_KEY");
        assert_eq!(config.embedding.dimensions, Some(1024));
        assert!(
            !config.embedding.send_dimensions,
            "硅基流动会拒绝请求体中的 dimensions，示例必须保持关闭"
        );
    }
}

//! 语言识别与文件可索引性判定。
//!
//! 判定采用**扩展名白名单**而不是黑名单。原因是宿主的 `read_file` 只在读取时才暴露文件
//! 内容与体量（`FileEntry` 没有 size / mtime，见 `docs/code-context-engine/architecture.md`
//! 的 Recommended 改动），因此"先读了再判断要不要"这条路会为每个未知扩展名付出一次完整
//! 读取的代价，还可能把一个几百 MB 的产物读进 64 MiB 的线性内存。白名单把这个风险挡在
//! 读取之前。
//!
//! # 三类结果
//!
//! - [`PathClass::Indexable`]：进入索引，附带识别出的语言。
//! - [`PathClass::Sensitive`]：可能含凭证，**任何模式下都不进入索引**。
//! - [`PathClass::Skipped`]：二进制、生成产物或未识别扩展名。

use serde::{Deserialize, Serialize};

/// 索引覆盖的语言。
///
/// 枚举同时承担两个职责：标注 chunk 的语言，以及声明该语言是否有 Tier 2 结构化解析器
/// （[`Language::supports_structural_parsing`]）。未覆盖结构化解析的语言仍然进入索引，
/// 只是走 Tier 3 文本分块——它们的标识符与文档注释依然可被词法检索命中。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    /// Rust。
    Rust,
    /// TypeScript，含 `.tsx`。
    TypeScript,
    /// JavaScript，含 `.jsx` 与 `.mjs`。
    JavaScript,
    /// Python。
    Python,
    /// Go。
    Go,
    /// Vue 单文件组件。
    Vue,
    /// Java。
    Java,
    /// PHP。
    Php,
    /// C。
    C,
    /// C++。
    Cpp,
    /// C#。
    CSharp,
    /// Ruby。
    Ruby,
    /// Swift。
    Swift,
    /// Kotlin。
    Kotlin,
    /// Shell 脚本。
    Shell,
    /// SQL。
    Sql,
    /// Markdown 文档。
    Markdown,
    /// JSON 配置。
    Json,
    /// TOML 配置。
    Toml,
    /// YAML 配置。
    Yaml,
    /// HTML。
    Html,
    /// CSS 及其预处理器方言。
    Css,
    /// 未归类的纯文本。
    Text,
}

impl Language {
    /// 返回稳定的机器可读名称，用于 `CodeChunk::language` 与工具返回值。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::TypeScript => "typescript",
            Self::JavaScript => "javascript",
            Self::Python => "python",
            Self::Go => "go",
            Self::Vue => "vue",
            Self::Java => "java",
            Self::Php => "php",
            Self::C => "c",
            Self::Cpp => "cpp",
            Self::CSharp => "csharp",
            Self::Ruby => "ruby",
            Self::Swift => "swift",
            Self::Kotlin => "kotlin",
            Self::Shell => "shell",
            Self::Sql => "sql",
            Self::Markdown => "markdown",
            Self::Json => "json",
            Self::Toml => "toml",
            Self::Yaml => "yaml",
            Self::Html => "html",
            Self::Css => "css",
            Self::Text => "text",
        }
    }

    /// 该语言是否有 Tier 2 结构化解析器。
    ///
    /// 返回 `false` 不代表文件被排除，只代表它走 Tier 3 文本分块。V1 的覆盖范围依据
    /// `architecture.md` 第 12 问确定。
    pub fn supports_structural_parsing(self) -> bool {
        matches!(
            self,
            Self::Rust | Self::TypeScript | Self::JavaScript | Self::Python | Self::Go | Self::Vue
        )
    }
}

/// 文件被跳过的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// 已知的二进制扩展名。
    Binary,
    /// 依赖锁文件等体量大且无检索价值的生成产物。
    Generated,
    /// 扩展名不在白名单内。
    Unrecognized,
}

impl SkipReason {
    /// 返回稳定的机器可读名称。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Binary => "binary",
            Self::Generated => "generated",
            Self::Unrecognized => "unrecognized",
        }
    }
}

/// 一个路径的索引归类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathClass {
    /// 可以进入索引。
    Indexable(Language),
    /// 命中敏感文件模式。
    Sensitive,
    /// 不进入索引。
    Skipped(SkipReason),
}

/// 无扩展名但确定为文本的常见文件名（小写比较）。
const KNOWN_TEXT_FILENAMES: &[(&str, Language)] = &[
    ("makefile", Language::Shell),
    ("dockerfile", Language::Shell),
    ("containerfile", Language::Shell),
    ("justfile", Language::Shell),
    ("rakefile", Language::Ruby),
    ("gemfile", Language::Ruby),
    ("readme", Language::Markdown),
    ("license", Language::Text),
    ("notice", Language::Text),
    ("changelog", Language::Markdown),
    ("codeowners", Language::Text),
];

/// 扩展名 → 语言白名单（小写比较）。
const EXTENSION_LANGUAGES: &[(&str, Language)] = &[
    ("rs", Language::Rust),
    ("ts", Language::TypeScript),
    ("tsx", Language::TypeScript),
    ("mts", Language::TypeScript),
    ("cts", Language::TypeScript),
    ("js", Language::JavaScript),
    ("jsx", Language::JavaScript),
    ("mjs", Language::JavaScript),
    ("cjs", Language::JavaScript),
    ("py", Language::Python),
    ("pyi", Language::Python),
    ("go", Language::Go),
    ("vue", Language::Vue),
    ("nvue", Language::Vue),
    ("java", Language::Java),
    ("php", Language::Php),
    ("c", Language::C),
    ("h", Language::C),
    ("cc", Language::Cpp),
    ("cpp", Language::Cpp),
    ("cxx", Language::Cpp),
    ("hpp", Language::Cpp),
    ("hh", Language::Cpp),
    ("cs", Language::CSharp),
    ("rb", Language::Ruby),
    ("swift", Language::Swift),
    ("kt", Language::Kotlin),
    ("kts", Language::Kotlin),
    ("sh", Language::Shell),
    ("bash", Language::Shell),
    ("zsh", Language::Shell),
    ("fish", Language::Shell),
    ("sql", Language::Sql),
    ("md", Language::Markdown),
    ("mdx", Language::Markdown),
    ("json", Language::Json),
    ("jsonc", Language::Json),
    ("toml", Language::Toml),
    ("yaml", Language::Yaml),
    ("yml", Language::Yaml),
    ("html", Language::Html),
    ("htm", Language::Html),
    ("css", Language::Css),
    ("scss", Language::Css),
    ("sass", Language::Css),
    ("less", Language::Css),
    ("txt", Language::Text),
    ("wit", Language::Text),
    ("proto", Language::Text),
    ("graphql", Language::Text),
    ("gql", Language::Text),
];

/// 已知二进制扩展名（小写比较）。
///
/// 这些扩展名在读取前就被排除。宿主的 `read_file` 对二进制会在 UTF-8 校验上失败，
/// 因此不列举也不会污染索引；提前排除是为了省下一次注定失败的宿主调用，以及避免把
/// 大体量文件读进线性内存。
const BINARY_EXTENSIONS: &[&str] = &[
    "png",
    "jpg",
    "jpeg",
    "gif",
    "bmp",
    "ico",
    "webp",
    "avif",
    "tiff",
    "svg",
    "pdf",
    "zip",
    "gz",
    "tgz",
    "bz2",
    "xz",
    "zst",
    "7z",
    "rar",
    "tar",
    "exe",
    "dll",
    "so",
    "dylib",
    "a",
    "o",
    "obj",
    "lib",
    "bin",
    "wasm",
    "class",
    "jar",
    "war",
    "pyc",
    "pyo",
    "pdb",
    "node",
    "onnx",
    "gguf",
    "safetensors",
    "pt",
    "pth",
    "ckpt",
    "npy",
    "npz",
    "woff",
    "woff2",
    "ttf",
    "otf",
    "eot",
    "mp3",
    "mp4",
    "avi",
    "mov",
    "mkv",
    "webm",
    "wav",
    "flac",
    "ogg",
    "db",
    "sqlite",
    "sqlite3",
    "mdb",
    "dat",
    "bak",
    "swp",
    "ds_store",
];

/// 体量大且无检索价值的生成产物文件名（小写比较）。
const GENERATED_FILENAMES: &[&str] = &[
    "cargo.lock",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lock",
    "bun.lockb",
    "composer.lock",
    "gemfile.lock",
    "poetry.lock",
    "pdm.lock",
    "go.sum",
];

/// 无条件视为敏感的精确文件名（小写比较）。
const SENSITIVE_FILENAMES: &[&str] = &[
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
    ".netrc",
    ".pgpass",
    ".npmrc",
    ".htpasswd",
    ".dockercfg",
];

/// 无条件视为敏感的扩展名（小写比较）。
const SENSITIVE_EXTENSIONS: &[&str] = &[
    "pem", "key", "p12", "pfx", "jks", "keystore", "asc", "gpg", "kdbx",
];

/// 返回路径的最后一段。
fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// 返回小写扩展名；无扩展名时为 `None`。
///
/// 以点开头的文件（`.gitignore`）不把整个名字当作扩展名。
fn extension(name: &str) -> Option<String> {
    let stem = name.strip_prefix('.').unwrap_or(name);
    let (_, ext) = stem.rsplit_once('.')?;
    if ext.is_empty() {
        return None;
    }
    Some(ext.to_ascii_lowercase())
}

/// 判断文件名是否命中敏感模式。
///
/// # 为什么 `credential*` / `secret*` 只作用于非源码文件
///
/// 需求要求过滤 `credential*` 与 `secret*`。直接按前缀过滤会把 `secrets.rs`、
/// `credentials.ts` 这类**定义凭证处理逻辑的源码**一并排除——而 Agent 恰恰经常需要检索
/// 它们，且它们存放的是代码不是凭证值。真正的风险集中在 `secrets.json`、`credentials.yaml`
/// 这类数据与配置文件。因此这两条前缀规则只在文件不属于已识别源码语言时生效。
///
/// `.env`、`*.pem`、`*.key` 等则无条件排除：这些形态不存在"其实是源码"的情况。
fn is_sensitive(name_lower: &str, extension: Option<&str>, language: Option<Language>) -> bool {
    if name_lower == ".env" || name_lower.starts_with(".env.") || name_lower.ends_with(".env") {
        return true;
    }
    if SENSITIVE_FILENAMES.contains(&name_lower) {
        return true;
    }
    if extension.is_some_and(|ext| SENSITIVE_EXTENSIONS.contains(&ext)) {
        return true;
    }
    // 源码语言豁免前缀规则，理由见函数级文档。
    let is_source = language.is_some_and(|language| {
        !matches!(
            language,
            Language::Json | Language::Toml | Language::Yaml | Language::Text
        )
    });
    if is_source {
        return false;
    }
    name_lower.starts_with("credential") || name_lower.starts_with("secret")
}

/// 对仓库相对路径做索引归类。
///
/// 输入是仓库相对路径（例如 `src/permission/manager.rs`）。判定只依赖路径本身，不读取
/// 文件内容——这是它能在读取前挡住二进制与敏感文件的前提。
pub fn classify(path: &str) -> PathClass {
    let name = file_name(path);
    let name_lower = name.to_ascii_lowercase();
    let extension = extension(&name_lower);
    let extension_ref = extension.as_deref();

    if GENERATED_FILENAMES.contains(&name_lower.as_str()) {
        return PathClass::Skipped(SkipReason::Generated);
    }

    let language = extension_ref
        .and_then(|ext| {
            EXTENSION_LANGUAGES
                .iter()
                .find(|(candidate, _)| *candidate == ext)
                .map(|(_, language)| *language)
        })
        .or_else(|| {
            // 无扩展名时才回退到已知文件名表，避免 `readme.md` 被这张表覆盖成 Markdown 之外的判定。
            if extension_ref.is_some() {
                return None;
            }
            KNOWN_TEXT_FILENAMES
                .iter()
                .find(|(candidate, _)| *candidate == name_lower)
                .map(|(_, language)| *language)
        });

    // 敏感判定必须先于可索引判定：`config.key` 有已知扩展名，但绝不能进索引。
    if is_sensitive(&name_lower, extension_ref, language) {
        return PathClass::Sensitive;
    }
    if extension_ref.is_some_and(|ext| BINARY_EXTENSIONS.contains(&ext)) {
        return PathClass::Skipped(SkipReason::Binary);
    }
    match language {
        Some(language) => PathClass::Indexable(language),
        None => PathClass::Skipped(SkipReason::Unrecognized),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 断言路径被判定为可索引且语言正确。
    fn assert_language(path: &str, expected: Language) {
        assert_eq!(
            classify(path),
            PathClass::Indexable(expected),
            "路径 {path} 的语言判定不符"
        );
    }

    /// 主要语言必须被正确识别。
    #[test]
    fn recognizes_primary_languages() {
        assert_language("src/lib.rs", Language::Rust);
        assert_language("src/app.ts", Language::TypeScript);
        assert_language("src/App.tsx", Language::TypeScript);
        assert_language("scripts/build.mjs", Language::JavaScript);
        assert_language("tool/main.py", Language::Python);
        assert_language("cmd/server/main.go", Language::Go);
        assert_language("src/App.vue", Language::Vue);
        assert_language("src/App.nvue", Language::Vue);
    }

    /// 已实现解析器的语言声明结构化能力，Vue/nvue 共享 SFC 适配器。
    #[test]
    fn only_tier_two_languages_support_structural_parsing() {
        for language in [
            Language::Rust,
            Language::TypeScript,
            Language::JavaScript,
            Language::Python,
            Language::Go,
            Language::Vue,
        ] {
            assert!(language.supports_structural_parsing(), "{language:?}");
        }
        for language in [
            Language::Java,
            Language::Markdown,
            Language::Json,
            Language::Text,
        ] {
            assert!(!language.supports_structural_parsing(), "{language:?}");
        }
    }

    /// 大小写不同的扩展名必须得到同一判定。
    #[test]
    fn extension_matching_is_case_insensitive() {
        assert_language("src/Main.RS", Language::Rust);
        assert_eq!(
            classify("assets/Logo.PNG"),
            PathClass::Skipped(SkipReason::Binary)
        );
    }

    /// 无扩展名的常见文本文件必须可索引。
    #[test]
    fn recognizes_extensionless_text_files() {
        assert_language("Makefile", Language::Shell);
        assert_language("docker/Dockerfile", Language::Shell);
        assert_language("LICENSE", Language::Text);
    }

    /// 二进制与生成产物必须在读取前被排除。
    #[test]
    fn excludes_binary_and_generated_files() {
        assert_eq!(
            classify("target/plugin.wasm"),
            PathClass::Skipped(SkipReason::Binary)
        );
        assert_eq!(
            classify("models/qwen.gguf"),
            PathClass::Skipped(SkipReason::Binary)
        );
        assert_eq!(
            classify("Cargo.lock"),
            PathClass::Skipped(SkipReason::Generated)
        );
        assert_eq!(
            classify("bun.lockb"),
            PathClass::Skipped(SkipReason::Generated)
        );
    }

    /// 未识别扩展名必须跳过，而不是当作文本读取。
    #[test]
    fn unrecognized_extensions_are_skipped() {
        assert_eq!(
            classify("data/blob.xyz"),
            PathClass::Skipped(SkipReason::Unrecognized)
        );
        assert_eq!(
            classify(".gitignore"),
            PathClass::Skipped(SkipReason::Unrecognized)
        );
    }

    /// 凭证类文件必须被判定为敏感。
    #[test]
    fn detects_sensitive_files() {
        for path in [
            ".env",
            ".env.local",
            "config/production.env",
            "certs/server.pem",
            "certs/private.key",
            "keys/store.p12",
            "deploy/id_rsa",
            ".npmrc",
            "config/secrets.json",
            "config/credentials.yaml",
        ] {
            assert_eq!(
                classify(path),
                PathClass::Sensitive,
                "路径 {path} 应判定为敏感"
            );
        }
    }

    /// 敏感判定必须优先于扩展名白名单。
    #[test]
    fn sensitive_check_precedes_language_detection() {
        // `.key` 与 `.pem` 都不在语言白名单里，但 `secrets.json` 的扩展名是被支持的，
        // 这条断言保证顺序不会被后续改动写反。
        assert_eq!(classify("config/secrets.json"), PathClass::Sensitive);
        assert_ne!(
            classify("config/secrets.json"),
            PathClass::Indexable(Language::Json)
        );
    }

    /// 定义凭证逻辑的源码不得被前缀规则误伤。
    #[test]
    fn source_files_named_after_secrets_stay_indexable() {
        assert_language("src/secrets.rs", Language::Rust);
        assert_language("src/credentials.ts", Language::TypeScript);
        assert_language("app/secret_manager.py", Language::Python);
    }

    /// 路径中的目录名不得影响文件判定。
    #[test]
    fn classification_uses_file_name_only() {
        assert_language("secret/handler.rs", Language::Rust);
        assert_language("node_modules_docs/guide.md", Language::Markdown);
    }
}

//! 宿主执行的受限 HTTP 请求能力。
//!
//! # 为什么由 Host 执行而不是把网络交给 Guest
//!
//! 插件需要访问用户自配的 OpenAI 兼容 `/v1/embeddings` 端点，但 `adr-api-embedding.md`
//! 已经确定：**密钥不进入 Guest**。这与 `host-model-complete` 是同一条范式——Guest 只提交
//! 「做什么」，「往哪发」和「用什么身份」由 Host 从应用配置注入。
//!
//! 因此这里的请求形状是刻意受限的：Guest 给出方法、**相对路径**、非敏感 header 与正文；
//! base URL、`Authorization` 由 Host 补齐。Guest 无法构造一个指向任意地址的请求，
//! 也无法读回注入的凭证。
//!
//! # 三道闸门
//!
//! 1. **manifest allowlist**：`[capabilities.http] allow_hosts` 是用户与审计者能看到的
//!    授权边界。Host 会用它校验自己注入的 base URL——声明与实际目的地不一致时拒绝。
//! 2. **执行策略**：`allow_network` 为假时整条能力关闭，与档位限额一并生效。
//! 3. **请求整形**：方法白名单、路径不得逃逸、敏感 header 不接受 Guest 覆盖、
//!    请求与响应体积上限、超时、不跟随重定向。
//!
//! 第 3 条里「不跟随重定向」值得单独说明：跟随重定向会让一个 allowlist 内的主机把请求
//! 引到 allowlist 外，把前两道闸门一并绕过。

use crate::capability::{capability_denied, parse_request, CapabilityState};
use crate::manifest::HttpCapabilitySection;
use crate::services::HttpHostServices;
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;

/// Guest 单次 HTTP 请求 JSON 的字节上限。
const MAX_HTTP_REQUEST_BYTES: usize = 8 * 1024 * 1024;

/// Guest 可提交的 header 条数上限。
const MAX_HTTP_HEADERS: usize = 32;

/// 单个 header 名或值的字节上限。
const MAX_HTTP_HEADER_BYTES: usize = 8 * 1024;

/// 相对路径的字节上限。
const MAX_HTTP_PATH_BYTES: usize = 4 * 1024;

/// Guest 允许使用的 HTTP 方法。
///
/// 不含 `CONNECT` 与 `TRACE`：前者是代理隧道语义，后者会回显请求内容（含 Host 注入的
/// 凭证 header），等于给 Guest 开了一条读回密钥的旁路。
const ALLOWED_HTTP_METHODS: &[&str] = &["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD"];

/// Guest 不得自行设置的 header。
///
/// 这些 header 要么由 Host 注入凭证（`authorization` / `api-key` 系列），要么会改变
/// 请求的目的地或传输语义（`host`）。允许 Guest 覆盖等于让它绕过 allowlist 或伪造身份。
const RESERVED_REQUEST_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "api-key",
    "x-api-key",
    "x-goog-api-key",
    "cookie",
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "upgrade",
];

/// 不回传给 Guest 的响应 header。
///
/// `set-cookie` 可能携带会话凭证，没有任何插件用例需要它。
const REDACTED_RESPONSE_HEADERS: &[&str] = &["set-cookie", "set-cookie2"];

/// Guest 提交的受限 HTTP 请求。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GuestHttpRequest {
    /// HTTP 方法，大小写不敏感。
    pub(crate) method: String,
    /// 相对于 Host 注入 base URL 的路径，可带查询字符串。
    pub(crate) path: String,
    /// 非敏感请求 header。
    #[serde(default)]
    pub(crate) headers: HashMap<String, String>,
    /// 可选 UTF-8 请求正文。
    #[serde(default)]
    pub(crate) body: Option<String>,
}

impl CapabilityState {
    /// 克隆 HTTP 调用所需的 manifest allowlist 与应用绑定，避免跨 `await` 借用 store。
    pub(crate) fn http_context(&self) -> (Option<HttpCapabilitySection>, Option<HttpHostServices>) {
        (self.permissions.http.clone(), self.http.clone())
    }

    /// 校验并执行一次受限 HTTP 请求。
    ///
    /// `allowlist` 来自 manifest，`binding` 来自应用注入。两者缺一都不放行：没有 manifest
    /// 声明说明插件未申请该能力，没有绑定说明应用未配置端点。
    pub(crate) async fn http_request_with(
        allowlist: Option<HttpCapabilitySection>,
        binding: Option<HttpHostServices>,
        request_json: &str,
    ) -> Result<Value> {
        let allowlist =
            allowlist.ok_or_else(|| capability_denied("插件 manifest 未授权 http 能力"))?;
        if request_json.len() > MAX_HTTP_REQUEST_BYTES {
            return Err(anyhow!("HTTP 请求超过 {MAX_HTTP_REQUEST_BYTES} 字节限制"));
        }
        let request: GuestHttpRequest = parse_request(request_json)?;
        let binding = binding.ok_or_else(|| anyhow!("应用未注入 HTTP 端点配置"))?;

        let method = normalize_method(&request.method)?;
        let target = binding.resolve(&request.path)?;
        // Host 自己注入的 base URL 也必须落在 manifest 声明的 allowlist 内：
        // 声明与实际目的地不一致时，用户看到的授权边界就是假的。
        let host = target
            .host_str()
            .ok_or_else(|| anyhow!("HTTP 目标缺少主机名"))?;
        if !allowlist.allows_host(host, target.port()) {
            return Err(capability_denied(format!(
                "HTTP 目标主机 `{host}` 不在 manifest allow_hosts 内"
            )));
        }
        let headers = sanitize_request_headers(&request.headers)?;
        let body = request.body.unwrap_or_default();
        if body.len() > binding.max_request_bytes {
            return Err(anyhow!(
                "HTTP 请求正文超过 {} 字节限制",
                binding.max_request_bytes
            ));
        }
        binding
            .reserve(request_json.len())
            .map_err(capability_denied)?;

        binding.execute(method, target, headers, body).await
    }
}

/// 规范化并校验 HTTP 方法。
fn normalize_method(method: &str) -> Result<String> {
    let normalized = method.trim().to_ascii_uppercase();
    if !ALLOWED_HTTP_METHODS.contains(&normalized.as_str()) {
        return Err(anyhow!("不支持的 HTTP 方法：`{method}`"));
    }
    Ok(normalized)
}

/// 校验 Guest 提交的 header，拒绝保留名与非法字符。
fn sanitize_request_headers(headers: &HashMap<String, String>) -> Result<Vec<(String, String)>> {
    if headers.len() > MAX_HTTP_HEADERS {
        return Err(anyhow!("HTTP header 数量超过 {MAX_HTTP_HEADERS} 项限制"));
    }
    let mut sanitized = Vec::with_capacity(headers.len());
    for (name, value) in headers {
        let lowered = name.trim().to_ascii_lowercase();
        if lowered.is_empty() {
            return Err(anyhow!("HTTP header 名称不能为空"));
        }
        if RESERVED_REQUEST_HEADERS.contains(&lowered.as_str()) {
            return Err(capability_denied(format!(
                "HTTP header `{name}` 由宿主管理，插件不得设置"
            )));
        }
        if lowered.len() > MAX_HTTP_HEADER_BYTES || value.len() > MAX_HTTP_HEADER_BYTES {
            return Err(anyhow!("HTTP header `{name}` 超过 {MAX_HTTP_HEADER_BYTES} 字节限制"));
        }
        if !lowered
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        {
            return Err(anyhow!("HTTP header 名称含非法字符：`{name}`"));
        }
        // 控制字符会被用于拼接出额外的 header 行（header 注入）。
        if value
            .bytes()
            .any(|byte| byte < 0x20 && byte != b'\t' || byte == 0x7F)
        {
            return Err(anyhow!("HTTP header `{name}` 的值含控制字符"));
        }
        sanitized.push((lowered, value.clone()));
    }
    sanitized.sort();
    Ok(sanitized)
}

/// 校验 Guest 提交的相对路径，并拼接到可信 base URL 上。
///
/// 拒绝绝对 URL、协议相对地址与父目录穿越：三者都能让请求脱离 base URL 指向的前缀。
pub(crate) fn join_base_url(base: &url::Url, path: &str) -> Result<url::Url> {
    let path = path.trim();
    if path.len() > MAX_HTTP_PATH_BYTES {
        return Err(anyhow!("HTTP 路径超过 {MAX_HTTP_PATH_BYTES} 字节限制"));
    }
    if path.contains("://") || path.starts_with("//") {
        return Err(anyhow!("HTTP 路径必须是相对路径，不能指定主机：`{path}`"));
    }
    if path
        .bytes()
        .any(|byte| byte < 0x20 || byte == 0x7F || byte == b' ')
    {
        return Err(anyhow!("HTTP 路径含非法字符：`{path}`"));
    }
    let relative = path.strip_prefix('/').unwrap_or(path);
    let joined = base
        .join(relative)
        .with_context(|| format!("HTTP 路径无法解析：`{path}`"))?;
    // `join` 已经规范化了 `..`；用前缀比较确认结果没有跳出 base URL 的路径范围。
    if joined.origin() != base.origin() || !joined.path().starts_with(base.path()) {
        return Err(anyhow!("HTTP 路径超出宿主配置的 base URL 范围：`{path}`"));
    }
    Ok(joined)
}

/// 过滤响应 header，去掉可能携带凭证的条目。
pub(crate) fn sanitize_response_headers(
    headers: &reqwest::header::HeaderMap,
) -> HashMap<String, String> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            let name = name.as_str().to_ascii_lowercase();
            if REDACTED_RESPONSE_HEADERS.contains(&name.as_str()) {
                return None;
            }
            Some((name, value.to_str().ok()?.to_string()))
        })
        .collect()
}

/// 把宿主响应编码成 Guest 可解析的稳定结构。
pub(crate) fn encode_http_response(
    status: u16,
    headers: HashMap<String, String>,
    body: String,
) -> Value {
    json!({
        "status": status,
        "headers": headers,
        "body": body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造测试用 base URL。
    fn base() -> url::Url {
        url::Url::parse("https://api.example.com/v1/").expect("测试 base URL")
    }

    /// 相对路径必须拼接到 base URL 之后。
    #[test]
    fn relative_path_is_joined_onto_base() {
        let joined = join_base_url(&base(), "embeddings").expect("应拼接成功");
        assert_eq!(joined.as_str(), "https://api.example.com/v1/embeddings");
    }

    /// 前导斜杠不应把路径重置到根，否则 base URL 的路径前缀形同虚设。
    #[test]
    fn leading_slash_does_not_escape_base_path() {
        let joined = join_base_url(&base(), "/embeddings").expect("应拼接成功");
        assert_eq!(joined.as_str(), "https://api.example.com/v1/embeddings");
    }

    /// 查询字符串必须被保留。
    #[test]
    fn query_string_is_preserved() {
        let joined = join_base_url(&base(), "models?limit=10").expect("应拼接成功");
        assert_eq!(joined.query(), Some("limit=10"));
    }

    /// 绝对 URL 必须被拒绝——它能让请求指向任意主机。
    #[test]
    fn absolute_url_is_rejected() {
        assert!(join_base_url(&base(), "https://evil.example.com/steal").is_err());
        assert!(join_base_url(&base(), "//evil.example.com/steal").is_err());
    }

    /// 父目录穿越必须被拒绝。
    #[test]
    fn parent_traversal_is_rejected() {
        assert!(join_base_url(&base(), "../admin").is_err());
        assert!(join_base_url(&base(), "a/../../admin").is_err());
    }

    /// 路径中的控制字符与空格必须被拒绝。
    #[test]
    fn control_characters_in_path_are_rejected() {
        assert!(join_base_url(&base(), "embed\r\nX-Injected: 1").is_err());
        assert!(join_base_url(&base(), "embed dings").is_err());
    }

    /// 方法白名单之外的动词必须被拒绝。
    #[test]
    fn unsupported_methods_are_rejected() {
        assert_eq!(normalize_method("post").expect("应接受"), "POST");
        assert!(normalize_method("CONNECT").is_err());
        assert!(normalize_method("TRACE").is_err());
    }

    /// 凭证类 header 不得由 Guest 设置。
    #[test]
    fn reserved_headers_are_rejected() {
        for name in ["Authorization", "x-api-key", "Host", "Cookie"] {
            let headers = HashMap::from([(name.to_string(), "value".to_string())]);
            assert!(
                sanitize_request_headers(&headers).is_err(),
                "{name} 应被拒绝"
            );
        }
    }

    /// header 值里的控制字符必须被拒绝，避免 header 注入。
    #[test]
    fn header_injection_is_rejected() {
        let headers = HashMap::from([(
            "x-trace".to_string(),
            "a\r\nAuthorization: Bearer leak".to_string(),
        )]);
        assert!(sanitize_request_headers(&headers).is_err());
    }

    /// 正常 header 必须通过并被规范化为小写。
    #[test]
    fn ordinary_headers_are_normalized() {
        let headers = HashMap::from([("Content-Type".to_string(), "application/json".to_string())]);
        let sanitized = sanitize_request_headers(&headers).expect("应通过");
        assert_eq!(
            sanitized,
            vec![("content-type".to_string(), "application/json".to_string())]
        );
    }

    /// 响应中的 Set-Cookie 必须被剔除。
    #[test]
    fn response_cookies_are_redacted() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("set-cookie", "session=secret".parse().expect("测试 header"));
        headers.insert("content-type", "application/json".parse().expect("测试 header"));
        let sanitized = sanitize_response_headers(&headers);

        assert!(!sanitized.contains_key("set-cookie"));
        assert_eq!(
            sanitized.get("content-type").map(String::as_str),
            Some("application/json")
        );
    }
}
